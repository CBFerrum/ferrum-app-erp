//! Bad-debt write-off (FN-13) as one action: a customer's open invoice is settled by the write-off
//! (an allocation, as a payment's), booked to the account its reason names for the company, less the
//! tax relief the company's country gives (`localization::bad_debt_relief`; none by default). A
//! recovery later is money in, credited to the reason's recovery account, with the relief's share
//! owed again. Approval is the workspace's policy on submitting a write-off, not code.

use crate::flow::sync_invoice_outstanding;
use crate::ledger::{self, Entry, Line};
use crate::util::{company, d, date, get, id, money, round, rows, s, set, sys};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, Error, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};

const ENTITY: &str = "BadDebtWriteOff";

pub struct WriteOffHook;

#[async_trait]
impl DocHook for WriteOffHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if let Some(c) = id(doc, "customer") {
            let cust = get(hc.kernel, hc.ctx, hc.tx, "Customer", c).await?;
            set(doc, "customer_name", s(&cust, "customer_name").unwrap_or_default().to_string());
        }
        let Some(inv) = id(doc, "invoice") else { return Ok(()) };
        let Some(item) = ledger::load_open_item(hc.tx, hc.ctx.tenant, inv, false).await? else { return Ok(()) };
        set(doc, "invoice_type", hc.ctx.schema.entity(&item.entity).map(|e| e.def.label.clone()).unwrap_or(item.entity.clone()));
        set(doc, "invoice_number", item.number.clone().map(Value::String).unwrap_or(Value::Null));
        set(doc, "currency", item.currency.clone());
        set(doc, "outstanding", money(item.outstanding));
        if d(doc, "amount").is_zero() {
            set(doc, "amount", money(item.outstanding));
        }
        if let (Some(cid), Some(reason)) = (id(doc, "company"), id(doc, "reason")) {
            let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
            set(doc, "base_amount", money(round(d(doc, "amount") * item.exchange_rate, co.precision)));
            let r = get(hc.kernel, hc.ctx, hc.tx, "WriteOffReason", reason).await?;
            let row = rows(&r, "accounts").into_iter().find(|a| id(a, "company") == Some(cid));
            set(doc, "write_off_account", row.and_then(|a| id(&a, "account")).map(|a| json!(a.to_string())).unwrap_or(Value::Null));
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let inv = id(doc, "invoice").ok_or_else(|| Error::field("invoice", "choose the open invoice written off"))?;
        let item = ledger::load_open_item(hc.tx, hc.ctx.tenant, inv, false).await?.ok_or_else(|| Error::field("invoice", "is not open"))?;
        if item.customer_id.is_none() || item.customer_id != id(doc, "customer") {
            return Err(Error::field("invoice", "is not this customer's"));
        }
        let amount = d(doc, "amount");
        if amount <= Decimal::ZERO || amount > item.outstanding {
            return Err(Error::field("amount", format!("between nothing and the {} outstanding", item.outstanding.normalize())));
        }
        if id(doc, "write_off_account").is_none() {
            return Err(Error::field("reason", "this reason has no account for the company: add one to the reason"));
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let wid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let on = date(doc, "posting_date")?;
        let inv = id(doc, "invoice").unwrap_or_default();
        // Locked: a payment and a write-off of the same invoice at once settle it once.
        let item = ledger::load_open_item(hc.tx, hc.ctx.tenant, inv, true).await?.ok_or_else(|| Error::field("invoice", "is not open"))?;
        let amount = d(doc, "amount");
        let base = round(amount * item.exchange_rate, co.precision);
        ledger::allocate(hc.tx, hc.ctx, co.id, inv, ENTITY, wid, on, amount, base).await?;
        let invoice = match hc.ctx.schema.entity(&item.entity) {
            Some(_) if item.entity != "JournalEntry" => get(hc.kernel, hc.ctx, hc.tx, &item.entity, inv).await.unwrap_or_default(),
            _ => Doc::new(),
        };
        let relief = crate::localization::bad_debt_relief(hc.kernel, hc.ctx, hc.tx, co.id, &item.entity, &invoice, base).await?;
        let relieved: Decimal = relief.iter().map(|r| r.amount).sum();
        if relieved > base {
            return Err(Error::internal("tax relief over the amount written off"));
        }
        let mut entry = Entry::new(co.id, on, ENTITY, wid, s(doc, "number").map(str::to_string));
        entry.memo = Some(format!("Bad debt written off: {}", item.number.clone().unwrap_or_default()));
        entry.lines.push(
            Line::new(item.account_id, -base, &co.currency_code)
                .txn(-amount, &item.currency, item.exchange_rate)
                .customer(item.customer_id)
                .against(&item.entity, inv),
        );
        for r in &relief {
            entry.lines.push(Line::new(r.account, r.amount, &co.currency_code).remarks(r.description.clone()));
        }
        entry.lines.push(Line::new(id(doc, "write_off_account").unwrap_or_default(), base - relieved, &co.currency_code).remarks("Bad debt"));
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
        let rows: Vec<Value> = relief.iter().map(|r| json!({"account": r.account, "amount": money(r.amount), "description": r.description})).collect();
        hc.kernel.set_child_rows_tx(&sys(hc.ctx), hc.tx, ENTITY, "relief", wid, rows.into_iter().filter_map(|v| v.as_object().cloned()).collect()).await?;
        sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, &item.entity, inv).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if d(doc, "recovered_amount") > Decimal::ZERO {
            return Err(Error::invalid("money was recovered on this write-off: it stands"));
        }
        let wid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, _) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        let items = ledger::unallocate(hc.tx, hc.ctx, ENTITY, wid, on).await?;
        ledger::reverse(hc.tx, hc.ctx, ENTITY, wid, on, co.precision).await?;
        for inv in items {
            if let Some(item) = ledger::load_open_item(hc.tx, hc.ctx.tenant, inv, false).await? {
                sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, &item.entity, inv).await?;
            }
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "record_recovery" {
            return Ok(None);
        }
        let wid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let sctx = sys(hc.ctx);
        // Locked: two recoveries at once are added up, never over what was written off.
        let w = hc.kernel.get_tx(&sctx, hc.tx, ENTITY, wid, true).await?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(&w, "company").unwrap_or_default()).await?;
        let amount = round(
            params
                .get("amount")
                .and_then(|v| v.as_str().map(str::to_string).or(v.as_f64().map(|f| f.to_string())))
                .and_then(|v| v.parse::<Decimal>().ok())
                .unwrap_or_default(),
            co.precision,
        );
        let (written, recovered) = (d(&w, "base_amount"), d(&w, "recovered_amount"));
        if amount <= Decimal::ZERO || amount + recovered > written {
            return Err(Error::field("amount", format!("between nothing and the {} not yet recovered", (written - recovered).normalize())));
        }
        let bank =
            params.get("bank_account").and_then(Value::as_str).and_then(|v| v.parse().ok()).ok_or_else(|| Error::field("bank_account", "is required"))?;
        let b = get(hc.kernel, hc.ctx, hc.tx, "Account", bank).await?;
        if id(&b, "company") != Some(co.id) || !matches!(s(&b, "account_type"), Some("Bank" | "Cash")) {
            return Err(Error::field("bank_account", "choose a Bank or Cash account of this company"));
        }
        let on = match params.get("on").and_then(Value::as_str).filter(|v| !v.is_empty()) {
            Some(v) => ferrum_kernel::value::parse_date(v).map_err(|_| Error::field("on", "is not a date"))?,
            None => hc.ctx.today(),
        };
        let reason = get(hc.kernel, hc.ctx, hc.tx, "WriteOffReason", id(&w, "reason").unwrap_or_default()).await?;
        let row = rows(&reason, "accounts").into_iter().find(|a| id(a, "company") == Some(co.id)).unwrap_or_default();
        let income = id(&row, "recovery_account").or(id(&w, "write_off_account")).ok_or_else(|| Error::field("reason", "has no account for the company"))?;
        let mut entry = Entry::new(co.id, on, ENTITY, wid, s(&w, "number").map(|n| format!("{n} (recovery)")));
        entry.memo = Some("Bad debt recovered".into());
        entry.lines.push(Line::new(bank, amount, &co.currency_code));
        // The relief taken is owed again in the share recovered.
        let mut owed = Decimal::ZERO;
        for r in rows(&w, "relief") {
            let (Some(acc), part) = (id(&r, "account"), round(d(&r, "amount") * amount / written, co.precision)) else { continue };
            owed += part;
            entry.lines.push(Line::new(acc, -part, &co.currency_code).remarks(s(&r, "description").unwrap_or("Tax relief").to_string()));
        }
        entry.lines.push(Line::new(income, -(amount - owed), &co.currency_code).remarks("Bad debt recovered"));
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
        let mut v = Doc::new();
        v.insert("recovered_amount".into(), money(recovered + amount));
        hc.kernel.set_values_tx(&sctx, hc.tx, ENTITY, wid, v).await?;
        Ok(Some(Value::Object(hc.kernel.get_tx(&sctx, hc.tx, ENTITY, wid, false).await?)))
    }
}
