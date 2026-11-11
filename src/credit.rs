//! A customer's credit: what the company has committed to them (open orders not yet billed,
//! deliveries not yet billed, and what is outstanding, each counted once), checked against their
//! credit limit when an order, a delivery or an invoice is submitted or an order is changed, and
//! their credit hold, which stops new orders and deliveries.
//!
//! A sales order line counts for what is neither billed nor (once the order is closed) left to
//! deliver: `max(ordered, delivered) - billed`, or `delivered - billed` on a closed order, so a
//! delivery made from it or an invoice billing it moves the figure without adding to it. A
//! delivery note line counts only when it was made without an order. Figures are in the company's
//! currency, taxes included, for the document's company.

use crate::util::{b, d, get, id, s};
use ferrum_kernel::{Doc, Error, HookCtx, Result};
use rust_decimal::Decimal;
use uuid::Uuid;

/// What is committed to `customer` in `company`, leaving out document `except` (one being submitted).
pub async fn exposure(hc: &mut HookCtx<'_>, company: Uuid, customer: Uuid, except: Option<Uuid>) -> Result<Decimal> {
    let except = except.unwrap_or_default();
    let outstanding: Decimal = sqlx::query_scalar(
        "select coalesce(sum(base_outstanding), 0) from open_item where tenant_id = $1 and customer_id = $2 and company_id = $3 and status = 'open' and id <> $4",
    )
    .bind(hc.ctx.tenant)
    .bind(customer)
    .bind(company)
    .bind(except)
    .fetch_one(&mut **hc.tx)
    .await?;
    let mut total = outstanding;
    if hc.ctx.schema.entity("SalesOrder").is_some() {
        let ordered: Option<Decimal> = sqlx::query_scalar(
            "select sum(greatest(case when o.status = 'Closed' then coalesce(l.delivered_qty, 0) else greatest(l.qty, coalesce(l.delivered_qty, 0)) end \
             - coalesce(l.billed_qty, 0), 0) / l.qty * l.net_amount * o.base_grand_total / o.net_total) \
             from e_sales_order o join e_sales_order__items l on l.tenant_id = o.tenant_id and l.parent_id = o.id \
             where o.tenant_id = $1 and o.customer = $2 and o.company = $3 and o.docstatus = 1 and o.status <> 'Completed' and o.id <> $4 \
             and l.qty > 0 and o.net_total <> 0",
        )
        .bind(hc.ctx.tenant)
        .bind(customer)
        .bind(company)
        .bind(except)
        .fetch_one(&mut **hc.tx)
        .await?;
        total += ordered.unwrap_or_default();
    }
    if hc.ctx.schema.entity("DeliveryNote").is_some() {
        let delivered: Option<Decimal> = sqlx::query_scalar(
            "select sum(greatest(l.qty - coalesce(l.billed_qty, 0) - coalesce(l.returned_qty, 0), 0) / l.qty * l.net_amount * n.base_grand_total / n.net_total) \
             from e_delivery_note n join e_delivery_note__items l on l.tenant_id = n.tenant_id and l.parent_id = n.id \
             where n.tenant_id = $1 and n.customer = $2 and n.company = $3 and n.docstatus = 1 and not coalesce(n.is_return, false) and n.id <> $4 \
             and l.so_line is null and l.qty > 0 and n.net_total <> 0",
        )
        .bind(hc.ctx.tenant)
        .bind(customer)
        .bind(company)
        .bind(except)
        .fetch_one(&mut **hc.tx)
        .await?;
        total += delivered.unwrap_or_default();
    }
    Ok(total)
}

/// The figures a check on `doc` compares, read before the document's effects are written: the
/// customer, their limit and what was committed without this document. None when there is
/// nothing to check: no customer, no limit, a return.
pub struct Before {
    customer: Doc,
    company: Uuid,
    limit: Decimal,
    committed: Decimal,
}

/// `except_self`: leave the document out (it is being submitted and is stored as submitted);
/// a change to a submitted order counts it as it was.
pub async fn before(hc: &mut HookCtx<'_>, doc: &Doc, except_self: bool) -> Result<Option<Before>> {
    let (Some(cust), Some(company)) = (id(doc, "customer"), id(doc, "company")) else { return Ok(None) };
    if b(doc, "is_return") {
        return Ok(None);
    }
    let customer = get(hc.kernel, hc.ctx, hc.tx, "Customer", cust).await?;
    let limit = d(&customer, "credit_limit");
    if limit <= Decimal::ZERO && !b(&customer, "credit_hold") {
        return Ok(None);
    }
    let committed = exposure(hc, company, cust, id(doc, "id").filter(|_| except_self)).await?;
    Ok(Some(Before { customer, company, limit, committed }))
}

/// After a document's effects are written: refused when it adds to what is committed and that
/// passes the limit, unless `released` (the document's Credit Release Reason, which only people
/// with the credit permission can write). `what`: "this order", "this delivery", "this change"; a
/// change that adds anything is also refused while the customer is on hold.
pub async fn check(hc: &mut HookCtx<'_>, before: Option<Before>, what: &str, released: bool) -> Result<()> {
    let Some(before) = before else { return Ok(()) };
    let cust = id(&before.customer, "id").unwrap_or_default();
    let after = exposure(hc, before.company, cust, None).await?;
    let adds = after - before.committed;
    if adds <= Decimal::ZERO {
        return Ok(());
    }
    let name = s(&before.customer, "customer_name").unwrap_or("this customer");
    if what == "this change" && b(&before.customer, "credit_hold") {
        return Err(Error::invalid(format!("{name} is on credit hold ({}): an order cannot grow while they are", hold_reason(&before.customer))));
    }
    if before.limit <= Decimal::ZERO || after <= before.limit || released {
        return Ok(());
    }
    Err(Error::invalid(format!(
        "credit limit exceeded for {name}: the limit is {}, {} is already committed (open orders, deliveries not billed and what is outstanding), \
         and {what} adds {}; someone allowed to release credit can fill in its Credit Release Reason",
        before.limit.normalize(),
        before.committed.round_dp(2).normalize(),
        adds.round_dp(2).normalize()
    )))
}

fn hold_reason(customer: &Doc) -> &str {
    s(customer, "credit_hold_reason").filter(|r| !r.trim().is_empty()).unwrap_or("no reason given")
}

/// A customer on credit hold takes no new orders or deliveries (payments and credit notes are
/// still welcome, and what was delivered can still be billed).
pub async fn hold(hc: &mut HookCtx<'_>, doc: &Doc, what: &str) -> Result<()> {
    let Some(cust) = id(doc, "customer").filter(|_| !b(doc, "is_return")) else { return Ok(()) };
    let customer = get(hc.kernel, hc.ctx, hc.tx, "Customer", cust).await?;
    if b(&customer, "credit_hold") {
        let name = s(&customer, "customer_name").unwrap_or("this customer");
        return Err(Error::invalid(format!("{name} is on credit hold ({}): no new {what} until they are taken off it", hold_reason(&customer))));
    }
    Ok(())
}

/// Whether a document was released over the credit limit (its reason is kept on it).
pub fn released(doc: &Doc) -> bool {
    s(doc, "credit_release_reason").is_some_and(|r| !r.trim().is_empty())
}
