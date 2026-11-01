//! Releasing orders. Allocation is by position: only available stock, in an open bin of a usable
//! zone, unexpired, the warehouse's rule deciding which goes first. What cannot be allocated is
//! written down as a shortfall, never quietly dropped, and it stays on the order.

use crate::posting;
use crate::tasks::{self, NewTask};
use async_trait::async_trait;
use chrono::NaiveDate;
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
pub async fn allocate(tx: &mut Tx, tenant: Uuid, warehouse: Uuid, item: Uuid, owner: Option<Uuid>, qty: Decimal, rule: &str, today: NaiveDate, reserved_bins: &[Uuid]) -> Result<Vec<Take>> {
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
    let orders: Vec<Uuid> = doc.get("orders").and_then(Value::as_array).map(|o| o.iter().filter_map(|r| r["sales_order"].as_str()?.parse().ok()).collect()).unwrap_or_default();
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
        tasks::create(k, ctx, tx, NewTask { warehouse, kind: "Pick", item, qty, lot, serial, unit, from_bin: bin, to_bin: Some(outbound), wave: Some(wave), ..Default::default() }).await?;
        made += 1;
    }
    let values = json!({"status": "Released", "shortfall": shortfall});
    k.set_values_tx(&sys(ctx), tx, "PickWave", wave, values.as_object().cloned().unwrap_or_default()).await?;
    Ok(json!({"tasks": made, "shortfall": shortfall}))
}

/// A wave is picked when none of its tasks is still open.
pub async fn progress(k: &Kernel, ctx: &Ctx, tx: &mut Tx, wave: Uuid) -> Result<()> {
    let open: i64 = sqlx::query_scalar("select count(*) from e_warehouse_task where tenant_id = $1 and wave = $2 and status not in ('Done', 'Short', 'Cancelled')").bind(ctx.tenant).bind(wave).fetch_one(&mut **tx).await?;
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
                NewTask { warehouse: r.warehouse, kind: "Replenish", item: r.item, qty: take.qty, lot: take.lot, serial: take.serial, unit: take.unit, from_bin: take.bin, to_bin: Some(r.pick_bin), priority: 5, ..Default::default() },
            )
            .await?;
            made += 1;
        }
    }
    Ok(made)
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
