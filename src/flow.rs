//! ERP's side of document flows: the statuses of orders, deliveries and receipts as they are
//! fulfilled and billed, and an invoice's outstanding amount from its open item. The flows'
//! shared helpers (progress counters, mapping one document into the next) are ferrum-domain's.

pub(crate) use ferrum_domain::flow::*;

use crate::util::{d, money, rows, s, sys};
use ferrum_kernel::{Ctx, Doc, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

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
    if !matches!(entity, "SalesInvoice" | "PurchaseInvoice") || ctx.schema.entity(entity).is_none() {
        return Ok(());
    }
    let Some(item) = crate::ledger::load_open_item(tx, ctx.tenant, invoice, false).await? else { return Ok(()) };
    let sctx = sys(ctx);
    let doc = k.get_tx(&sctx, tx, entity, invoice, false).await?;
    let is_return = crate::util::b(&doc, "is_return");
    let status = if item.status == "cancelled" { "Cancelled" } else { invoice_status(item.amount, item.outstanding, is_return) };
    let mut v = Doc::new();
    v.insert("outstanding_amount".into(), money(item.outstanding));
    v.insert("status".into(), json!(status));
    k.set_values_tx(&sctx, tx, entity, invoice, v).await
}
