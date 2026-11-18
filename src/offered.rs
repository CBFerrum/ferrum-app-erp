//! What ERP offers other apps through the gate (the platform's docs/app-contract.md, *Using another app's
//! operations*), so they call an operation with a contract instead of compiling against ERP's code: post to the
//! ledger and reverse what was posted for a record of theirs (`accounting.company.post_entry`,
//! `accounting.company.reverse_entries`), find or make a person's customer (`selling.company.customer_for`), the
//! customers an address belongs to (`selling.company.customers_of`), and price a document as a sales or purchase
//! order would be (`selling.company.price_document`). docs/contracts.md lists their inputs.
//!
//! Posting is an app's, never a person's: an app's call runs as the system (`Kernel::call_tx`), and a person or an
//! agent asking directly is refused; people post through journal entries. A key the app gives makes the call happen
//! once (the gate's receipt, in the caller's transaction).

use crate::ledger::{self, Entry, Line};
use crate::pricing::{Side, calculate};
use crate::util::{company, id, s, sys};
use ferrum_kernel::{Action, Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

/// Lines in one posting, and lines in one priced document, at most.
const MAX_LINES: usize = 500;

fn text<'a>(params: &'a Value, k: &str) -> Option<&'a str> {
    params.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

fn uuid_of(params: &Value, k: &str) -> Result<Option<Uuid>> {
    match text(params, k) {
        None => Ok(None),
        Some(v) => v.parse().map(Some).map_err(|_| Error::field(k, "is not an id")),
    }
}

fn date_of(params: &Value, k: &str) -> Result<Option<chrono::NaiveDate>> {
    match text(params, k) {
        None => Ok(None),
        Some(v) => ferrum_kernel::value::parse_date(v).map(Some).map_err(|m| Error::field(k, m)),
    }
}

/// A list given as JSON text (or, from code, as a list).
fn list(params: &Value, k: &str) -> Result<Vec<Value>> {
    let v = match params.get(k) {
        Some(Value::String(t)) => serde_json::from_str(t).map_err(|e| Error::field(k, format!("is not JSON: {e}")))?,
        Some(v) => v.clone(),
        None => Value::Null,
    };
    match v {
        Value::Array(a) if !a.is_empty() && a.len() <= MAX_LINES => Ok(a),
        _ => Err(Error::field(k, format!("give 1 to {MAX_LINES} lines"))),
    }
}

fn amount(row: &Value, k: &str, i: usize) -> Result<Decimal> {
    match row.get(k) {
        None | Some(Value::Null) => Ok(Decimal::ZERO),
        Some(v) => ferrum_kernel::value::parse_decimal(v).map_err(|m| Error::Validation(vec![FieldError::new(k, m).in_row("lines", i)])),
    }
}

/// The record a posting is for: an entity this workspace has, and a record of it that exists.
async fn source(hc: &mut HookCtx<'_>, params: &Value) -> Result<(String, Uuid)> {
    let entity = text(params, "source_entity").ok_or_else(|| Error::field("source_entity", "name the entity of the record it is for"))?.to_string();
    if hc.ctx.schema.entity(&entity).is_none() {
        return Err(Error::field("source_entity", format!("{entity} is not an entity of this workspace")));
    }
    let source = uuid_of(params, "source_id")?.ok_or_else(|| Error::field("source_id", "give the record's id"))?;
    match hc.kernel.get_tx(&sys(hc.ctx), hc.tx, &entity, source, false).await {
        Ok(_) => Ok((entity, source)),
        Err(Error::NotFound(_)) => Err(Error::field("source_id", format!("no {entity} has this id"))),
        Err(e) => Err(e),
    }
}

/// Posting is for an app's call (which runs as the system) and ERP's own code.
fn only_apps(hc: &HookCtx<'_>) -> Result<()> {
    if hc.ctx.is_system { Ok(()) } else { Err(Error::Forbidden("an app posts to the ledger for its own records; people post journal entries".into())) }
}

/// `post_entry {posting_date, source_entity, source_id, source_number?, memo?, lines}`: one balanced entry for the
/// app's record. Each line: `account`, `debit` or `credit` in the company's currency, and as needed `currency`,
/// `amount_txn` (signed, debit positive) and `exchange_rate` for another currency, `customer`, `supplier`,
/// `cost_center`, `project`, `against_entity` with `against_id`, `remarks`.
async fn post_entry(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    only_apps(hc)?;
    let cid = id(doc, "id").ok_or_else(|| Error::internal("company without id"))?;
    let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
    let on = date_of(params, "posting_date")?.ok_or_else(|| Error::field("posting_date", "is required"))?;
    let (entity, source_id) = source(hc, params).await?;
    let rows = list(params, "lines")?;
    let mut lines = Vec::with_capacity(rows.len());
    let (mut debit, mut credit) = (Decimal::ZERO, Decimal::ZERO);
    for (i, row) in rows.iter().enumerate() {
        let bad = |k: &str, m: &str| Error::Validation(vec![FieldError::new(k, m).in_row("lines", i)]);
        let link = |k: &str| match row.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()) {
            None => Ok(None),
            Some(v) => v.parse::<Uuid>().map(Some).map_err(|_| bad(k, "is not an id")),
        };
        let account = link("account")?.ok_or_else(|| bad("account", "is required"))?;
        let ok: bool =
            sqlx::query_scalar("select exists (select 1 from e_account where tenant_id = $1 and id = $2 and company = $3 and not coalesce(is_group, false))")
                .bind(hc.ctx.tenant)
                .bind(account)
                .bind(cid)
                .fetch_one(&mut **hc.tx)
                .await?;
        if !ok {
            return Err(bad("account", "is not a ledger account of this company"));
        }
        let (dr, cr) = (amount(row, "debit", i)?, amount(row, "credit", i)?);
        if dr < Decimal::ZERO || cr < Decimal::ZERO || (!dr.is_zero() && !cr.is_zero()) {
            return Err(bad("debit", "give a debit or a credit, not below zero"));
        }
        if dr.round_dp(co.precision) != dr || cr.round_dp(co.precision) != cr {
            return Err(bad("debit", &format!("has more than the company currency's {} decimals", co.precision)));
        }
        debit += dr;
        credit += cr;
        let mut l = Line::new(account, dr - cr, &co.currency_code);
        if let Some(cur) = row.get("currency").and_then(Value::as_str).filter(|c| !c.is_empty() && *c != co.currency_code) {
            let txn = amount(row, "amount_txn", i)?;
            let rate = match row.get("exchange_rate") {
                Some(v) if !v.is_null() => ferrum_kernel::value::parse_decimal(v).map_err(|m| bad("exchange_rate", &m))?,
                _ => return Err(bad("exchange_rate", "is required with another currency")),
            };
            if txn.is_zero() || (txn > Decimal::ZERO) != (dr > cr) {
                return Err(bad("amount_txn", "is the line's amount in its currency, with the same sign"));
            }
            l = l.txn(txn, cur, rate);
        }
        l = l.customer(link("customer")?).supplier(link("supplier")?).cost_center(link("cost_center")?).project(link("project")?);
        if let (Some(e), Some(a)) = (row.get("against_entity").and_then(Value::as_str).filter(|e| !e.is_empty()), link("against_id")?) {
            l = l.against(e, a);
        }
        if let Some(r) = row.get("remarks").and_then(Value::as_str).filter(|r| !r.is_empty()) {
            l = l.remarks(r.chars().take(1000).collect::<String>());
        }
        lines.push(l);
    }
    if debit != credit {
        return Err(Error::field("lines", format!("debits {debit} and credits {credit} differ: an entry balances")));
    }
    let entry = Entry {
        memo: text(params, "memo").map(|m| m.chars().take(1000).collect()),
        lines,
        ..Entry::new(cid, on, &entity, source_id, text(params, "source_number").map(str::to_string))
    };
    let posted = ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
    Ok(json!({"entry": posted, "debit": debit.to_string(), "credit": credit.to_string()}))
}

/// `reverse_entries {source_entity, source_id, on?}`: every entry posted for the record and not reversed yet,
/// reversed on `on` (today when empty), once.
async fn reverse_entries(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    only_apps(hc)?;
    let cid = id(doc, "id").ok_or_else(|| Error::internal("company without id"))?;
    let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
    let (entity, source_id) = source(hc, params).await?;
    let on = date_of(params, "on")?.unwrap_or_else(|| hc.ctx.today());
    let reversed = ledger::reverse_on(hc.tx, hc.ctx, &entity, source_id, on, co.precision).await?;
    Ok(json!({"reversed": reversed}))
}

/// The company's operations of the accounting package.
pub struct LedgerOps;

#[async_trait::async_trait]
impl DocHook for LedgerOps {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        match name {
            "post_entry" => post_entry(hc, doc, params).await.map(Some),
            "reverse_entries" => reverse_entries(hc, doc, params).await.map(Some),
            _ => Ok(None),
        }
    }
}

/// The company's operations of the selling package that other apps use.
pub struct PartyOps;

#[async_trait::async_trait]
impl DocHook for PartyOps {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let out = match name {
            "customer_for" => {
                if !hc.ctx.is_system {
                    hc.ctx.require("Customer", Action::Create)?;
                }
                let name = text(params, "name").ok_or_else(|| Error::field("name", "is required"))?;
                let name: String = name.chars().take(140).collect();
                let (customer, created) =
                    crate::portal::customer_for(hc.kernel, &sys(hc.ctx), hc.tx, &name, text(params, "email"), text(params, "phone")).await?;
                json!({"customer": customer, "created": created})
            }
            "customers_of" => {
                if !hc.ctx.is_system {
                    hc.ctx.require("Customer", Action::Read)?;
                }
                let email = text(params, "email").ok_or_else(|| Error::field("email", "is required"))?;
                json!({"customers": crate::portal::customers_of(hc.kernel, &sys(hc.ctx), hc.tx, email).await?})
            }
            "price_document" => price_document(hc, doc, params).await?,
            _ => return Ok(None),
        };
        Ok(Some(out))
    }
}

/// `price_document {document, side?}`: the document (a sales order's fields, or a purchase order's on the buying
/// side) with its prices, discounts, taxes and totals, as an order of the company would have them; nothing is saved.
async fn price_document(hc: &mut HookCtx<'_>, company_doc: &Doc, params: &Value) -> Result<Value> {
    let side = match text(params, "side") {
        None | Some("Selling") => Side::Selling,
        Some("Buying") => Side::Buying,
        Some(_) => return Err(Error::field("side", "Selling or Buying")),
    };
    let mut doc: Doc = match params.get("document") {
        Some(Value::String(t)) => serde_json::from_str(t).map_err(|e| Error::field("document", format!("is not a JSON object: {e}")))?,
        Some(Value::Object(m)) => m.clone(),
        _ => return Err(Error::field("document", "give the document as a JSON object")),
    };
    let items = doc.get("items").and_then(Value::as_array).map_or(0, Vec::len);
    if items == 0 || items > MAX_LINES {
        return Err(Error::field("document", format!("give it 1 to {MAX_LINES} items")));
    }
    let entity = if side == Side::Selling { "SalesOrder" } else { "PurchaseOrder" };
    let ent = hc.ctx.schema.entity(entity).cloned().ok_or_else(|| Error::invalid(format!("{entity} is not installed here")))?;
    // Only what a new order would carry: no id, so nothing is read back from a stored one.
    doc.remove("id");
    doc.insert("company".into(), json!(id(company_doc, "id").unwrap_or_default().to_string()));
    if s(&doc, "posting_date").is_none() {
        doc.insert("posting_date".into(), json!(hc.ctx.today().to_string()));
    }
    calculate(hc.kernel, hc.ctx, hc.tx, &ent, &mut doc, side).await?;
    Ok(json!({"document": doc}))
}
