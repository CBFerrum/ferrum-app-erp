//! Revaluing foreign currency balances at the end of a period.
//!
//! What a bank account in euros, a customer who owes euros or a supplier owed euros stands for in the
//! books stays at the rate it was booked at until it is settled. At a period's end the books should
//! carry it at the rate of that day: an exchange revaluation works out, for each such balance
//! (bank and cash accounts, receivables and payables, accounts kept in another currency; each
//! customer and supplier apart), what it would be worth at the day's rate against what the books
//! have, and books the difference to the exchange gain/loss account in one entry. The gain or loss
//! is unrealised, so the revaluation is reversed on the day after (or the day chosen): when the
//! balance is settled, the engine posts the real difference against the original rate, and this
//! entry must not be there to count it twice. Cancelling reverses it at once.

use crate::common::child_rows;
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_app_erp::ledger::{self, Entry, Line};
use ferrum_domain::util::{Company, b, company, d, date, exchange_rate, get, id, money, round, rows, s, set, sys};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

const ENTITY: &str = "ExchangeRevaluation";

/// One balance that stands at another rate than the books booked it at.
#[derive(Debug, Clone)]
struct Balance {
    account: Uuid,
    customer: Option<Uuid>,
    supplier: Option<Uuid>,
    currency: Uuid,
    /// In its own currency; debit positive.
    foreign: Decimal,
    /// In company currency, as the books have it; debit positive.
    book: Decimal,
    rate: Decimal,
    revalued: Decimal,
}

impl Balance {
    /// What the books must take on: positive is a debit to the account and a gain.
    fn gain(&self) -> Decimal {
        self.revalued - self.book
    }
}

/// Foreign currency balances on `on` that are worth something else at that day's rate. Bank and cash
/// accounts, receivables and payables count, and so do accounts kept in another currency.
async fn balances(k: &Kernel, ctx: &Ctx, tx: &mut Tx, co: &Company, on: NaiveDate) -> Result<Vec<Balance>> {
    let found: Vec<(Uuid, Option<Uuid>, Option<Uuid>, String, Decimal, Decimal)> = sqlx::query_as(
        "select l.account_id, l.customer_id, l.supplier_id, trim(l.currency), sum(l.debit_txn - l.credit_txn), sum(l.debit - l.credit) \
         from journal_line_in_book l join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
         where l.tenant_id = $1 and l.company_id = $2 and l.posting_date <= $3 and trim(l.currency) <> $4 \
         and (a.account_type in ('Bank', 'Cash', 'Receivable', 'Payable') or (a.account_currency is not null and a.account_currency <> $5)) \
         group by l.account_id, l.customer_id, l.supplier_id, trim(l.currency), a.account_number \
         having sum(l.debit_txn - l.credit_txn) <> 0 or sum(l.debit - l.credit) <> 0 \
         order by a.account_number nulls last, l.account_id, l.customer_id, l.supplier_id, trim(l.currency)",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(on)
    .bind(&co.currency_code)
    .bind(co.currency_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut out = Vec::new();
    let mut rates: Vec<(String, Uuid, Decimal)> = Vec::new();
    for (account, customer, supplier, code, foreign, book) in found {
        let (currency, rate) = match rates.iter().find(|(c, ..)| *c == code) {
            Some((_, i, r)) => (*i, *r),
            None => {
                let currency: Uuid = sqlx::query_scalar("select id from e_currency where tenant_id = $1 and code = $2")
                    .bind(ctx.tenant)
                    .bind(&code)
                    .fetch_one(&mut **tx)
                    .await?;
                let rate = exchange_rate(k, ctx, tx, currency, co.currency_id, on).await.map_err(|e| match e {
                    Error::Validation(_) => Error::field(
                        "posting_date",
                        format!("no {code} to {} exchange rate is defined on or before {on}: add one under Exchange Rate", co.currency_code),
                    ),
                    other => other,
                })?;
                rates.push((code.clone(), currency, rate));
                (currency, rate)
            }
        };
        let revalued = round(foreign * rate, co.precision);
        let balance = Balance { account, customer, supplier, currency, foreign, book, rate, revalued };
        if !balance.gain().is_zero() {
            out.push(balance);
        }
    }
    Ok(out)
}

/// The rows of a revaluation as the document keeps them.
fn rows_of(found: &[Balance]) -> Value {
    child_rows(
        found
            .iter()
            .map(|b| {
                let mut r = Doc::new();
                r.insert("account".into(), json!(b.account.to_string()));
                r.insert(
                    "party_type".into(),
                    json!(if b.customer.is_some() {
                        "Customer"
                    } else if b.supplier.is_some() {
                        "Supplier"
                    } else {
                        ""
                    }),
                );
                r.insert("customer".into(), b.customer.map(|c| json!(c.to_string())).unwrap_or(Value::Null));
                r.insert("supplier".into(), b.supplier.map(|c| json!(c.to_string())).unwrap_or(Value::Null));
                r.insert("currency".into(), json!(b.currency.to_string()));
                r.insert("balance".into(), money(b.foreign));
                r.insert("book_value".into(), money(b.book));
                r.insert("exchange_rate".into(), money(b.rate));
                r.insert("revalued".into(), money(b.revalued));
                r.insert("gain_loss".into(), money(b.gain()));
                r
            })
            .collect(),
    )
}

pub struct RevaluationHook;

#[async_trait]
impl DocHook for RevaluationHook {
    /// What the server works out for a draft: the balances to revalue on the day, and what each
    /// gains or loses. A screen can ask for it before saving (`_calc`).
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !d(doc, "docstatus").is_zero() {
            return Ok(());
        }
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        if id(doc, "gain_loss_account").is_none()
            && let Some(a) = co.account("exchange_gain_loss_account")
        {
            set(doc, "gain_loss_account", a.to_string());
        }
        let Ok(on) = date(doc, "posting_date") else { return Ok(()) };
        let found = balances(hc.kernel, hc.ctx, hc.tx, &co, on).await?;
        set(doc, "rows", rows_of(&found));
        set(doc, "total_gain_loss", money(found.iter().map(Balance::gain).sum()));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let mut errors = Vec::new();
        match id(doc, "gain_loss_account") {
            Some(a) => {
                let acc = get(hc.kernel, hc.ctx, hc.tx, "Account", a).await?;
                if id(&acc, "company") != Some(cid) || b(&acc, "is_group") {
                    errors.push(FieldError::new("gain_loss_account", "choose an account of this company that takes postings"));
                }
            }
            None => errors.push(FieldError::new("gain_loss_account", "the company has no exchange gain/loss account: choose one")),
        }
        if b(doc, "auto_reverse")
            && let (Ok(on), Ok(back)) = (date(doc, "posting_date"), date(doc, "reversal_date"))
            && back <= on
        {
            errors.push(FieldError::new("reversal_date", "must be after the day of the revaluation"));
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    /// Read again as the document is submitted: what is posted is what the document then shows.
    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?).await?;
        let on = date(doc, "posting_date")?;
        let found = balances(hc.kernel, hc.ctx, hc.tx, &co, on).await?;
        if found.is_empty() {
            return Err(Error::field("posting_date", format!("nothing to revalue on {on}: every foreign currency balance already stands at that day's rate")));
        }
        set(doc, "rows", rows_of(&found));
        set(doc, "total_gain_loss", money(found.iter().map(Balance::gain).sum()));
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let rid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let on = date(doc, "posting_date")?;
        let gl = id(doc, "gain_loss_account").ok_or_else(|| Error::field("gain_loss_account", "is required"))?;
        // The balances the document shows are the ones it posts.
        let mut entry = Entry::new(co.id, on, ENTITY, rid, s(doc, "number").map(str::to_string));
        entry.memo = Some(format!("Revaluation of foreign currency balances on {on}"));
        let mut net = Decimal::ZERO;
        for r in rows(doc, "rows") {
            let (Some(account), Some(currency)) = (id(&r, "account"), id(&r, "currency")) else { continue };
            let code: String = get(hc.kernel, hc.ctx, hc.tx, "Currency", currency).await?.get("code").and_then(Value::as_str).unwrap_or_default().to_string();
            let gain = d(&r, "gain_loss");
            net += gain;
            entry.lines.push(
                Line::new(account, gain, &co.currency_code)
                    .txn(Decimal::ZERO, &code, d(&r, "exchange_rate"))
                    .customer(id(&r, "customer"))
                    .supplier(id(&r, "supplier"))
                    .remarks("Revaluation"),
            );
        }
        entry.lines.push(Line::new(gl, -net, &co.currency_code).remarks("Unrealised exchange difference"));
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
        if b(doc, "auto_reverse") {
            ferrum_app_erp::accounting::reverse_from(hc, ENTITY, doc, on, co.precision).await?;
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let rid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        // What was reversed on its day stays as it is: the two net to nothing.
        ledger::reverse(hc.tx, hc.ctx, ENTITY, rid, hc.ctx.today(), co.precision).await?;
        Ok(())
    }
}

/// Reverses the revaluation's entry on its reversal day, once: the document records the day.
async fn reverse_posted(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc) -> Result<()> {
    let rid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let co = company(k, ctx, tx, id(doc, "company").unwrap_or_default()).await?;
    let on = date(doc, "reversal_date")?;
    ledger::reverse_on(tx, ctx, ENTITY, rid, on, co.precision).await?;
    let mut v = Doc::new();
    v.insert("reversed_on".into(), json!(on.to_string()));
    k.set_values_tx(&sys(ctx), tx, ENTITY, rid, v).await
}

/// The reversals whose day has come: each in a transaction of its own, so one that cannot be
/// posted (its period closed) waits for the next round without holding up the others.
pub async fn upkeep(k: &Kernel, ctx: &Ctx) -> Result<()> {
    let today = ctx.today();
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let due: Vec<Uuid> = sqlx::query_scalar(
        "select id from e_exchange_revaluation where tenant_id = $1 and docstatus = 1 and auto_reverse and reversed_on is null and reversal_date <= $2 order by reversal_date",
    )
    .bind(ctx.tenant)
    .bind(today)
    .fetch_all(&mut *tx)
    .await?;
    tx.rollback().await?;
    for rid in due {
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let done = async {
            let doc = k.get_tx(&sys(ctx), &mut tx, ENTITY, rid, true).await?;
            reverse_posted(k, ctx, &mut tx, &doc).await
        }
        .await;
        match done {
            Ok(()) => tx.commit().await?,
            Err(e) => {
                tx.rollback().await?;
                tracing::warn!(tenant = %ctx.tenant, revaluation = %rid, "revaluation not reversed: {e}");
            }
        }
    }
    Ok(())
}
