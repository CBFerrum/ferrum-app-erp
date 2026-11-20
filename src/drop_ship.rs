//! Drop shipment (docs/drop-shipment.md): a sales order line the supplier sends straight to the customer. Its purchase
//! order is made from the sales order; the purchase receipt records that the supplier shipped, without a stock move,
//! booking cost of sales against "stock received but not billed" and marking the sales line delivered; the supplier's
//! bill clears that account at the receipt's value and puts any price difference in cost of sales.

use crate::flow::{QTY_TOLERANCE, add_progress_in, create_for_user, created, refresh_order};
use crate::ledger::Line;
use crate::pricing::breakup;
use crate::util::{Company, b, d, id, money, round, rows, s, stock_qty, sys, unit_factor};
use ferrum_kernel::{Doc, Error, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// `SalesOrder.make_drop_ship_order`: a draft purchase order per supplier (or for `supplier` alone) for the drop-ship
/// lines not yet ordered, delivered to the customer.
pub async fn make_order(hc: &mut HookCtx<'_>, so: &Doc, params: &Value) -> Result<Value> {
    if hc.ctx.schema.entity("PurchaseOrder").is_none() {
        return Err(Error::invalid("Buying is not installed: there is no purchase order to make"));
    }
    let only = params.get("supplier").and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
    let mut by_supplier: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for r in rows(so, "items") {
        let (true, Some(supplier), Some(item)) = (b(&r, "drop_ship"), s(&r, "drop_ship_supplier"), id(&r, "item")) else { continue };
        if only.as_deref().is_some_and(|o| o != supplier) {
            continue;
        }
        let left = d(&r, "qty") - d(&r, "drop_ship_ordered_qty");
        if left <= QTY_TOLERANCE {
            continue;
        }
        by_supplier.entry(supplier.to_string()).or_default().push(json!({
            "item": item.to_string(), "uom": r.get("uom"), "qty": money(left), "drop_ship": true,
            "sales_order": s(so, "id"), "so_line": s(&r, "id"), "schedule_date": r.get("delivery_date").or(so.get("delivery_date")),
        }));
    }
    if by_supplier.is_empty() {
        return Err(Error::invalid(format!("{}: no line the supplier delivers is left to order", s(so, "number").unwrap_or_default())));
    }
    let mut orders = Vec::new();
    for (supplier, lines) in by_supplier {
        let mut input = Doc::new();
        input.insert("company".into(), so.get("company").cloned().unwrap_or(Value::Null));
        input.insert("supplier".into(), json!(supplier));
        input.insert("ship_to".into(), so.get("ship_to").filter(|v| !v.is_null()).or(so.get("customer_address")).cloned().unwrap_or(Value::Null));
        input.insert("drop_ship_customer".into(), so.get("customer").cloned().unwrap_or(Value::Null));
        input.insert("items".into(), Value::Array(lines));
        let po = create_for_user(hc, "PurchaseOrder", input).await?;
        let mut made = created("PurchaseOrder", &po);
        made["supplier"] = json!(supplier);
        orders.push(made);
    }
    Ok(json!({"orders": orders}))
}

/// Recounts what each sales order a purchase order delivers for has ordered from suppliers, under the sales order's
/// row lock. With `check` (a submit, Update Items, a re-open), more than a sales line's quantity is refused.
pub async fn recount(hc: &mut HookCtx<'_>, po: &Doc, check: bool) -> Result<()> {
    let mut sales: Vec<Uuid> = rows(po, "items").iter().filter_map(|r| id(r, "sales_order")).collect();
    sales.sort();
    sales.dedup();
    if sales.is_empty() || hc.ctx.schema.entity("SalesOrder").is_none() {
        return Ok(());
    }
    let sctx = sys(hc.ctx);
    for sid in sales {
        let so = hc.kernel.get_tx(&sctx, hc.tx, "SalesOrder", sid, true).await?;
        let ordered: Vec<(String, Decimal)> = sqlx::query_as(
            "select l.so_line, sum((l.qty - coalesce(l.closed_qty, 0)) * coalesce(nullif(l.conversion_factor, 0), 1)) \
             from e_purchase_order o join e_purchase_order__items l on l.tenant_id = o.tenant_id and l.parent_id = o.id \
             where o.tenant_id = $1 and o.docstatus = 1 and l.sales_order = $2 and l.so_line is not null group by 1",
        )
        .bind(hc.ctx.tenant)
        .bind(sid.to_string())
        .fetch_all(&mut **hc.tx)
        .await?;
        let ordered: HashMap<String, Decimal> = ordered.into_iter().collect();
        let number = s(&so, "number").unwrap_or_default().to_string();
        for r in rows(&so, "items").into_iter().filter(|r| b(r, "drop_ship")) {
            let Some(line) = s(&r, "id").map(str::to_string) else { continue };
            let o = (ordered.get(&line).copied().unwrap_or_default() / unit_factor(&r)).round_dp(9);
            if check && o > d(&r, "qty") + QTY_TOLERANCE {
                return Err(Error::invalid(format!(
                    "{}: {number} sells {}; this would order {} from suppliers",
                    s(&r, "item_name").unwrap_or_default(),
                    d(&r, "qty").normalize(),
                    o.normalize()
                )));
            }
            let mut v = Doc::new();
            v.insert("drop_ship_ordered_qty".into(), money(o));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, "SalesOrder", "items", line.parse().map_err(|_| Error::internal("line id"))?, v).await?;
        }
    }
    Ok(())
}

/// A receipt's drop-ship lines: cost of sales debited and "received but not billed" credited with each line's net
/// amount in company currency (negative on a return, so the signs follow).
pub fn cost_lines(co: &Company, doc: &Doc) -> Result<Vec<Line>> {
    let mut out = Vec::new();
    for (r, base) in breakup(doc).lines.iter().filter(|(r, _)| b(r, "drop_ship")) {
        let v = if b(doc, "is_return") { -base.abs() } else { *base };
        if v.is_zero() {
            continue;
        }
        let cogs = co.require("cogs_account", "cost of goods sold account")?;
        let srbnb = co.require("srbnb_account", "stock received but not billed account")?;
        out.push(Line::new(cogs, v, &co.currency_code).cost_center(id(r, "cost_center")).remarks("Drop shipment"));
        out.push(Line::new(srbnb, -v, &co.currency_code).remarks("Drop shipment"));
    }
    Ok(out)
}

/// The sales order lines a receipt's drop-ship lines deliver, moved by `sign` (1 on submit; −1 on cancel; a return the
/// other way), each sales order refreshed after.
pub async fn deliver(hc: &mut HookCtx<'_>, doc: &Doc, sign: Decimal) -> Result<()> {
    if hc.ctx.schema.entity("SalesOrder").is_none() {
        return Ok(());
    }
    let sign = if b(doc, "is_return") { -sign } else { sign };
    let mut orders = Vec::new();
    for r in rows(doc, "items").into_iter().filter(|r| b(r, "drop_ship")) {
        let (Some(so), Some(line)) = (id(&r, "sales_order"), id(&r, "so_line")) else { continue };
        add_progress_in(hc.kernel, hc.ctx, hc.tx, "SalesOrder", so, line, "delivered_qty", sign * d(&r, "qty").abs(), Some(unit_factor(&r))).await?;
        if !orders.contains(&so) {
            orders.push(so);
        }
    }
    for so in orders {
        refresh_order(hc.kernel, hc.ctx, hc.tx, "SalesOrder", so, "delivered_qty", "per_delivered").await?;
    }
    Ok(())
}

/// A supplier's bill line for a drop-ship receipt line: "received but not billed" cleared at the receipt line's value
/// for the quantity billed, the difference in cost of sales. None when the line is not one.
pub async fn bill_lines(hc: &mut HookCtx<'_>, co: &Company, r: &Doc, base: Decimal) -> Result<Option<Vec<Line>>> {
    let (true, Some(pr), Some(pr_line)) = (b(r, "drop_ship"), id(r, "purchase_receipt"), id(r, "pr_line")) else { return Ok(None) };
    let receipt = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "PurchaseReceipt", pr, false).await?;
    let Some((line, value)) = breakup(&receipt).lines.into_iter().find(|(l, _)| id(l, "id") == Some(pr_line)) else { return Ok(None) };
    let received = stock_qty(&line);
    if received.is_zero() {
        return Ok(None);
    }
    let cleared = round(value / received * stock_qty(r), co.precision);
    let srbnb = co.require("srbnb_account", "stock received but not billed account")?;
    let cogs = co.require("cogs_account", "cost of goods sold account")?;
    let mut out = vec![Line::new(srbnb, cleared, &co.currency_code).remarks("SRBNB clearing")];
    if base != cleared {
        out.push(Line::new(cogs, base - cleared, &co.currency_code).remarks("Drop shipment price difference"));
    }
    Ok(Some(out))
}

/// A delivery note may not deliver a sales order line the supplier delivers.
pub async fn refuse_delivery(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
    let mut orders: HashMap<Uuid, Doc> = HashMap::new();
    for (i, r) in rows(doc, "items").iter().enumerate() {
        let (Some(so), Some(line)) = (id(r, "sales_order"), id(r, "so_line")) else { continue };
        if let std::collections::hash_map::Entry::Vacant(e) = orders.entry(so) {
            e.insert(hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "SalesOrder", so, false).await?);
        }
        if rows(&orders[&so], "items").iter().any(|l| id(l, "id") == Some(line) && b(l, "drop_ship")) {
            return Err(Error::field(format!("items.{i}.so_line"), "the supplier delivers this line: it is not delivered from stock"));
        }
    }
    Ok(())
}
