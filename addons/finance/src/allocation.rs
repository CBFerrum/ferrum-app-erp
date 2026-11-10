//! Payment allocation: one customer's or supplier's unapplied money (payments, credit notes,
//! journal credits) applied to their open invoices in one document, with what each pairing does to
//! the books in company currency.
//!
//! Each row pairs an open credit with an open invoice of the same party, account and currency for
//! an amount. Submitting allocates both open items by that amount under the document's own key (the
//! ledger's allocations are append-only, so cancelling reverses them) and, when the two were booked
//! at different exchange rates, posts the difference to the exchange gain/loss account. Everything
//! is checked before anything is written, and the whole document posts or none of it does.

use crate::advances::difference;
use crate::common::installed;
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_app_erp::flow::sync_invoice_outstanding;
use ferrum_app_erp::ledger::{self, Entry, Line};
use ferrum_domain::util::{Company, company, d, date, id, money, rows, s, set};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

const ENTITY: &str = "PaymentAllocation";

/// An open item as an allocation sees it.
#[derive(Debug, Clone, sqlx::FromRow)]
struct Item {
    id: Uuid,
    company_id: Uuid,
    entity: String,
    number: Option<String>,
    customer_id: Option<Uuid>,
    supplier_id: Option<Uuid>,
    account_id: Uuid,
    currency: String,
    exchange_rate: Decimal,
    outstanding: Decimal,
    due_date: Option<NaiveDate>,
    posting_date: NaiveDate,
    status: String,
}

async fn items(hc: &mut HookCtx<'_>, ids: &[Uuid], lock: bool) -> Result<HashMap<Uuid, Item>> {
    let sql = format!(
        "select id, company_id, entity, number, customer_id, supplier_id, account_id, trim(currency) as currency, exchange_rate, outstanding, due_date, \
         posting_date, status from open_item where tenant_id = $1 and id = any($2){}",
        if lock { " order by id for update" } else { "" }
    );
    let found: Vec<Item> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(hc.ctx.tenant).bind(ids).fetch_all(&mut **hc.tx).await?;
    Ok(found.into_iter().map(|i| (i.id, i)).collect())
}

/// What a row did, worked out.
#[derive(Debug, Clone)]
struct Pair {
    invoice: Item,
    credit: Item,
    take: Decimal,
    /// Company-currency value of `take` on each side, at the rate each was booked at.
    invoice_base: Decimal,
    credit_base: Decimal,
}

fn bad(row: usize, field: &str, message: impl Into<String>) -> Error {
    Error::Validation(vec![FieldError::new(field, message).in_row("allocations", row)])
}

fn kind(entity: &str) -> &str {
    match entity {
        "SalesInvoice" => "Sales Invoice",
        "PurchaseInvoice" => "Purchase Invoice",
        "OpeningInvoice" => "Opening Invoice",
        "PaymentEntry" => "Payment Entry",
        "JournalEntry" => "Journal Entry",
        other => other,
    }
}

/// Reads and checks every row against the open items as they stand (locked when `lock`): both
/// items must be open, the party's, in the company, on the same account and in the same currency;
/// the credit must be money to apply and the invoice something to pay; no item is used beyond what
/// it has left; and the allocation is not dated before either of them.
async fn pairs(hc: &mut HookCtx<'_>, doc: &Doc, co: &Company, lock: bool) -> Result<Vec<Pair>> {
    let customer = s(doc, "party_type") != Some("Supplier");
    let party = if customer { id(doc, "customer") } else { id(doc, "supplier") }
        .ok_or_else(|| Error::field(if customer { "customer" } else { "supplier" }, "is required"))?;
    let on = date(doc, "posting_date")?;
    let list = rows(doc, "allocations");
    if list.is_empty() {
        return Err(Error::field("allocations", "choose at least one pairing"));
    }
    if !ledger::period_open(hc.tx, hc.ctx.tenant, co.id, on).await? {
        return Err(Error::field("posting_date", "the accounting period of this date is closed"));
    }
    let mut ids = Vec::new();
    for (i, r) in list.iter().enumerate() {
        ids.push(id(r, "credit").ok_or_else(|| bad(i, "credit", "choose the money to apply"))?);
        ids.push(id(r, "invoice").ok_or_else(|| bad(i, "invoice", "choose the invoice to apply it to"))?);
    }
    ids.sort();
    ids.dedup();
    let found = items(hc, &ids, lock).await?;
    let mut used: HashMap<Uuid, Decimal> = HashMap::new();
    let mut out: Vec<Pair> = Vec::new();
    for (i, r) in list.iter().enumerate() {
        let (cid, iid) = (id(r, "credit").unwrap_or_default(), id(r, "invoice").unwrap_or_default());
        let (Some(credit), Some(invoice)) = (found.get(&cid), found.get(&iid)) else {
            return Err(bad(i, "credit", "one of these is no longer open or does not exist"));
        };
        let take = d(r, "allocated_amount");
        if take <= Decimal::ZERO {
            return Err(bad(i, "allocated_amount", "apply an amount above zero"));
        }
        for (field, item) in [("credit", credit), ("invoice", invoice)] {
            let mine = if customer { item.customer_id } else { item.supplier_id };
            if item.company_id != co.id || mine != Some(party) {
                return Err(bad(i, field, if item.company_id != co.id { "belongs to another company" } else { "belongs to another party" }));
            }
            if item.status != "open" {
                return Err(bad(i, field, format!("{} is no longer open", item.number.clone().unwrap_or_else(|| kind(&item.entity).to_string()))));
            }
        }
        if credit.outstanding >= Decimal::ZERO {
            return Err(bad(i, "credit", "this is not money to apply: choose a payment, a credit note or a journal credit"));
        }
        if invoice.outstanding <= Decimal::ZERO {
            return Err(bad(i, "invoice", "there is nothing to pay on this one"));
        }
        if credit.account_id != invoice.account_id {
            return Err(bad(i, "invoice", "is on another receivable or payable account than the money"));
        }
        if credit.currency != invoice.currency {
            return Err(bad(i, "invoice", format!("is in {}, the money in {}", invoice.currency, credit.currency)));
        }
        if on < credit.posting_date.max(invoice.posting_date) {
            return Err(Error::field("posting_date", "an allocation cannot be dated before the money or the invoice it joins"));
        }
        for (item, room) in [(invoice, invoice.outstanding), (credit, -credit.outstanding)] {
            let total = used.entry(item.id).or_default();
            *total += take;
            if *total > room {
                let what = item.number.clone().unwrap_or_else(|| kind(&item.entity).to_string());
                return Err(bad(i, "allocated_amount", format!("{what} has {} left, not {}", room.normalize(), total.normalize())));
            }
        }
        let (invoice_base, credit_base, _) = difference(invoice.exchange_rate, credit.exchange_rate, take, co.precision, customer);
        out.push(Pair { invoice: invoice.clone(), credit: credit.clone(), take, invoice_base, credit_base });
    }
    Ok(out)
}

pub struct AllocationHook;

#[async_trait]
impl DocHook for AllocationHook {
    /// What the server works out for a draft: the party's name, each row's documents and exchange
    /// gain or loss, and the totals. A screen can ask for it before saving (`_calc`).
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let customer = s(doc, "party_type") != Some("Supplier");
        if customer {
            set(doc, "supplier", Value::Null);
        } else {
            set(doc, "customer", Value::Null);
        }
        let party = if customer { id(doc, "customer") } else { id(doc, "supplier") };
        if let Some(p) = party {
            let p = ferrum_domain::util::get(hc.kernel, hc.ctx, hc.tx, if customer { "Customer" } else { "Supplier" }, p).await?;
            set(doc, "party_name", s(&p, if customer { "customer_name" } else { "supplier_name" }).unwrap_or_default().to_string());
        }
        let mut list = rows(doc, "allocations");
        let ids: Vec<Uuid> = list.iter().flat_map(|r| [id(r, "credit"), id(r, "invoice")]).flatten().collect();
        let found = items(hc, &ids, false).await?;
        let (mut total, mut gain, mut currency) = (Decimal::ZERO, Decimal::ZERO, None::<String>);
        for r in list.iter_mut() {
            let (credit, invoice) = (id(r, "credit").and_then(|i| found.get(&i)), id(r, "invoice").and_then(|i| found.get(&i)));
            if let Some(c) = credit {
                set(r, "credit_type", kind(&c.entity).to_string());
                set(r, "credit_number", c.number.clone().map(Value::String).unwrap_or(Value::Null));
            }
            if let Some(i) = invoice {
                set(r, "invoice_type", kind(&i.entity).to_string());
                set(r, "invoice_number", i.number.clone().map(Value::String).unwrap_or(Value::Null));
                set(r, "due_date", i.due_date.map(|x| json!(x.to_string())).unwrap_or(Value::Null));
                currency.get_or_insert_with(|| i.currency.clone());
            }
            let take = d(r, "allocated_amount");
            total += take;
            let g = match (credit, invoice) {
                (Some(c), Some(i)) => difference(i.exchange_rate, c.exchange_rate, take, co.precision, customer).2,
                _ => Decimal::ZERO,
            };
            gain += g;
            set(r, "exchange_difference", money(g));
        }
        set(doc, "allocations", Value::Array(list.into_iter().map(Value::Object).collect()));
        set(doc, "total_allocated", money(total));
        set(doc, "exchange_difference", money(gain));
        if let Some(code) = currency {
            let cur = ferrum_domain::util::find(hc.kernel, hc.ctx, hc.tx, "Currency", vec![ferrum_kernel::Filter::eq("code", code)], None, 1).await?;
            set(doc, "currency", cur.first().and_then(|c| id(c, "id")).map(|i| json!(i.to_string())).unwrap_or(Value::Null));
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?).await?;
        let found = pairs(hc, doc, &co, false).await?;
        let mut currencies: Vec<&str> = found.iter().map(|p| p.invoice.currency.as_str()).collect();
        currencies.dedup();
        if currencies.len() > 1 {
            return Err(Error::field("allocations", "one document applies money in one currency: make one for each"));
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let on = date(doc, "posting_date")?;
        let customer = s(doc, "party_type") != Some("Supplier");
        // Locked and read again: what was fine when the draft was saved must still be.
        let found = pairs(hc, doc, &co, true).await?;
        let number = s(doc, "number").map(str::to_string);
        let mut entry = Entry::new(co.id, on, ENTITY, pid, number.clone());
        entry.memo = Some(format!("Exchange difference on payment allocation {}", number.unwrap_or_default()));
        let mut touched = Vec::new();
        for p in &found {
            ledger::allocate(hc.tx, hc.ctx, co.id, p.invoice.id, ENTITY, pid, on, p.take, p.invoice_base).await?;
            ledger::allocate(hc.tx, hc.ctx, co.id, p.credit.id, ENTITY, pid, on, -p.take, -p.credit_base).await?;
            let diff = p.invoice_base - p.credit_base;
            if !diff.is_zero() {
                // The party's account keeps the base difference between the two rates; clear it.
                let sign = if customer { -Decimal::ONE } else { Decimal::ONE };
                let gl = co.require("exchange_gain_loss_account", "exchange gain/loss account")?;
                entry.lines.push(
                    Line::new(p.invoice.account_id, sign * diff, &co.currency_code)
                        .txn(Decimal::ZERO, &p.invoice.currency, Decimal::ONE)
                        .customer(p.invoice.customer_id)
                        .supplier(p.invoice.supplier_id)
                        .against(&p.invoice.entity, p.invoice.id),
                );
                entry.lines.push(Line::new(gl, -sign * diff, &co.currency_code).remarks("Exchange difference"));
            }
            for item in [&p.invoice, &p.credit] {
                if !touched.contains(&(item.entity.clone(), item.id)) {
                    touched.push((item.entity.clone(), item.id));
                }
            }
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
        for (entity, item) in touched {
            sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, &entity, item).await?;
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        // Allocations and ledger reverse on the same date, so as-of reports agree.
        let (on, _) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        let items = ledger::unallocate(hc.tx, hc.ctx, ENTITY, pid, on).await?;
        ledger::reverse(hc.tx, hc.ctx, ENTITY, pid, on, co.precision).await?;
        for item in items {
            if let Some(o) = ledger::load_open_item(hc.tx, hc.ctx.tenant, item, false).await? {
                sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, &o.entity, item).await?;
            }
        }
        Ok(())
    }
}

/// Invoices, payments and journals that a payment allocation applied cannot be cancelled until the
/// allocation is: it says which one.
pub struct AllocationGuard;

#[async_trait]
impl DocHook for AllocationGuard {
    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let Some(rid) = id(doc, "id") else { return Ok(()) };
        // A journal entry's open items are its lines; every other document's is its own id.
        let found: Option<Option<String>> = sqlx::query_scalar(
            "select pa.number from allocation a join e_payment_allocation pa on pa.tenant_id = a.tenant_id and pa.id = a.payment_id \
             where a.tenant_id = $1 and a.payment_entity = $3 and a.reversal_of is null \
             and not exists (select 1 from allocation r where r.tenant_id = a.tenant_id and r.reversal_of = a.id) \
             and (a.open_item_id = $2 or a.open_item_id in (select l.id from e_journal_entry__accounts l where l.tenant_id = $1 and l.parent_id = $2)) limit 1",
        )
        .bind(hc.ctx.tenant)
        .bind(rid)
        .bind(ENTITY)
        .fetch_optional(&mut **hc.tx)
        .await?;
        match found {
            Some(number) => Err(Error::invalid(format!("this is applied by payment allocation {}; cancel that first", number.unwrap_or_default()))),
            None => Ok(()),
        }
    }
}
