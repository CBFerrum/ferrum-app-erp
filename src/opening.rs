//! Opening invoices: the invoices still open in the books a company comes from, carried into these
//! one by one (so a spreadsheet imports them), each an open item a payment settles as it settles an
//! invoice made here, under the number the party knows, falling due on its own day, in its own
//! currency. Posted against the company's Opening Balance Account on the opening date, one at a
//! time or all a company's drafts at once.

use crate::flow::sync_invoice_outstanding;
use crate::ledger::{self, Entry, Line, OpenItem};
use crate::util::{company, d, date, get, id, money, round, s, set};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, Error, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

/// The party's field, entity, name field, and the receivable or payable default.
fn side(doc: &Doc) -> (&'static str, &'static str, &'static str, &'static str, &'static str) {
    if s(doc, "party_type") == Some("Supplier") {
        ("supplier", "Supplier", "supplier_name", "payable_account", "default_payable_account")
    } else {
        ("customer", "Customer", "customer_name", "receivable_account", "default_receivable_account")
    }
}

/// "Opening invoice INV-77 of Acme".
fn named(doc: &Doc) -> String {
    format!("Opening invoice {} of {}", s(doc, "invoice_no").unwrap_or("?"), s(doc, "party_name").unwrap_or("the party"))
}

pub struct OpeningInvoiceHook;

#[async_trait]
impl DocHook for OpeningInvoiceHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let (field, entity, name, own, default) = side(doc);
        let other = if field == "customer" { "supplier" } else { "customer" };
        set(doc, other, Value::Null);
        if let Some(p) = id(doc, field) {
            let p = get(hc.kernel, hc.ctx, hc.tx, entity, p).await?;
            set(doc, "party_name", p.get(name).cloned().unwrap_or(Value::Null));
            let acc = id(&p, own).or(co.account(default));
            set(doc, "party_account", acc.map(|a| json!(a.to_string())).unwrap_or(Value::Null));
        }
        set(doc, "opening_account", co.account("opening_balance_account").map(|a| json!(a.to_string())).unwrap_or(Value::Null));
        if id(doc, "currency").is_none() {
            set(doc, "currency", co.currency_id.to_string());
        }
        // In the company's currency the rate is 1; in another, the rate given (never guessed: it
        // is the one the old books carried it at).
        let rate = if id(doc, "currency") == Some(co.currency_id) { Decimal::ONE } else { d(doc, "conversion_rate") };
        set(doc, "conversion_rate", money(rate));
        set(doc, "base_amount", money(round(d(doc, "amount") * rate, co.precision)));
        if d(doc, "docstatus").is_zero() {
            set(doc, "status", "Draft");
            set(doc, "outstanding_amount", money(d(doc, "amount")));
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let what = named(doc);
        if d(doc, "amount").is_zero() {
            return Err(Error::field("amount", format!("{what} has nothing open: an amount of zero carries nothing into these books")));
        }
        if d(doc, "conversion_rate") <= Decimal::ZERO {
            let cur = id(doc, "currency").unwrap_or_default();
            let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, cur).await?;
            return Err(Error::field("conversion_rate", format!("{what} is in {code}: give the rate it is carried at")));
        }
        if id(doc, "party_account").is_none() {
            return Err(Error::field("party_account", "no receivable/payable account found for this party or company"));
        }
        if id(doc, "opening_account").is_none() {
            return Err(Error::field("opening_account", "set the company's Opening Balance Account (Create Standard Chart of Accounts sets it)"));
        }
        if let (Ok(invoiced), Ok(opening)) = (date(doc, "invoice_date"), date(doc, "posting_date"))
            && invoiced > opening
        {
            return Err(Error::field(
                "invoice_date",
                format!("{what} is dated {invoiced}, after the opening date {opening}: an invoice from after the books' start is entered as an invoice, not an opening balance"),
            ));
        }
        let (field, ..) = side(doc);
        let (Some(party), Some(number)) = (id(doc, field), s(doc, "invoice_no").map(str::trim)) else { return Ok(()) };
        let me = id(doc, "id");
        let twice: Option<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select id from e_opening_invoice where tenant_id = $1 and company = $2 and {field} = $3 and trim(invoice_no) = $4 and docstatus < 2 \
             and id is distinct from $5 limit 1"
        )))
        .bind(hc.ctx.tenant)
        .bind(id(doc, "company"))
        .bind(party)
        .bind(number)
        .bind(me)
        .fetch_optional(&mut **hc.tx)
        .await?;
        if twice.is_some() {
            return Err(Error::field("invoice_no", format!("{what} is already carried in: one opening invoice per number and party")));
        }
        // The same invoice made here already: carrying it in as well would count it twice.
        let (table, column) = if field == "customer" { ("e_sales_invoice", "number") } else { ("e_purchase_invoice", "bill_no") };
        let entity = if field == "customer" { "SalesInvoice" } else { "PurchaseInvoice" };
        if hc.ctx.schema.entity(entity).is_some() {
            let real: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "select coalesce(number, id::text) from {table} where tenant_id = $1 and company = $2 and {field} = $3 and trim({column}) = $4 and docstatus = 1 limit 1"
            )))
            .bind(hc.ctx.tenant)
            .bind(id(doc, "company"))
            .bind(party)
            .bind(number)
            .fetch_optional(&mut **hc.tx)
            .await?;
            if let Some(real) = real {
                return Err(Error::field("invoice_no", format!("{what} is already in these books as {real}: carrying it in as well would count it twice")));
            }
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let oid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let on = date(doc, "posting_date")?;
        let invoiced = date(doc, "invoice_date")?;
        let (field, ..) = side(doc);
        let party = id(doc, field);
        let (customer, supplier) = if field == "customer" { (party, None) } else { (None, party) };
        let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, id(doc, "currency").unwrap_or(co.currency_id)).await?;
        let rate = d(doc, "conversion_rate");
        let (amount, base) = (d(doc, "amount"), d(doc, "base_amount"));
        let account = id(doc, "party_account").ok_or_else(|| Error::field("party_account", "is required"))?;
        let opening = id(doc, "opening_account").ok_or_else(|| Error::field("opening_account", "is required"))?;
        let number = s(doc, "invoice_no").map(|x| x.trim().to_string());
        // A customer owes a debit; the company owes a supplier a credit. Negative, the other way.
        let sign = if customer.is_some() { Decimal::ONE } else { -Decimal::ONE };
        let mut entry = Entry::new(co.id, on, "OpeningInvoice", oid, number.clone());
        entry.memo = Some(format!("Opening balance: {}", named(doc)));
        entry.lines.push(Line::new(account, sign * base, &co.currency_code).txn(sign * amount, &code, rate).customer(customer).supplier(supplier).against("OpeningInvoice", oid));
        entry.lines.push(Line::new(opening, -sign * base, &co.currency_code).remarks("Opening balance"));
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
        ledger::open_item(
            hc.tx,
            hc.ctx,
            &OpenItem {
                id: oid,
                company: co.id,
                entity: "OpeningInvoice".into(),
                number,
                customer,
                supplier,
                account,
                // Aged from the old invoice's own days, not the day it was carried in.
                posting_date: invoiced,
                due_date: Some(date(doc, "due_date").unwrap_or(invoiced)),
                currency: code,
                exchange_rate: rate,
                amount,
                base_amount: base,
            },
        )
        .await?;
        sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, "OpeningInvoice", oid).await
    }

    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let oid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let live: i64 = sqlx::query_scalar(
            "select count(*) from allocation a where a.tenant_id = $1 and a.open_item_id = $2 and a.reversal_of is null \
             and not exists (select 1 from allocation r where r.tenant_id = a.tenant_id and r.reversal_of = a.id)",
        )
        .bind(hc.ctx.tenant)
        .bind(oid)
        .fetch_one(&mut **hc.tx)
        .await?;
        if live > 0 {
            return Err(Error::invalid(format!("{} has payments allocated to it; cancel those payments first", named(doc))));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let oid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, _) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        ledger::cancel_open_item(hc.tx, hc.ctx, oid).await?;
        ledger::reverse(hc.tx, hc.ctx, "OpeningInvoice", oid, on, co.precision).await?;
        sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, "OpeningInvoice", oid).await
    }
}

/// Company action `post_opening_invoices`: submits every draft opening invoice of the company in
/// one transaction, as the person who asks (who needs to submit opening invoices). All are posted,
/// or none: the first refused is named and nothing is posted.
pub async fn post_all(hc: &mut HookCtx<'_>, company: Uuid) -> Result<Value> {
    if hc.ctx.schema.entity("OpeningInvoice").is_none() {
        return Err(Error::invalid("opening invoices are not installed here"));
    }
    let drafts: Vec<(Uuid, Option<String>, Option<String>)> = sqlx::query_as(
        "select id, invoice_no, party_name from e_opening_invoice where tenant_id = $1 and company = $2 and docstatus = 0 order by invoice_date, invoice_no, id",
    )
    .bind(hc.ctx.tenant)
    .bind(company)
    .fetch_all(&mut **hc.tx)
    .await?;
    if drafts.is_empty() {
        return Err(Error::invalid("this company has no draft opening invoices to post"));
    }
    for (oid, number, party) in &drafts {
        if let Err(e) = hc.kernel.submit_tx(hc.ctx, hc.tx, "OpeningInvoice", *oid, None).await {
            let why = match &e {
                Error::Validation(fields) => fields.iter().map(|f| f.message.clone()).collect::<Vec<_>>().join("; "),
                other => other.to_string(),
            };
            let (number, party) = (number.as_deref().unwrap_or("?"), party.as_deref().unwrap_or("the party"));
            return Err(Error::invalid(format!("Opening invoice {number} of {party} was refused, so none was posted: {why}")));
        }
    }
    let n = drafts.len();
    Ok(json!({"posted": n, "message": format!("Posted {n} opening invoice{}", if n == 1 { "" } else { "s" })}))
}
