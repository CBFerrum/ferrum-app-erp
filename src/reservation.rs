//! Stock reserved for submitted sales orders: per item and warehouse, what their open lines have
//! yet to deliver (`stock_balance.reserved_qty`). It is recomputed from the orders whenever one
//! changes — never added to or subtracted from — so it cannot drift, and each balance is locked
//! before it is summed (in the same item-then-warehouse order as stock postings), so concurrent
//! orders for the same item never lose each other's quantities.

use ferrum_kernel::{Result, Tx};
use rust_decimal::Decimal;
use uuid::Uuid;

/// Refreshes the reservations of every stock item and warehouse on sales order `order`.
pub async fn refresh_for_order(tx: &mut Tx, tenant: Uuid, order: Uuid) -> Result<()> {
    let mut pairs: Vec<(Uuid, Uuid, Uuid)> = sqlx::query_as(
        "select distinct l.item, coalesce(l.warehouse, o.set_warehouse), o.company from e_sales_order o \
         join e_sales_order__items l on l.tenant_id = o.tenant_id and l.parent_id = o.id \
         join e_item i on i.tenant_id = l.tenant_id and i.id = l.item \
         where o.tenant_id = $1 and o.id = $2 and i.is_stock_item and coalesce(l.warehouse, o.set_warehouse) is not null",
    )
    .bind(tenant)
    .bind(order)
    .fetch_all(&mut **tx)
    .await?;
    pairs.sort();
    pairs.dedup_by_key(|p| (p.0, p.1));
    for (item, warehouse, company) in pairs {
        refresh(tx, tenant, item, warehouse, company).await?;
    }
    Ok(())
}

/// Refreshes the reservations of `pairs` (item, warehouse) of `company`: lines an order no longer has.
pub async fn refresh_pairs(tx: &mut Tx, tenant: Uuid, company: Uuid, mut pairs: Vec<(Uuid, Uuid)>) -> Result<()> {
    pairs.sort();
    pairs.dedup();
    for (item, warehouse) in pairs {
        refresh(tx, tenant, item, warehouse, company).await?;
    }
    Ok(())
}

async fn refresh(tx: &mut Tx, tenant: Uuid, item: Uuid, warehouse: Uuid, company: Uuid) -> Result<()> {
    // The balance must exist to be locked; a concurrent first insert waits for this one.
    sqlx::query(
        "insert into stock_balance (tenant_id, item_id, warehouse_id, company_id) values ($1, $2, $3, $4) \
         on conflict (tenant_id, item_id, warehouse_id) do nothing",
    )
    .bind(tenant)
    .bind(item)
    .bind(warehouse)
    .bind(company)
    .execute(&mut **tx)
    .await?;
    sqlx::query("select 1 from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = $3 for update")
        .bind(tenant)
        .bind(item)
        .bind(warehouse)
        .execute(&mut **tx)
        .await?;
    // A new statement after the lock: it sees every order committed while this one waited.
    let reserved: Decimal = sqlx::query_scalar(
        "select coalesce(sum(greatest(l.qty - coalesce(l.delivered_qty, 0), 0) * coalesce(nullif(l.conversion_factor, 0), 1)), 0) from e_sales_order__items l \
         join e_sales_order o on o.tenant_id = l.tenant_id and o.id = l.parent_id \
         where l.tenant_id = $1 and l.item = $2 and coalesce(l.warehouse, o.set_warehouse) = $3 \
         and o.docstatus = 1 and o.status not in ('Closed', 'Completed')",
    )
    .bind(tenant)
    .bind(item)
    .bind(warehouse)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query("update stock_balance set reserved_qty = $4, updated_at = now() where tenant_id = $1 and item_id = $2 and warehouse_id = $3")
        .bind(tenant)
        .bind(item)
        .bind(warehouse)
        .bind(reserved)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

// ───────────────────────────── holds and promises ─────────────────────────────
//
// Under the Hold policy (a company setting, which an item group may override) an order's line holds
// stock: `stock_reserved_qty` on the line, in stock units, as much as was free when it was reserved,
// the rest back-ordered. What a line holds is the least of that and what it has yet to deliver, so a
// delivery against it uses it up; a cancelled, closed or completed order holds nothing, nor does a
// reservation past its `reserved_until`. Reserving locks the stock balance first (as every stock
// posting does), so of two orders reserving the last of an item one gets it; and no issue of stock
// (a delivery, a sale that updates stock, a stock entry) may take what other orders hold.

use chrono::NaiveDate;
use ferrum_kernel::{Ctx, Doc, Error, Kernel};
use serde_json::{Value, json};

/// A company's reservation policy for an item.
pub struct Policy {
    pub hold: bool,
    pub on_submit: bool,
    pub days: Option<i64>,
}

pub async fn policy(tx: &mut Tx, tenant: Uuid, company: Uuid, item: Uuid) -> Result<Policy> {
    let row: Option<(Option<String>, Option<String>, Option<i64>, Option<String>)> = sqlx::query_as(
        "select c.reservation_policy, c.reserve_on, c.reservation_days, g.reservation_policy from e_company c \
         join e_item i on i.tenant_id = c.tenant_id and i.id = $3 left join e_item_group g on g.tenant_id = i.tenant_id and g.id = i.item_group \
         where c.tenant_id = $1 and c.id = $2",
    )
    .bind(tenant)
    .bind(company)
    .bind(item)
    .fetch_optional(&mut **tx)
    .await?;
    let Some((co, on, days, group)) = row else { return Ok(Policy { hold: false, on_submit: true, days: None }) };
    let chosen = group.filter(|g| !g.is_empty()).or(co);
    Ok(Policy { hold: chosen.as_deref() == Some("Hold"), on_submit: on.as_deref() != Some("On Demand"), days: days.filter(|d| *d > 0) })
}

/// What open orders hold of an item in a warehouse, in stock units, but the line `except`.
pub async fn held(tx: &mut Tx, tenant: Uuid, item: Uuid, warehouse: Uuid, except: Option<Uuid>) -> Result<Decimal> {
    Ok(sqlx::query_scalar(
        "select coalesce(sum(least(l.stock_reserved_qty, greatest(l.qty - coalesce(l.delivered_qty, 0), 0) * coalesce(nullif(l.conversion_factor, 0), 1))), 0) \
         from e_sales_order__items l join e_sales_order o on o.tenant_id = l.tenant_id and o.id = l.parent_id \
         where l.tenant_id = $1 and l.item = $2 and coalesce(l.warehouse, o.set_warehouse) = $3 and coalesce(l.stock_reserved_qty, 0) > 0 \
         and o.docstatus = 1 and o.status not in ('Closed', 'Completed') and (l.reserved_until is null or l.reserved_until >= current_date) \
         and l.id is distinct from $4",
    )
    .bind(tenant)
    .bind(item)
    .bind(warehouse)
    .bind(except)
    .fetch_one(&mut **tx)
    .await?)
}

/// Refuses an issue of `need` (stock units) from a locked balance of `on_hand` that would take what
/// other orders hold. `source_entity` and `source_line` say whose line issues it: a delivery or sale
/// against an order's line may take that line's own reservation.
pub async fn check_issue(
    tx: &mut Tx,
    ctx: &Ctx,
    source_entity: &str,
    source_line: Option<Uuid>,
    item: Uuid,
    warehouse: Uuid,
    on_hand: Decimal,
    need: Decimal,
) -> Result<Option<Decimal>> {
    if ctx
        .schema
        .entity("SalesOrder")
        .and_then(|e| e.children.get("items"))
        .and_then(|c| ctx.schema.entity(&c.entity))
        .is_none_or(|c| c.field("stock_reserved_qty").is_none())
    {
        return Ok(None);
    }
    let own: Option<Uuid> = match (source_entity, source_line) {
        ("DeliveryNote" | "SalesInvoice", Some(line)) => {
            let table = ctx.schema.entity(source_entity).and_then(|e| e.children.get("items")).map(|c| ferrum_kernel::ddl::q(&c.table));
            match table {
                Some(t) => sqlx::query_scalar::<_, Option<String>>(sqlx::AssertSqlSafe(format!("select so_line from {t} where tenant_id = $1 and id = $2")))
                    .bind(ctx.tenant)
                    .bind(line)
                    .fetch_optional(&mut **tx)
                    .await?
                    .flatten()
                    .and_then(|l| l.parse().ok()),
                None => None,
            }
        }
        _ => None,
    };
    let others = held(tx, ctx.tenant, item, warehouse, own).await?;
    Ok((others > Decimal::ZERO && need > on_hand - others).then_some(others))
}

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap_or_default()
}

/// Reserves what is free for a submitted sales order's open lines under the Hold policy (`asked`:
/// pressed by someone, which also reserves under On Demand and renews time limits), each line under
/// its stock balance's lock. Lines of items counted only are left alone. Returns each line's
/// reservation and back-order.
pub async fn reserve_order(k: &Kernel, ctx: &Ctx, tx: &mut Tx, order: Uuid, asked: bool) -> Result<Value> {
    let sctx = crate::util::sys(ctx);
    let doc = k.get_tx(&sctx, tx, "SalesOrder", order, false).await?;
    let Some(company) = crate::util::id(&doc, "company") else { return Ok(json!({"lines": []})) };
    if ctx.schema.entity("SalesOrderItem").is_none_or(|e| e.field("stock_reserved_qty").is_none())
        || matches!(crate::util::s(&doc, "status"), Some("Closed" | "Completed"))
    {
        return Ok(json!({"lines": []}));
    }
    let today = ctx.today();
    let mut lines: Vec<(Uuid, Uuid, Uuid, Decimal, Decimal)> = Vec::new();
    for r in crate::util::rows(&doc, "items") {
        let (Some(line), Some(item)) = (crate::util::id(&r, "id"), crate::util::id(&r, "item")) else { continue };
        let Some(wh) = crate::util::id(&r, "warehouse").or(crate::util::id(&doc, "set_warehouse")) else { continue };
        let factor = Some(crate::util::d(&r, "conversion_factor")).filter(|f| !f.is_zero()).unwrap_or(Decimal::ONE);
        let open = ((crate::util::d(&r, "qty") - crate::util::d(&r, "delivered_qty")).max(Decimal::ZERO) * factor).round_dp(9);
        let stock: bool = sqlx::query_scalar("select coalesce(is_stock_item, false) from e_item where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(item)
            .fetch_one(&mut **tx)
            .await?;
        if stock {
            lines.push((item, wh, line, open, crate::util::d(&r, "stock_reserved_qty")));
        }
    }
    lines.sort();
    let mut out = Vec::new();
    for (item, wh, line, open, had) in lines {
        let p = policy(tx, ctx.tenant, company, item).await?;
        if !p.hold || !(asked || p.on_submit) {
            continue;
        }
        let bal = crate::stock::lock_balances(tx, ctx.tenant, company, &[(item, wh)]).await?;
        let on_hand = bal.get(&(item, wh)).map(|b| b.0).unwrap_or_default();
        let free = (on_hand - held(tx, ctx.tenant, item, wh, Some(line)).await?).max(Decimal::ZERO);
        let now = open.min(free);
        let mut v = Doc::new();
        v.insert("stock_reserved_qty".into(), crate::util::money(now));
        v.insert("backorder_qty".into(), crate::util::money(open - now));
        if asked || had.is_zero() {
            v.insert("reserved_until".into(), json!(p.days.map(|d| today + chrono::Duration::days(d))));
        }
        k.set_child_values_tx(&sctx, tx, "SalesOrder", "items", line, v).await?;
        out.push(json!({"line": line, "reserved": crate::util::money(now), "backorder": crate::util::money(open - now)}));
    }
    Ok(json!({"lines": out}))
}

/// Gives back what an order's lines hold.
pub async fn release_order(k: &Kernel, ctx: &Ctx, tx: &mut Tx, order: Uuid) -> Result<Value> {
    let sctx = crate::util::sys(ctx);
    let doc = k.get_tx(&sctx, tx, "SalesOrder", order, false).await?;
    let mut n = 0;
    for r in crate::util::rows(&doc, "items") {
        let Some(line) = crate::util::id(&r, "id") else { continue };
        if crate::util::d(&r, "stock_reserved_qty").is_zero() {
            continue;
        }
        k.set_child_values_tx(&sctx, tx, "SalesOrder", "items", line, obj(json!({"stock_reserved_qty": "0", "backorder_qty": null, "reserved_until": null})))
            .await?;
        n += 1;
    }
    Ok(json!({"released": n}))
}

/// What can be promised of an item from `from`: per warehouse, on hand, what open orders have yet to
/// take (`reserved`, of it `held` under the Hold policy), available now, what comes in by date (open
/// purchase order lines by their schedule date, open work orders by their expected end) and the first
/// day the running total covers `qty`; else the item's lead time from `from`.
pub async fn availability(ctx: &Ctx, tx: &mut Tx, company: Uuid, item: Uuid, qty: Decimal, warehouse: Option<Uuid>, from: NaiveDate) -> Result<Value> {
    // The item's lead time, where buying gives items one.
    let lead: i64 = if ctx.schema.entity("Item").is_some_and(|e| e.field("lead_time_days").is_some()) {
        sqlx::query_scalar::<_, Option<i64>>("select lead_time_days from e_item where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(item)
            .fetch_optional(&mut **tx)
            .await?
            .flatten()
            .unwrap_or(0)
    } else {
        0
    };
    let balances: Vec<(Uuid, Decimal, Decimal)> = sqlx::query_as(
        "select warehouse_id, qty, coalesce(reserved_qty, 0) from stock_balance where tenant_id = $1 and company_id = $2 and item_id = $3 and ($4::uuid is null or warehouse_id = $4)",
    )
    .bind(ctx.tenant)
    .bind(company)
    .bind(item)
    .bind(warehouse)
    .fetch_all(&mut **tx)
    .await?;
    let mut incoming: Vec<(Uuid, NaiveDate, Decimal, String, Uuid)> = Vec::new();
    if ctx.schema.entity("PurchaseOrder").is_some() {
        incoming.extend(
            sqlx::query_as::<_, (Uuid, NaiveDate, Decimal, String, Uuid)>(
                "select coalesce(l.warehouse, o.set_warehouse), coalesce(l.schedule_date, o.schedule_date, o.posting_date), \
                 greatest(l.qty - coalesce(l.received_qty, 0), 0) * coalesce(nullif(l.conversion_factor, 0), 1), 'PurchaseOrder', o.id \
                 from e_purchase_order__items l join e_purchase_order o on o.tenant_id = l.tenant_id and o.id = l.parent_id \
                 where l.tenant_id = $1 and o.company = $2 and l.item = $3 and o.docstatus = 1 and o.status not in ('Closed', 'Completed') \
                 and l.qty > coalesce(l.received_qty, 0) and coalesce(l.warehouse, o.set_warehouse) is not null and ($4::uuid is null or coalesce(l.warehouse, o.set_warehouse) = $4)",
            )
            .bind(ctx.tenant)
            .bind(company)
            .bind(item)
            .bind(warehouse)
            .fetch_all(&mut **tx)
            .await?,
        );
    }
    if ctx.schema.entity("WorkOrder").is_some() {
        incoming.extend(
            sqlx::query_as::<_, (Uuid, NaiveDate, Decimal, String, Uuid)>(
                "select target_warehouse, coalesce(expected_end_date, planned_start_date), greatest(qty - coalesce(produced_qty, 0), 0), 'WorkOrder', id from e_work_order \
                 where tenant_id = $1 and company = $2 and item = $3 and docstatus = 1 and status not in ('Completed', 'Stopped', 'Cancelled') \
                 and qty > coalesce(produced_qty, 0) and ($4::uuid is null or target_warehouse = $4)",
            )
            .bind(ctx.tenant)
            .bind(company)
            .bind(item)
            .bind(warehouse)
            .fetch_all(&mut **tx)
            .await?,
        );
    }
    let mut whs: Vec<Uuid> = balances.iter().map(|b| b.0).chain(incoming.iter().map(|i| i.0)).chain(warehouse).collect();
    whs.sort();
    whs.dedup();
    let mut best: Option<NaiveDate> = None;
    let mut out = Vec::new();
    for wh in whs {
        let (on_hand, reserved) = balances.iter().find(|b| b.0 == wh).map(|b| (b.1, b.2)).unwrap_or_default();
        let hold = held(tx, ctx.tenant, item, wh, None).await?;
        let now = on_hand - reserved.max(hold);
        let mut coming: Vec<&(Uuid, NaiveDate, Decimal, String, Uuid)> = incoming.iter().filter(|i| i.0 == wh).collect();
        coming.sort_by_key(|i| i.1);
        let mut running = now;
        let mut promise = (running >= qty).then_some(from);
        for c in &coming {
            running += c.2;
            if promise.is_none() && running >= qty {
                promise = Some(c.1.max(from));
            }
        }
        if let Some(p) = promise {
            best = Some(best.map_or(p, |b| b.min(p)));
        }
        out.push(json!({"warehouse": wh, "on_hand": crate::util::money(on_hand), "reserved": crate::util::money(reserved), "held": crate::util::money(hold),
            "available_now": crate::util::money(now),
            "incoming": coming.iter().map(|c| json!({"date": c.1, "qty": crate::util::money(c.2), "from": c.3, "id": c.4})).collect::<Vec<_>>(),
            "promise_date": promise}));
    }
    let promise = best.unwrap_or(from + chrono::Duration::days(lead));
    Ok(json!({"item": item, "qty": crate::util::money(qty), "lead_time_days": lead, "promise_date": promise, "warehouses": out}))
}

/// A draft order's stock lines: what is available at their warehouse now, and the day they can be
/// promised, when the line has none yet.
pub async fn promise_lines(ctx: &Ctx, tx: &mut Tx, doc: &mut Doc) -> Result<()> {
    if ctx.schema.entity("SalesOrderItem").is_none_or(|e| e.field("promised_date").is_none()) {
        return Ok(());
    }
    let (Some(company), Ok(on)) = (crate::util::id(doc, "company"), crate::util::date(doc, "posting_date")) else { return Ok(()) };
    let header = crate::util::id(doc, "set_warehouse");
    let mut items = crate::util::rows(doc, "items");
    for r in items.iter_mut() {
        let (Some(item), Some(wh)) = (crate::util::id(r, "item"), crate::util::id(r, "warehouse").or(header)) else { continue };
        let stock: bool = sqlx::query_scalar("select coalesce(is_stock_item, false) from e_item where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(item)
            .fetch_one(&mut **tx)
            .await?;
        if !stock {
            continue;
        }
        let qty = Some(crate::util::d(r, "stock_qty")).filter(|q| !q.is_zero()).unwrap_or_else(|| crate::util::d(r, "qty"));
        let a = availability(ctx, tx, company, item, qty, Some(wh), on).await?;
        let w = a["warehouses"].as_array().and_then(|l| l.first()).cloned().unwrap_or(Value::Null);
        crate::util::set(r, "available_qty", w.get("available_now").cloned().unwrap_or(json!("0")));
        if crate::util::s(r, "promised_date").is_none() {
            crate::util::set(r, "promised_date", a["promise_date"].clone());
        }
    }
    crate::util::set(doc, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
    Ok(())
}

/// The company's `availability` query (a contracted capability, read only).
pub struct AvailabilityHook;

#[async_trait::async_trait]
impl ferrum_kernel::DocHook for AvailabilityHook {
    async fn action(&self, hc: &mut ferrum_kernel::HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "availability" {
            return Ok(None);
        }
        let company = crate::util::id(doc, "id").ok_or_else(|| Error::internal("company without id"))?;
        let text = |k: &str| {
            params.get(k).and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_number().map(|n| n.to_string()))).filter(|v| !v.trim().is_empty())
        };
        let item = text("item").and_then(|i| i.parse().ok()).ok_or_else(|| Error::field("item", "is required"))?;
        let qty = match text("qty") {
            Some(q) => q.trim().parse::<Decimal>().map_err(|_| Error::field("qty", "is not a number"))?,
            None => Decimal::ONE,
        };
        let warehouse = match text("warehouse") {
            Some(w) => Some(w.parse().map_err(|_| Error::field("warehouse", "is not a warehouse"))?),
            None => None,
        };
        let from = match text("date") {
            Some(d) => NaiveDate::parse_from_str(&d, "%Y-%m-%d").map_err(|_| Error::field("date", "is not a date"))?,
            None => hc.ctx.today(),
        };
        availability(hc.ctx, hc.tx, company, item, qty, warehouse, from).await.map(Some)
    }
}
