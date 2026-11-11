//! ERP's side of document flows: the statuses of orders, deliveries and receipts as they are
//! fulfilled and billed, and an invoice's outstanding amount from its open item. The flows'
//! shared helpers (progress counters, mapping one document into the next) are ferrum-domain's.

pub(crate) use ferrum_domain::flow::*;

use crate::util::{d, id, money, rows, s, sys, unit_factor};
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

/// How far past an order line's quantity `field` may go, in percent: the item's allowance when it
/// sets one (an empty field follows the company), else the company's; none unless one is set.
/// Delivered and received quantities take the over-delivery allowance, billed ones the over-billing.
pub async fn allowance(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Option<Uuid>, item: Option<Uuid>, field: &str) -> Result<Decimal> {
    let key = if field == "billed_qty" { "over_billing_allowance" } else { "over_delivery_allowance" };
    let given = |doc: &Doc| doc.get(key).filter(|v| !v.is_null() && v.as_str() != Some("")).map(|_| d(doc, key));
    if let Some(item) = item {
        let it = crate::util::get(k, ctx, tx, "Item", item).await?;
        if let Some(p) = given(&it) {
            return Ok(p.max(Decimal::ZERO));
        }
    }
    match company {
        Some(c) => Ok(given(&crate::util::get(k, ctx, tx, "Company", c).await?).unwrap_or_default().max(Decimal::ZERO)),
        None => Ok(Decimal::ZERO),
    }
}

/// Adds `delta` (counted on a line whose unit holds `factor` stock units) to a progress field of
/// one order line: ferrum-domain's counter, with the allowance of [`allowance`]. Within it the
/// line shows the excess (a delivered quantity above the ordered); beyond it the refusal says what
/// was ordered, what is allowed and what this would bring it to.
#[allow(clippy::too_many_arguments)]
pub async fn add_progress_in(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut Tx,
    entity: &str,
    doc_id: Uuid,
    line_id: Uuid,
    field: &str,
    delta: Decimal,
    factor: Option<Decimal>,
) -> Result<()> {
    let sctx = sys(ctx);
    let order = k.get_tx(&sctx, tx, entity, doc_id, true).await?;
    let number = s(&order, "number").unwrap_or_default();
    if d(&order, "docstatus") != Decimal::ONE {
        return Err(Error::invalid(format!("{entity} {number} is not submitted")));
    }
    let line = rows(&order, "items").into_iter().find(|r| id(r, "id") == Some(line_id)).ok_or_else(|| Error::invalid(format!("line not found on {entity}")))?;
    let own = unit_factor(&line);
    let delta = match factor {
        Some(f) if f > Decimal::ZERO && f != own => (delta * f / own).round_dp(9),
        _ => delta,
    };
    let qty = d(&line, "qty");
    let mut new = d(&line, field) + delta;
    if (new - qty).abs() <= QTY_TOLERANCE {
        new = qty;
    } else if new.abs() <= QTY_TOLERANCE {
        new = Decimal::ZERO;
    }
    if new > qty && delta > Decimal::ZERO {
        let pct = allowance(k, ctx, tx, id(&order, "company"), id(&line, "item"), field).await?;
        let allowed = (qty * (Decimal::ONE_HUNDRED + pct) / Decimal::ONE_HUNDRED).round_dp(9);
        if new > allowed + QTY_TOLERANCE {
            let what = s(&line, "item_name").or(s(&line, "description")).unwrap_or("item");
            let verb = match field {
                "billed_qty" => "billed",
                "received_qty" => "received",
                _ => "delivered",
            };
            let room = if pct.is_zero() { "no more".to_string() } else { format!("{} with its {}% allowance", allowed.normalize(), pct.normalize()) };
            let label = k.entity(ctx, entity).map(|e| e.def.label.clone()).unwrap_or_else(|_| entity.to_string());
            return Err(Error::invalid(format!(
                "{what}: {label} {number} ordered {}, allows {room}, and this would bring what is {verb} to {}",
                qty.normalize(),
                new.normalize()
            )));
        }
    }
    if new < Decimal::ZERO {
        return Err(Error::internal(format!("{field} would become negative on {entity} line {line_id}")));
    }
    let mut v = Doc::new();
    v.insert(field.into(), money(new));
    k.set_child_values_tx(&sctx, tx, entity, "items", line_id, v).await
}

fn pct(done: Decimal, total: Decimal) -> Decimal {
    if total.is_zero() { Decimal::ZERO } else { (done / total * Decimal::ONE_HUNDRED).round_dp(2).min(Decimal::ONE_HUNDRED) }
}

/// Recomputes completion percentages and status of a sales/purchase order.
pub async fn refresh_order(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, doc_id: Uuid, fulfil_field: &str, pct_field: &str) -> Result<()> {
    let sctx = sys(ctx);
    let order = k.get_tx(&sctx, tx, entity, doc_id, false).await?;
    let lines = rows(&order, "items");
    let total: Decimal = lines.iter().map(|l| d(l, "qty")).sum();
    // Lines without an item (services, fees) are billed, never delivered or received: an order of
    // them alone has nothing to deliver.
    let goods: Vec<&Doc> = lines.iter().filter(|l| l.get("item").is_some_and(|v| !v.is_null())).collect();
    let to_move: Decimal = goods.iter().map(|l| d(l, "qty")).sum();
    let done: Decimal = goods.iter().map(|l| d(l, fulfil_field).min(d(l, "qty"))).sum();
    let billed: Decimal = lines.iter().map(|l| d(l, "billed_qty").min(d(l, "qty"))).sum();
    let pf = if goods.is_empty() { Decimal::ONE_HUNDRED } else { pct(done, to_move) };
    let pb = pct(billed, total);
    let docstatus = d(&order, "docstatus");
    let current = s(&order, "status").unwrap_or_default();
    let (to_fulfil, both) = if fulfil_field == "delivered_qty" { ("To Deliver", "To Deliver and Bill") } else { ("To Receive", "To Receive and Bill") };
    let status = if docstatus == Decimal::TWO {
        "Cancelled"
    } else if docstatus.is_zero() {
        "Draft"
    } else if current == "Closed" {
        "Closed"
    } else if pf >= Decimal::ONE_HUNDRED && pb >= Decimal::ONE_HUNDRED {
        "Completed"
    } else if pf >= Decimal::ONE_HUNDRED {
        "To Bill"
    } else if pb >= Decimal::ONE_HUNDRED {
        to_fulfil
    } else {
        both
    };
    let mut v = Doc::new();
    v.insert(pct_field.into(), money(pf));
    v.insert("per_billed".into(), money(pb));
    v.insert("status".into(), json!(status));
    k.set_values_tx(&sctx, tx, entity, doc_id, v).await?;
    if entity == "SalesOrder" {
        crate::reservation::refresh_for_order(tx, ctx.tenant, doc_id).await?;
    }
    Ok(())
}

/// Recomputes `per_billed` and status of a delivery note / purchase receipt.
pub async fn refresh_billing(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, doc_id: Uuid) -> Result<()> {
    let sctx = sys(ctx);
    let doc = k.get_tx(&sctx, tx, entity, doc_id, false).await?;
    let lines = rows(&doc, "items");
    let total: Decimal = lines.iter().map(|l| d(l, "qty")).sum();
    let billed: Decimal = lines.iter().map(|l| d(l, "billed_qty").min(d(l, "qty"))).sum();
    let pb = pct(billed, total);
    let status = match (d(&doc, "docstatus"), crate::util::b(&doc, "is_return")) {
        (x, _) if x == Decimal::TWO => "Cancelled",
        (x, _) if x.is_zero() => "Draft",
        (_, true) => "Return",
        _ if pb >= Decimal::ONE_HUNDRED => "Completed",
        _ => "To Bill",
    };
    let mut v = Doc::new();
    v.insert("per_billed".into(), money(pb));
    v.insert("status".into(), json!(status));
    k.set_values_tx(&sctx, tx, entity, doc_id, v).await
}

pub fn invoice_status(amount: Decimal, outstanding: Decimal, is_return: bool) -> &'static str {
    if is_return {
        "Return"
    } else if outstanding.is_zero() {
        "Paid"
    } else if outstanding == amount {
        "Unpaid"
    } else {
        "Partly Paid"
    }
}

/// Mirrors an open item's outstanding amount onto its invoice document.
pub async fn sync_invoice_outstanding(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, invoice: Uuid) -> Result<()> {
    if !matches!(entity, "SalesInvoice" | "PurchaseInvoice" | "OpeningInvoice") || ctx.schema.entity(entity).is_none() {
        return Ok(());
    }
    let Some(item) = crate::ledger::load_open_item(tx, ctx.tenant, invoice, false).await? else { return Ok(()) };
    let sctx = sys(ctx);
    let doc = k.get_tx(&sctx, tx, entity, invoice, false).await?;
    let is_return = crate::util::b(&doc, "is_return");
    let status = match () {
        _ if item.status == "cancelled" => "Cancelled",
        // An opening credit (an advance, a credit note from the books before) until it is used up.
        _ if entity == "OpeningInvoice" && item.amount < Decimal::ZERO && !item.outstanding.is_zero() => "Credit",
        _ => invoice_status(item.amount, item.outstanding, is_return),
    };
    let mut v = Doc::new();
    v.insert("outstanding_amount".into(), money(item.outstanding));
    v.insert("status".into(), json!(status));
    k.set_values_tx(&sctx, tx, entity, invoice, v).await
}
