//! Releasing orders. Allocation is by position: only available stock, in an open bin of a usable
//! zone, unexpired, the warehouse's rule deciding which goes first. What cannot be allocated is
//! written down as a shortfall, never quietly dropped, and it stays on the order.

use crate::posting;
use crate::tasks::{self, NewTask};
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_domain::util::d;
use ferrum_domain::util::{elevated, id, sys};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(sqlx::FromRow, Debug)]
struct Open {
    bin_id: Uuid,
    lot: String,
    serial: String,
    unit_id: Option<Uuid>,
    free: Decimal,
}

/// One allocation.
#[derive(Debug, Clone, PartialEq)]
pub struct Take {
    pub bin: Uuid,
    pub lot: String,
    pub serial: String,
    pub unit: Option<Uuid>,
    pub qty: Decimal,
}

/// What can be promised of `qty`, in the order the rule says. Positions stay locked to the end of
/// the transaction, so two waves cannot promise the same stock. Stock for a person's task keeps
/// its bin's picking sequence inside the rule.
pub async fn allocate(
    tx: &mut Tx,
    tenant: Uuid,
    warehouse: Uuid,
    item: Uuid,
    owner: Option<Uuid>,
    qty: Decimal,
    rule: &str,
    today: NaiveDate,
    reserved_bins: &[Uuid],
) -> Result<Vec<Take>> {
    let order = if rule == "FIFO" { "p.received_on, p.expiry nulls last" } else { "p.expiry nulls last, p.received_on" };
    let rows: Vec<Open> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select p.bin_id, p.lot, p.serial, p.unit_id, p.qty - p.allocated as free from wms_position p \
         join e_storage_bin b on b.tenant_id = p.tenant_id and b.id = p.bin_id \
         join e_storage_zone z on z.tenant_id = b.tenant_id and z.id = b.zone \
         where p.tenant_id = $1 and p.warehouse_id = $2 and p.item_id = $3 and p.owner_id is not distinct from $4 and p.status = 'Available' and p.qty > p.allocated \
         and coalesce(b.status, 'Open') = 'Open' and b.frozen_by is null and z.available and z.kind in ('Storage', 'Picking') \
         and (p.expiry is null or p.expiry >= $5) and not (p.bin_id = any($6)) \
         order by {order}, (z.kind = 'Picking') desc, b.pick_sequence, b.bin_code for update of p"
    )))
    .bind(tenant)
    .bind(warehouse)
    .bind(item)
    .bind(owner)
    .bind(today)
    .bind(reserved_bins)
    .fetch_all(&mut **tx)
    .await?;
    let (mut left, mut out) = (qty, Vec::new());
    for r in rows {
        if left <= Decimal::ZERO {
            break;
        }
        let part = left.min(r.free);
        out.push(Take { bin: r.bin_id, lot: r.lot, serial: r.serial, unit: r.unit_id, qty: part });
        left -= part;
    }
    Ok(out)
}

/// Releases a wave: allocates its orders' open lines, opens pick tasks, and prepares a shipment
/// for each order. Under `Batch`, what one bin gives to several orders is one trip.
async fn release(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc) -> Result<Value> {
    let wave = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let warehouse = id(doc, "warehouse").ok_or_else(|| Error::field("warehouse", "is required"))?;
    let w = posting::must_manage(tx, ctx.tenant, warehouse).await?;
    let outbound = w.outbound_bin.ok_or_else(|| Error::invalid("this warehouse has no outbound bin"))?;
    let batch = doc.get("strategy").and_then(Value::as_str) == Some("Batch");
    let orders: Vec<Uuid> =
        doc.get("orders").and_then(Value::as_array).map(|o| o.iter().filter_map(|r| r["sales_order"].as_str()?.parse().ok()).collect()).unwrap_or_default();
    let today = ctx.today();
    let rule = w.allocation_rule.clone().unwrap_or_else(|| "FEFO".into());
    let (mut shortfall, mut made) = (Vec::new(), 0u32);
    let mut trips: BTreeMap<(Uuid, Uuid, String, String, Option<Uuid>), Decimal> = BTreeMap::new();
    for order in &orders {
        let lines: Vec<(Uuid, Uuid, Decimal, String)> = sqlx::query_as(
            "select l.id, l.item, (greatest(l.qty - coalesce(l.delivered_qty, 0), 0) * coalesce(nullif(l.conversion_factor, 0), 1))::numeric(28,9), i.item_code from e_sales_order__items l \
             join e_item i on i.tenant_id = l.tenant_id and i.id = l.item \
             join e_sales_order o on o.tenant_id = l.tenant_id and o.id = l.parent_id \
             where l.tenant_id = $1 and l.parent_id = $2 and o.docstatus = 1 and i.is_stock_item order by l.idx",
        )
        .bind(ctx.tenant)
        .bind(order)
        .fetch_all(&mut **tx)
        .await?;
        if lines.is_empty() {
            return Err(Error::invalid("every order in a wave must be submitted, with stock to deliver"));
        }
        let mut items = Vec::new();
        for (line, item, open, code) in lines {
            if open <= Decimal::ZERO {
                continue;
            }
            let takes = allocate(tx, ctx.tenant, warehouse, item, None, open, &rule, today, &[outbound]).await?;
            let got: Decimal = takes.iter().map(|t| t.qty).sum();
            if got < open {
                shortfall.push(json!({"order": order, "item": code, "wanted": open, "allocated": got}));
            }
            for take in takes {
                if batch {
                    *trips.entry((take.bin, item, take.lot.clone(), take.serial.clone(), take.unit)).or_default() += take.qty;
                } else {
                    tasks::create(
                        k,
                        ctx,
                        tx,
                        NewTask {
                            warehouse,
                            kind: "Pick",
                            item,
                            qty: take.qty,
                            lot: take.lot.clone(),
                            serial: take.serial.clone(),
                            unit: take.unit,
                            from_bin: take.bin,
                            to_bin: Some(outbound),
                            wave: Some(wave),
                            sales_order: Some(*order),
                            so_line: Some(line.to_string()),
                            ..Default::default()
                        },
                    )
                    .await?;
                    made += 1;
                }
                items.push(json!({"item": item, "qty": take.qty, "lot": take.lot, "serial": take.serial, "sales_order": order, "so_line": line.to_string()}));
            }
        }
        if !items.is_empty() {
            crate::shipping::prepare(k, ctx, tx, doc, *order, warehouse, items).await?;
        }
    }
    for ((bin, item, lot, serial, unit), qty) in trips {
        tasks::create(
            k,
            ctx,
            tx,
            NewTask { warehouse, kind: "Pick", item, qty, lot, serial, unit, from_bin: bin, to_bin: Some(outbound), wave: Some(wave), ..Default::default() },
        )
        .await?;
        made += 1;
    }
    let values = json!({"status": "Released", "shortfall": shortfall});
    k.set_values_tx(&sys(ctx), tx, "PickWave", wave, values.as_object().cloned().unwrap_or_default()).await?;
    Ok(json!({"tasks": made, "shortfall": shortfall}))
}

/// A wave is picked when none of its tasks is still open.
pub async fn progress(k: &Kernel, ctx: &Ctx, tx: &mut Tx, wave: Uuid) -> Result<()> {
    let open: i64 =
        sqlx::query_scalar("select count(*) from e_warehouse_task where tenant_id = $1 and wave = $2 and status not in ('Done', 'Short', 'Cancelled')")
            .bind(ctx.tenant)
            .bind(wave)
            .fetch_one(&mut **tx)
            .await?;
    if open == 0 {
        let mut v = Doc::new();
        v.insert("status".into(), json!("Picked"));
        k.set_values_tx(&sys(ctx), tx, "PickWave", wave, v).await?;
    }
    Ok(())
}

pub struct WaveHook;

#[async_trait]
impl DocHook for WaveHook {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "release" {
            return Ok(None);
        }
        let ctx = elevated(hc.ctx);
        Ok(Some(release(hc.kernel, &ctx, hc.tx, doc).await?))
    }
}

/// Opens replenishment for rules whose pick bin has fallen below its minimum, unless one is
/// already on its way. Its source goes through the same allocation as any pick.
pub async fn replenish(k: &Kernel, ctx: &Ctx, tx: &mut Tx) -> Result<u32> {
    #[derive(sqlx::FromRow)]
    struct Rule {
        warehouse: Uuid,
        item: Uuid,
        pick_bin: Uuid,
        min_qty: Decimal,
        max_qty: Decimal,
        have: Decimal,
    }
    let rules: Vec<Rule> = sqlx::query_as(
        "select r.warehouse, r.item, r.pick_bin, r.min_qty, r.max_qty, \
           coalesce((select sum(p.qty) from wms_position p where p.tenant_id = r.tenant_id and p.bin_id = r.pick_bin and p.item_id = r.item and p.status = 'Available'), 0) as have \
         from e_replenishment_rule r where r.tenant_id = $1 and r.active \
         and not exists (select 1 from e_warehouse_task t where t.tenant_id = r.tenant_id and t.kind = 'Replenish' and t.to_bin = r.pick_bin and t.item = r.item \
                         and t.status not in ('Done', 'Short', 'Cancelled'))",
    )
    .bind(ctx.tenant)
    .fetch_all(&mut **tx)
    .await?;
    let mut made = 0;
    for r in rules {
        if r.have >= r.min_qty {
            continue;
        }
        let Some(w) = posting::managed(tx, ctx.tenant, r.warehouse).await? else { continue };
        let rule = w.allocation_rule.unwrap_or_else(|| "FEFO".into());
        for take in allocate(tx, ctx.tenant, r.warehouse, r.item, None, r.max_qty - r.have, &rule, ctx.today(), &[r.pick_bin]).await? {
            tasks::create(
                k,
                ctx,
                tx,
                NewTask {
                    warehouse: r.warehouse,
                    kind: "Replenish",
                    item: r.item,
                    qty: take.qty,
                    lot: take.lot,
                    serial: take.serial,
                    unit: take.unit,
                    from_bin: take.bin,
                    to_bin: Some(r.pick_bin),
                    priority: 5,
                    ..Default::default()
                },
            )
            .await?;
            made += 1;
        }
    }
    Ok(made)
}

/// What is at the line for anyone to use, today: not on hold, and not promised to a task.
async fn at_line(tx: &mut Tx, tenant: Uuid, line: Uuid, item: Uuid) -> Result<Decimal> {
    Ok(sqlx::query_scalar("select coalesce(sum(qty - allocated), 0) from wms_position where tenant_id = $1 and bin_id = $2 and item_id = $3 and owner_id is null and status = 'Available'")
        .bind(tenant)
        .bind(line)
        .bind(item)
        .fetch_one(&mut **tx)
        .await?)
}

/// Brings what a released run needs to the line, through the same allocation as any pick. What is at
/// the line counts only if no other open run has claimed it, because stock standing there is not
/// promised to anyone. The run's id marks it done, because this arrives at least once. The run hears
/// how it stands: everything there, on its way, or what cannot be brought, which otherwise would show
/// only when the material ran out. A material outside a managed line is left for a person to confirm.
pub async fn supply(k: &Kernel, ctx: &Ctx, tx: &mut Tx, run: Uuid, materials: &[Value]) -> Result<()> {
    if sqlx::query("insert into wms_scan (tenant_id, id) values ($1, $2) on conflict do nothing")
        .bind(ctx.tenant)
        .bind(run)
        .execute(&mut **tx)
        .await?
        .rows_affected()
        == 0
    {
        return Ok(());
    }
    let (mut short, mut ours, mut elsewhere) = (Vec::new(), false, false);
    for m in materials.iter().filter_map(Value::as_object) {
        let (Some(item), Some(warehouse)) = (id(m, "item"), id(m, "warehouse")) else { continue };
        let line = posting::managed(tx, ctx.tenant, warehouse).await?;
        let Some((w, line)) = line.and_then(|w| w.production_bin.map(|bin| (w, bin))) else {
            // On record with nothing at a line, so that the run is never told this much is there.
            claim(tx, ctx, run, warehouse, item, d(m, "qty"), Decimal::ZERO).await?;
            elsewhere = true;
            continue;
        };
        ours = true;
        // One run at a time at a warehouse's line, or two could each find the same stock free.
        sqlx::query("select 1 from e_managed_warehouse where tenant_id = $1 and warehouse = $2 for update")
            .bind(ctx.tenant)
            .bind(warehouse)
            .execute(&mut **tx)
            .await?;
        let there = at_line(tx, ctx.tenant, line, item).await?;
        // ponytail: a claim shrinks only when its run closes, so while a run consumes, others see less
        // free than there is and bring more. Leftovers at the line are the cost, not a false start.
        let claimed: Decimal = sqlx::query_scalar(
            "select coalesce(sum(at_line), 0) from wms_supply where tenant_id = $1 and warehouse_id = $2 and item_id = $3 and closed_at is null",
        )
        .bind(ctx.tenant)
        .bind(warehouse)
        .bind(item)
        .fetch_one(&mut **tx)
        .await?;
        let wanted = d(m, "qty");
        let free = wanted.min((there - claimed).max(Decimal::ZERO));
        claim(tx, ctx, run, warehouse, item, wanted, free).await?;
        let need = wanted - free;
        if need <= Decimal::ZERO {
            continue;
        }
        let rule = w.allocation_rule.unwrap_or_else(|| "FEFO".into());
        let takes = allocate(tx, ctx.tenant, warehouse, item, None, need, &rule, ctx.today(), &[line]).await?;
        let found: Decimal = takes.iter().map(|t| t.qty).sum();
        for take in takes {
            let task = NewTask {
                warehouse,
                kind: "Move",
                item,
                qty: take.qty,
                lot: take.lot,
                serial: take.serial,
                unit: take.unit,
                status: "Available".into(),
                from_bin: take.bin,
                to_bin: Some(line),
                run: Some(run),
                ..Default::default()
            };
            tasks::create(k, ctx, tx, task).await?;
        }
        if found < need {
            let goods = crate::slotting::goods(tx, ctx.tenant, item).await?;
            short.push(format!("{}: {} of {}", goods.item_code, found.normalize(), need.normalize()));
        }
    }
    if !short.is_empty() {
        return tell(tx, ctx, "wms.supply.short", run, json!({"run": run, "short": short.join("; ")})).await;
    }
    if !ours {
        return Ok(());
    }
    if elsewhere {
        return tell(tx, ctx, "wms.supply.reserved", run, json!({"run": run})).await;
    }
    supply_progress(tx, ctx, run).await
}

/// What a run wants of an item, and how much of what stands at the line is that run's.
async fn claim(tx: &mut Tx, ctx: &Ctx, run: Uuid, warehouse: Uuid, item: Uuid, need: Decimal, at_line: Decimal) -> Result<()> {
    sqlx::query(
        "insert into wms_supply (tenant_id, run_id, warehouse_id, item_id, need, at_line) values ($1, $2, $3, $4, $5, $6) \
         on conflict (tenant_id, run_id, warehouse_id, item_id) do update set need = wms_supply.need + excluded.need, at_line = wms_supply.at_line + excluded.at_line",
    )
    .bind(ctx.tenant)
    .bind(run)
    .bind(warehouse)
    .bind(item)
    .bind(need)
    .bind(at_line)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn tell(tx: &mut Tx, ctx: &Ctx, topic: &str, run: Uuid, payload: Value) -> Result<()> {
    ferrum_kernel::outbox::enqueue(tx, ctx.tenant, topic, Some("ProductionRun"), Some(run), payload).await
}

/// Stock of a run's reached the line.
pub async fn arrived_at_line(tx: &mut Tx, ctx: &Ctx, run: Uuid, warehouse: Uuid, item: Uuid, qty: Decimal) -> Result<()> {
    sqlx::query("update wms_supply set at_line = at_line + $5 where tenant_id = $1 and run_id = $2 and warehouse_id = $3 and item_id = $4")
        .bind(ctx.tenant)
        .bind(run)
        .bind(warehouse)
        .bind(item)
        .bind(qty)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Tells a run how its material stands: all of it usable at the line, on its way, or something in the
/// way. Said again whenever that may have changed, and hearing it twice changes nothing.
pub async fn supply_progress(tx: &mut Tx, ctx: &Ctx, run: Uuid) -> Result<()> {
    let rows: Vec<(Uuid, Uuid, Decimal, Decimal, Option<Uuid>)> = sqlx::query_as(
        "select s.warehouse_id, s.item_id, s.need, s.at_line, m.production_bin from wms_supply s \
         left join e_managed_warehouse m on m.tenant_id = s.tenant_id and m.warehouse = s.warehouse_id where s.tenant_id = $1 and s.run_id = $2 and s.closed_at is null",
    )
    .bind(ctx.tenant)
    .bind(run)
    .fetch_all(&mut **tx)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let tasks: Vec<(Option<String>, bool)> = sqlx::query_as(
        "select t.status, exists (select 1 from e_quality_hold h where h.tenant_id = t.tenant_id and h.item = t.item and h.status = 'Active' \
           and (coalesce(h.lot, '') = '' or h.lot = coalesce(t.lot, ''))) from e_warehouse_task t \
         where t.tenant_id = $1 and t.production_run = $2 and t.status not in ('Done', 'Short', 'Cancelled')",
    )
    .bind(ctx.tenant)
    .bind(run.to_string())
    .fetch_all(&mut **tx)
    .await?;
    if tasks.iter().any(|(status, _)| status.as_deref() == Some("Exception")) {
        return tell(tx, ctx, "wms.supply.short", run, json!({"run": run, "short": "stock on its way to the line went missing"})).await;
    }
    if tasks.iter().any(|(_, held)| *held) {
        return tell(tx, ctx, "wms.supply.short", run, json!({"run": run, "short": "stock on its way to the line went on quality hold"})).await;
    }
    let (mut missing, mut by_hand, mut unusable) = (false, false, Vec::new());
    for (_, item, need, have, line) in &rows {
        // Nothing here brings that one, or can see it arrive: a person says when it is there.
        let Some(line) = line else {
            by_hand = true;
            continue;
        };
        if have < need {
            missing = true;
            continue;
        }
        let usable = at_line(tx, ctx.tenant, *line, *item).await?;
        if usable < *need {
            let goods = crate::slotting::goods(tx, ctx.tenant, *item).await?;
            unusable.push(format!("{}: {} of {} usable at the line", goods.item_code, usable.normalize(), need.normalize()));
        }
    }
    if !unusable.is_empty() {
        return tell(tx, ctx, "wms.supply.short", run, json!({"run": run, "short": unusable.join("; ")})).await;
    }
    match (missing, tasks.is_empty()) {
        (true, true) => tell(tx, ctx, "wms.supply.short", run, json!({"run": run, "short": "some of it was not brought to the line"})).await,
        (false, _) if !by_hand => tell(tx, ctx, "wms.supply.ready", run, json!({"run": run})).await,
        _ => tell(tx, ctx, "wms.supply.reserved", run, json!({"run": run})).await,
    }
}

/// Runs waiting for this item at a line, which a hold or its release may have changed.
pub async fn recheck_supply(tx: &mut Tx, ctx: &Ctx, item: Uuid) -> Result<()> {
    let runs: Vec<Uuid> = sqlx::query_scalar("select distinct run_id from wms_supply where tenant_id = $1 and item_id = $2 and closed_at is null")
        .bind(ctx.tenant)
        .bind(item)
        .fetch_all(&mut **tx)
        .await?;
    for run in runs {
        supply_progress(tx, ctx, run).await?;
    }
    Ok(())
}

/// A run ended: what it claimed at the line is free again.
pub async fn close_supply(tx: &mut Tx, tenant: Uuid, run: Uuid) -> Result<()> {
    sqlx::query("update wms_supply set closed_at = now() where tenant_id = $1 and run_id = $2 and closed_at is null")
        .bind(tenant)
        .bind(run)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Stock available to promise, by item, for a warehouse.
pub async fn available(tx: &mut Tx, tenant: Uuid, warehouse: Uuid, item: Uuid) -> Result<Decimal> {
    Ok(sqlx::query_scalar("select coalesce(sum(qty - allocated), 0) from wms_position where tenant_id = $1 and warehouse_id = $2 and item_id = $3 and owner_id is null and status = 'Available'")
        .bind(tenant)
        .bind(warehouse)
        .bind(item)
        .fetch_one(&mut **tx)
        .await?)
}
