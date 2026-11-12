//! Payment instruments (FN-34): cheques, drafts, promissory notes, with a life of their own. A
//! payment whose mode of payment uses instruments books to the mode's instruments received account
//! (money in) or instruments issued account (money out) in the bank account's place, and makes a
//! `PaymentInstrument` holding the number, the date written on it and its bank. Clearing, on or after
//! that date, moves it into the bank account; a bounce takes the payment back on its day (its
//! invoices are owed again), reverses a clearing, charges the customer the mode's charge when asked,
//! and raises `accounting.instrument_bounced`. Every action locks the instrument first, so two people
//! (or two replicas) never clear or bounce it twice. Nothing here names a country: the mode says.

use crate::ledger::{self, Entry, Line};
use crate::util::{b, company, d, date, get, id, money, rows, s, sys};
use chrono::NaiveDate;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

const ENTITY: &str = "PaymentInstrument";

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap_or_default()
}

/// The payment's mode, when it uses instruments, with the company's row of its accounts.
async fn mode(hc: &mut HookCtx<'_>, doc: &Doc, company: Uuid) -> Result<Option<(Doc, Doc)>> {
    let Some(m) = id(doc, "mode_of_payment") else { return Ok(None) };
    let m = get(hc.kernel, hc.ctx, hc.tx, "ModeOfPayment", m).await?;
    if !b(&m, "uses_instruments") {
        return Ok(None);
    }
    let row = rows(&m, "accounts").into_iter().find(|r| id(r, "company") == Some(company)).unwrap_or_default();
    Ok(Some((m, row)))
}

/// A payment by instrument needs its number, its date and the account it waits in.
pub async fn check_payment(hc: &mut HookCtx<'_>, doc: &Doc, co: &crate::util::Company) -> Result<()> {
    let Some((m, row)) = mode(hc, doc, co.id).await? else { return Ok(()) };
    let name = s(&m, "mode_name").unwrap_or("this mode").to_string();
    if s(doc, "payment_type") == Some("Internal Transfer") {
        return Err(Error::field("mode_of_payment", format!("{name} is paid by instrument: a transfer between the company's own accounts is not")));
    }
    // A payment out takes its number from the mode's book, and its posting date, on submission.
    let out = s(doc, "payment_type") == Some("Pay");
    let book = out && row.get("next_instrument_no").is_some_and(|v| !v.is_null());
    if !book && s(doc, "reference_no").is_none_or(|r| r.trim().is_empty()) {
        return Err(Error::field("reference_no", format!("{name} is paid by instrument: give its number")));
    }
    if !out && date(doc, "reference_date").is_err() {
        return Err(Error::field("reference_date", format!("{name} is paid by instrument: give the date written on it")));
    }
    let (field, what) =
        if s(doc, "payment_type") == Some("Pay") { ("issued_account", "instruments issued") } else { ("in_hand_account", "instruments received") };
    if id(&row, field).is_none() {
        return Err(Error::field("mode_of_payment", format!("{name} has no {what} account for this company: add it to the mode's accounts")));
    }
    Ok(())
}

/// On submission, a payment out by instrument without a number takes the next from its mode's book
/// for the company (one locked update, so two payments never take the same number; the book used
/// up is refused), and one without a date is dated its posting date.
pub async fn fill(hc: &mut HookCtx<'_>, doc: &mut Doc, co: &crate::util::Company) -> Result<()> {
    if s(doc, "payment_type") != Some("Pay") {
        return Ok(());
    }
    let Some((m, row)) = mode(hc, doc, co.id).await? else { return Ok(()) };
    if date(doc, "reference_date").is_err()
        && let Some(on) = doc.get("posting_date").cloned()
    {
        doc.insert("reference_date".into(), on);
    }
    if s(doc, "reference_no").is_some_and(|r| !r.trim().is_empty()) || row.get("next_instrument_no").is_none_or(Value::is_null) {
        return Ok(());
    }
    let table = ferrum_kernel::ddl::q(&hc.ctx.schema.entity("ModeOfPaymentAccount").ok_or_else(|| Error::internal("no mode accounts"))?.table);
    let drawn: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "update {table} set next_instrument_no = next_instrument_no + 1 where tenant_id = $1 and id = $2 \
         and next_instrument_no is not null and (last_instrument_no is null or next_instrument_no <= last_instrument_no) returning (next_instrument_no - 1)::bigint"
    )))
    .bind(hc.ctx.tenant)
    .bind(id(&row, "id"))
    .fetch_optional(&mut **hc.tx)
    .await?;
    let Some(n) = drawn else {
        let name = s(&m, "mode_name").unwrap_or("this mode");
        return Err(Error::field("reference_no", format!("{name}'s book of numbers is used up: give the number, or start the next book on the mode")));
    };
    let width = row.get("instrument_digits").and_then(Value::as_i64).unwrap_or(0).clamp(0, 20) as usize;
    doc.insert("reference_no".into(), json!(format!("{}{:0width$}", s(&row, "instrument_prefix").unwrap_or_default(), n)));
    Ok(())
}

/// The account a payment by instrument books in the bank account's place, or `None`.
pub async fn holding_account(hc: &mut HookCtx<'_>, doc: &Doc, co: &crate::util::Company, receive: bool) -> Result<Option<Uuid>> {
    let Some((_, row)) = mode(hc, doc, co.id).await? else { return Ok(None) };
    Ok(id(&row, if receive { "in_hand_account" } else { "issued_account" }))
}

/// Makes the payment's instrument, received or issued.
pub async fn payment_submitted(hc: &mut HookCtx<'_>, doc: &Doc, co: &crate::util::Company, receive: bool) -> Result<()> {
    let Some(holding) = holding_account(hc, doc, co, receive).await? else { return Ok(()) };
    let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let direction = if receive { "Received" } else { "Issued" };
    let made = hc
        .kernel
        .insert_tx(
            &sys(hc.ctx),
            hc.tx,
            ENTITY,
            obj(json!({
                "company": co.id, "direction": direction, "instrument_no": s(doc, "reference_no"), "instrument_date": s(doc, "reference_date"),
                "bank": s(doc, "instrument_bank"), "mode_of_payment": s(doc, "mode_of_payment"), "payment_entry": pid, "party_name": s(doc, "party_name"), "customer": s(doc, "customer"), "supplier": s(doc, "supplier"),
                "currency": s(doc, "currency"), "amount": money(d(doc, "paid_amount")), "status": direction,
                "holding_account": holding,
            })),
        )
        .await?;
    let mut v = Doc::new();
    v.insert("payment_instrument".into(), made["id"].clone());
    hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "PaymentEntry", pid, v).await
}

/// A payment cancelled by hand: its instrument is cancelled with it, unless it cleared (then the
/// bank has the money, and what happened is a bounce, recorded on the instrument).
pub async fn payment_cancelled(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
    let Some(iid) = id(doc, "payment_instrument") else { return Ok(()) };
    let inst = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, ENTITY, iid, true).await?;
    match s(&inst, "status") {
        Some("Cleared") => Err(Error::invalid("its instrument has cleared: record the instrument's bounce instead of cancelling the payment")),
        Some("Received" | "Issued" | "Deposited") => {
            let mut v = Doc::new();
            v.insert("status".into(), json!("Cancelled"));
            v.insert("reason".into(), json!("The payment was cancelled"));
            hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, ENTITY, iid, v).await
        }
        _ => Ok(()),
    }
}

pub struct InstrumentHook;

#[async_trait::async_trait]
impl DocHook for InstrumentHook {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if !matches!(name, "deposit" | "clear" | "bounce" | "cancel_instrument" | "replace") {
            return Ok(None);
        }
        let iid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let sctx = sys(hc.ctx);
        // Locked, and its state read again under the lock: a second click waits and is then refused.
        let inst = hc.kernel.get_tx(&sctx, hc.tx, ENTITY, iid, true).await?;
        let status = s(&inst, "status").unwrap_or_default().to_string();
        let on = match params.get("on").and_then(Value::as_str).filter(|v| !v.is_empty()) {
            Some(v) => ferrum_kernel::value::parse_date(v).map_err(|_| Error::field("on", "is not a date"))?,
            None => hc.ctx.today(),
        };
        let reason = params.get("reason").and_then(Value::as_str).map(str::trim).filter(|r| !r.is_empty()).map(str::to_string);
        let allowed = match name {
            "deposit" => status == "Received",
            "clear" | "cancel_instrument" | "replace" => matches!(status.as_str(), "Received" | "Deposited" | "Issued"),
            _ => matches!(status.as_str(), "Received" | "Deposited" | "Issued" | "Cleared"),
        };
        if !allowed {
            return Err(Error::invalid(format!(
                "the instrument is {}: it cannot be {}",
                status.to_lowercase(),
                name.replace("_instrument", "led").replace('_', " ")
            )));
        }
        let pid = id(&inst, "payment_entry").ok_or_else(|| Error::internal("an instrument without its payment"))?;
        let mut v = Doc::new();
        match name {
            "deposit" => {
                v.insert("status".into(), json!("Deposited"));
                v.insert("deposited_on".into(), json!(on));
            }
            "clear" => {
                let dated = date(&inst, "instrument_date")?;
                if on < dated {
                    return Err(Error::field("on", format!("the instrument is dated {dated}: it cannot clear before")));
                }
                clear(hc.kernel, hc.ctx, hc.tx, &inst, pid, on).await?;
                v.insert("status".into(), json!("Cleared"));
                v.insert("cleared_on".into(), json!(on));
            }
            "bounce" | "cancel_instrument" => {
                if reason.is_none() {
                    return Err(Error::field("reason", "say why"));
                }
                let bounced = name == "bounce";
                let co = company(hc.kernel, hc.ctx, hc.tx, id(&inst, "company").unwrap_or_default()).await?;
                if status == "Cleared" {
                    ledger::reverse_on(hc.tx, hc.ctx, ENTITY, iid, on, co.precision).await?;
                }
                v.insert("status".into(), json!(if bounced { "Bounced" } else { "Cancelled" }));
                v.insert("reason".into(), json!(reason));
                if bounced {
                    v.insert("bounced_on".into(), json!(on));
                }
                // Marked first, so the payment's own cancel below finds nothing left to refuse.
                hc.kernel.set_values_tx(&sctx, hc.tx, ENTITY, iid, std::mem::take(&mut v)).await?;
                let payment = hc.kernel.get_tx(&sctx, hc.tx, "PaymentEntry", pid, true).await?;
                crate::accounting::undo_payment(hc.kernel, hc.ctx, hc.tx, &payment, on).await?;
                hc.kernel.cancel_tx(&sctx, hc.tx, "PaymentEntry", pid, None).await?;
                if bounced {
                    let wanted = !params.get("no_charge").and_then(Value::as_bool).unwrap_or(false);
                    let charge = if wanted { charge(hc, &inst, &payment, on, co.id).await? } else { None };
                    if let Some(je) = charge {
                        v.insert("charge_entry".into(), json!(je));
                    }
                    let payload = json!({"instrument": iid, "payment": pid, "direction": s(&inst, "direction"), "amount": s(&inst, "amount"),
                                         "reason": reason, "charge_entry": charge});
                    ferrum_kernel::outbox::enqueue(hc.tx, hc.ctx.tenant, "accounting.instrument_bounced", Some(ENTITY), Some(iid), payload).await?;
                }
            }
            _ => {
                let number = params.get("instrument_no").and_then(Value::as_str).map(str::trim).filter(|n| !n.is_empty());
                let dated = params.get("instrument_date").and_then(Value::as_str).filter(|n| !n.is_empty());
                let (Some(number), Some(dated)) = (number, dated) else {
                    return Err(Error::field("instrument_no", "give the new instrument's number and date"));
                };
                let mut fresh = inst.clone();
                for f in ["id", "name", "created_at", "created_by", "modified_at", "modified_by", "version", "deposited_on", "reminded_on", "reason"] {
                    fresh.remove(f);
                }
                fresh.insert("instrument_no".into(), json!(number));
                fresh.insert("instrument_date".into(), json!(dated));
                fresh.insert(
                    "bank".into(),
                    params.get("bank").cloned().filter(|b| !b.is_null()).unwrap_or_else(|| inst.get("bank").cloned().unwrap_or(Value::Null)),
                );
                fresh.insert("status".into(), json!(if s(&inst, "direction") == Some("Issued") { "Issued" } else { "Received" }));
                fresh.insert("replaces".into(), json!(iid));
                let made = hc.kernel.insert_tx(&sctx, hc.tx, ENTITY, fresh).await?;
                v.insert("status".into(), json!("Replaced"));
                v.insert("replaced_by".into(), made["id"].clone());
                v.insert("reason".into(), json!(reason));
                let mut p = Doc::new();
                p.insert("payment_instrument".into(), made["id"].clone());
                hc.kernel.set_values_tx(&sctx, hc.tx, "PaymentEntry", pid, p).await?;
            }
        }
        if !v.is_empty() {
            hc.kernel.set_values_tx(&sctx, hc.tx, ENTITY, iid, v).await?;
        }
        Ok(Some(Value::Object(hc.kernel.get_tx(&sctx, hc.tx, ENTITY, iid, false).await?)))
    }
}

/// Moves the instrument out of the instruments account into the bank account the payment named,
/// at the payment's amounts and rate.
async fn clear(k: &Kernel, ctx: &Ctx, tx: &mut Tx, inst: &Doc, pid: Uuid, on: NaiveDate) -> Result<()> {
    let payment = k.get_tx(&sys(ctx), tx, "PaymentEntry", pid, false).await?;
    let co = company(k, ctx, tx, id(&payment, "company").unwrap_or_default()).await?;
    let receive = s(inst, "direction") != Some("Issued");
    let holding = id(inst, "holding_account").ok_or_else(|| Error::internal("an instrument without the account it waits in"))?;
    let bank = id(&payment, "bank_account").ok_or_else(|| Error::field("bank_account", "is required"))?;
    let (code, _) = crate::util::currency(k, ctx, tx, id(&payment, "currency").unwrap_or(co.currency_id)).await?;
    let (paid, base, rate) = (d(&payment, "paid_amount"), d(&payment, "base_paid_amount"), d(&payment, "exchange_rate"));
    let sign = if receive { Decimal::ONE } else { -Decimal::ONE };
    let iid = id(inst, "id").unwrap_or_default();
    let mut entry = Entry::new(co.id, on, ENTITY, iid, s(inst, "instrument_no").map(str::to_string));
    entry.memo = Some(format!("Instrument {} cleared", s(inst, "instrument_no").unwrap_or_default()));
    entry.lines.push(Line::new(bank, sign * base, &co.currency_code).txn(sign * paid, &code, rate));
    entry.lines.push(Line::new(holding, -sign * base, &co.currency_code).txn(-sign * paid, &code, rate));
    ledger::post(tx, ctx, entry, co.precision).await?;
    Ok(())
}

/// The customer's charge for a bounce: the mode's amount, to its bounce charge account, owed by the
/// customer as a journal entry's open line. None when the mode has no charge or it was not a customer's.
async fn charge(hc: &mut HookCtx<'_>, inst: &Doc, payment: &Doc, on: NaiveDate, company: Uuid) -> Result<Option<Uuid>> {
    let Some(customer) = id(payment, "customer").filter(|_| s(inst, "direction") == Some("Received")) else { return Ok(None) };
    let mode = get(hc.kernel, hc.ctx, hc.tx, "ModeOfPayment", id(inst, "mode_of_payment").unwrap_or_default()).await?;
    let amount = d(&mode, "bounce_charge");
    if amount <= Decimal::ZERO {
        return Ok(None);
    }
    let row = rows(&mode, "accounts").into_iter().find(|r| id(r, "company") == Some(company)).unwrap_or_default();
    let income = id(&row, "bounce_charge_account")
        .ok_or_else(|| Error::invalid(format!("{} has no bounce charge account for this company", s(&mode, "mode_name").unwrap_or("the mode"))))?;
    let receivable = id(payment, "party_account").ok_or_else(|| Error::field("party_account", "is required"))?;
    let number = s(inst, "instrument_no").unwrap_or_default();
    let sctx = sys(hc.ctx);
    let je = hc
        .kernel
        .insert_tx(
            &sctx,
            hc.tx,
            "JournalEntry",
            obj(json!({"company": company, "posting_date": on, "remark": format!("Charge for the bounced instrument {number}"),
                "accounts": [
                    {"account": receivable, "party_type": "Customer", "customer": customer, "debit": money(amount), "reference_no": number},
                    {"account": income, "credit": money(amount)}]})),
        )
        .await?;
    let jid = id(&je, "id").ok_or_else(|| Error::internal("no id"))?;
    hc.kernel.submit_tx(&sctx, hc.tx, "JournalEntry", jid, None).await?;
    Ok(Some(jid))
}

/// The app's upkeep: tells the Accounts Managers, once, that a received instrument's date has come
/// and it can be deposited (`accounting.instrument_due`).
pub async fn upkeep(k: &Kernel, ctx: &Ctx, today: NaiveDate) -> Result<usize> {
    let Some(ent) = ctx.schema.entity(ENTITY) else { return Ok(0) };
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let due: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select id from {} where tenant_id = $1 and status = 'Received' and reminded_on is null and instrument_date <= $2 for update skip locked",
        ferrum_kernel::ddl::q(&ent.table)
    )))
    .bind(ctx.tenant)
    .bind(today)
    .fetch_all(&mut *tx)
    .await?;
    let sctx = sys(ctx);
    for iid in &due {
        let mut v = Doc::new();
        v.insert("reminded_on".into(), json!(today));
        k.set_values_tx(&sctx, &mut tx, ENTITY, *iid, v).await?;
        let doc = Value::Object(k.get_tx(&sctx, &mut tx, ENTITY, *iid, false).await?);
        let to = ferrum_kernel::messages::recipients(k, &mut tx, &sctx, ENTITY, &doc, "role:Accounts Manager").await?;
        let m = ferrum_kernel::messages::Message {
            template: "accounting.instrument_due".into(),
            entity: Some(ENTITY.into()),
            record: Some(*iid),
            to,
            ..Default::default()
        };
        ferrum_kernel::messages::send(k, &mut tx, &sctx, &m).await?;
    }
    tx.commit().await?;
    Ok(due.len())
}
