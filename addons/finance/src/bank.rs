//! Bank accounts, statement import and reconciliation.
//!
//! A bank account ties a company's account at a bank to its GL account. Statements are imported
//! from CSV with a column mapping; every line becomes a submitted bank transaction carrying a hash
//! of its content, so importing the same (or an overlapping) statement again skips what is there.
//! Reconciling allocates a transaction to payment entries and journal entries on the bank's GL
//! account - several at once, partly if need be - never beyond what either side has left; a
//! voucher cleared in full gets its clearance date. An unmatched line becomes a new payment or
//! journal entry, reconciled in the same step.

use crate::common::{installed, opt_date};
use crate::reports::bank_statement;
use crate::rules;
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_domain::reports::Params;
use ferrum_domain::util::{Company, b, company, currency, d, elevated, get, id, money, rows, s, set, sys};
use ferrum_kernel::{Action, Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use rust_decimal::prelude::Signed;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

const MAX_IMPORT_ROWS: usize = 10_000;
const MAX_IMPORT_BYTES: usize = 5 << 20;

fn ever() -> (NaiveDate, NaiveDate) {
    (NaiveDate::from_ymd_opt(1900, 1, 1).unwrap_or_default(), NaiveDate::from_ymd_opt(9999, 12, 31).unwrap_or_default())
}

pub struct BankAccountHook;

#[async_trait]
impl DocHook for BankAccountHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if let Some(iban) = s(doc, "iban") {
            let clean: String = iban.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_uppercase();
            set(doc, "iban", clean);
        }
        let number = s(doc, "iban").or(s(doc, "account_number")).unwrap_or_default().to_string();
        set(doc, "masked_number", rules::mask(&number));
        if id(doc, "currency").is_none()
            && let Some(cid) = id(doc, "company")
        {
            let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
            let gl_currency = match id(doc, "account") {
                Some(a) => id(&get(hc.kernel, hc.ctx, hc.tx, "Account", a).await?, "account_currency"),
                None => None,
            };
            set(doc, "currency", gl_currency.unwrap_or(co.currency_id).to_string());
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        if let Some(a) = id(doc, "account") {
            let acc = get(hc.kernel, hc.ctx, hc.tx, "Account", a).await?;
            if id(&acc, "company") != id(doc, "company") {
                errors.push(FieldError::new("account", "belongs to another company"));
            } else if s(&acc, "account_type") != Some("Bank") {
                errors.push(FieldError::new("account", "choose a GL account of type Bank"));
            }
        }
        if let Some(iban) = s(doc, "iban")
            && !rules::iban_valid(iban)
        {
            errors.push(FieldError::new("iban", "is not a valid IBAN (check the digits)"));
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, _old: Option<&Doc>) -> Result<()> {
        if b(doc, "is_default") {
            // One default bank account per company.
            sqlx::query("update e_bank_account set is_default = false, updated_at = now() where tenant_id = $1 and company = $2 and id <> $3 and is_default")
                .bind(hc.ctx.tenant)
                .bind(id(doc, "company"))
                .bind(id(doc, "id"))
                .execute(&mut **hc.tx)
                .await?;
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        match name {
            "import_transactions" => import(hc, doc, params).await.map(Some),
            "workbench" => workbench(hc, doc, params).await.map(Some),
            "accept_matches" => accept_matches(hc, params).await.map(Some),
            _ => Ok(None),
        }
    }
}

/// One parsed statement line.
struct Line {
    date: NaiveDate,
    deposit: Decimal,
    withdrawal: Decimal,
    description: String,
    reference: String,
}

/// The 0-based column a mapping entry names: a header (any case) or a 1-based column number.
fn column(header: &[String], key: &str, mapping: &Map<String, Value>) -> Result<Option<usize>> {
    let Some(v) = mapping.get(key).filter(|v| !v.is_null()) else { return Ok(None) };
    let by_number = match v {
        Value::Number(n) => n.as_u64().map(|i| i as usize),
        Value::String(t) => t.trim().parse::<usize>().ok(),
        _ => None,
    };
    if let Some(i) = by_number {
        return if (1..=header.len()).contains(&i) {
            Ok(Some(i - 1))
        } else {
            Err(Error::field("mapping", format!("{key}: column {i} does not exist (the statement has {})", header.len())))
        };
    }
    let name = v.as_str().unwrap_or_default().trim();
    header
        .iter()
        .position(|h| h.trim().eq_ignore_ascii_case(name))
        .map(Some)
        .ok_or_else(|| Error::field("mapping", format!("the statement has no column '{name}' (mapped to {key})")))
}

/// `import_transactions {csv, mapping: {date, amount | deposit/withdrawal, description?, reference?},
/// date_format?, decimal_comma?, delimiter?, header_row?, dry_run?}`. Amount columns are signed
/// (positive is a deposit) unless deposits and withdrawals have columns of their own. All or
/// nothing: any bad line refuses the whole statement, listing the lines to fix. A dry run writes
/// nothing: it answers what an import would do (new lines, duplicates, the first lines as read),
/// and lists bad lines instead of refusing, so a screen can show them next to the mapping.
async fn import(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    hc.ctx.require("BankTransaction", Action::Create)?;
    hc.ctx.require("BankTransaction", Action::Submit)?;
    if b(doc, "disabled") {
        return Err(Error::invalid("this bank account is disabled"));
    }
    let bank = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let text =
        params.get("csv").and_then(Value::as_str).filter(|t| !t.trim().is_empty()).ok_or_else(|| Error::field("csv", "paste the statement's CSV text"))?;
    if text.len() > MAX_IMPORT_BYTES {
        return Err(Error::field("csv", "the statement is larger than 5 MB; split it"));
    }
    let mapping = params.get("mapping").and_then(Value::as_object).ok_or_else(|| Error::field("mapping", "map the statement's columns"))?;
    let delimiter = params.get("delimiter").and_then(Value::as_str).and_then(|t| t.chars().next()).unwrap_or_else(|| rules::detect_delimiter(text));
    let date_format = params.get("date_format").and_then(Value::as_str);
    let decimal_comma = params.get("decimal_comma").and_then(Value::as_bool).unwrap_or(false);
    let dry_run = params.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let header_row = params.get("header_row").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
    let records = rules::parse_csv(text, delimiter);
    if records.len() < header_row {
        return Err(Error::field("csv", "the statement has no header row"));
    }
    let header = &records[header_row - 1];
    let data = &records[header_row..];
    if data.len() > MAX_IMPORT_ROWS {
        return Err(Error::field("csv", format!("at most {MAX_IMPORT_ROWS} lines per import")));
    }
    let col_date = column(header, "date", mapping)?.ok_or_else(|| Error::field("mapping", "map the date column"))?;
    let col_amount = column(header, "amount", mapping)?;
    let col_deposit = column(header, "deposit", mapping)?;
    let col_withdrawal = column(header, "withdrawal", mapping)?;
    if col_amount.is_none() && col_deposit.is_none() && col_withdrawal.is_none() {
        return Err(Error::field("mapping", "map an amount column, or deposit and withdrawal columns"));
    }
    let col_description = column(header, "description", mapping)?;
    let col_reference = column(header, "reference", mapping)?;
    let cell = |r: &Vec<String>, c: Option<usize>| c.and_then(|i| r.get(i)).map(|v| v.trim().to_string()).unwrap_or_default();

    let mut lines = Vec::new();
    let mut errors = Vec::new();
    let mut skipped = 0usize;
    for (i, r) in data.iter().enumerate() {
        let line_no = header_row + i + 1;
        let amount = |c: Option<usize>| rules::parse_amount(&cell(r, c), decimal_comma);
        let parsed = (|| -> std::result::Result<Option<(Decimal, Decimal)>, String> {
            let (dep, wd) = match col_amount {
                Some(_) => match amount(col_amount)? {
                    Some(a) if a > Decimal::ZERO => (a, Decimal::ZERO),
                    Some(a) => (Decimal::ZERO, -a),
                    None => (Decimal::ZERO, Decimal::ZERO),
                },
                None => (amount(col_deposit)?.unwrap_or_default().abs(), amount(col_withdrawal)?.unwrap_or_default().abs()),
            };
            if dep.is_zero() && wd.is_zero() {
                return Ok(None);
            }
            if !dep.is_zero() && !wd.is_zero() {
                return Err("has both a deposit and a withdrawal".into());
            }
            Ok(Some((dep, wd)))
        })();
        match parsed {
            Err(m) => errors.push(format!("line {line_no}: {m}")),
            Ok(None) => skipped += 1,
            Ok(Some((deposit, withdrawal))) => match rules::parse_statement_date(&cell(r, Some(col_date)), date_format) {
                None => errors.push(format!(
                    "line {line_no}: '{}' is not a date{}",
                    cell(r, Some(col_date)),
                    date_format.map(|f| format!(" in the format {f}")).unwrap_or_default()
                )),
                Some(date) => lines.push(Line { date, deposit, withdrawal, description: cell(r, col_description), reference: cell(r, col_reference) }),
            },
        }
    }
    if !errors.is_empty() {
        if dry_run {
            return Ok(
                json!({"dry_run": true, "imported": 0, "duplicates": 0, "skipped": skipped, "errors": errors.iter().take(50).collect::<Vec<_>>(), "preview": []}),
            );
        }
        let shown: Vec<FieldError> = errors.iter().take(50).map(|m| FieldError::new("csv", m.clone())).collect();
        return Err(Error::Validation(shown));
    }
    // Identical lines within one statement are numbered so each keeps its own identity.
    let mut seen: HashMap<String, u32> = HashMap::new();
    let hashes: Vec<String> = lines
        .iter()
        .map(|l| {
            let base = rules::statement_hash(bank, l.date, l.deposit - l.withdrawal, &l.description, &l.reference, 0);
            let n = seen.entry(base).or_insert(0);
            *n += 1;
            rules::statement_hash(bank, l.date, l.deposit - l.withdrawal, &l.description, &l.reference, *n)
        })
        .collect();
    let existing: HashSet<String> = sqlx::query_scalar::<_, String>(
        "select transaction_hash from e_bank_transaction where tenant_id = $1 and bank_account = $2 and transaction_hash = any($3) and docstatus < 2",
    )
    .bind(hc.ctx.tenant)
    .bind(bank)
    .bind(&hashes)
    .fetch_all(&mut **hc.tx)
    .await?
    .into_iter()
    .collect();
    if dry_run {
        let fresh: Vec<&Line> = lines.iter().zip(&hashes).filter(|(_, h)| !existing.contains(*h)).map(|(l, _)| l).collect();
        let preview: Vec<Value> = lines
            .iter()
            .zip(&hashes)
            .take(10)
            .map(|(l, h)| {
                json!({"date": l.date, "description": l.description, "reference": l.reference, "deposit": money(l.deposit), "withdrawal": money(l.withdrawal),
                       "duplicate": existing.contains(h)})
            })
            .collect();
        return Ok(json!({"dry_run": true, "imported": fresh.len(), "duplicates": lines.len() - fresh.len(), "skipped": skipped, "errors": [],
                         "from_date": fresh.iter().map(|l| l.date).min(), "to_date": fresh.iter().map(|l| l.date).max(), "preview": preview}));
    }
    // Hashes are engine-set (read-only), so the lines are created with engine rights after the
    // permission checks above; the audit trail still names the user.
    let engine = elevated(hc.ctx);
    let mut imported = 0usize;
    let (mut first, mut last): (Option<NaiveDate>, Option<NaiveDate>) = (None, None);
    for (l, hash) in lines.iter().zip(&hashes) {
        if existing.contains(hash) {
            continue;
        }
        let mut t = Doc::new();
        t.insert("bank_account".into(), json!(bank.to_string()));
        t.insert("date".into(), json!(l.date.to_string()));
        t.insert("deposit".into(), money(l.deposit));
        t.insert("withdrawal".into(), money(l.withdrawal));
        t.insert("description".into(), json!(l.description));
        t.insert("reference_number".into(), if l.reference.is_empty() { Value::Null } else { json!(l.reference) });
        t.insert("transaction_hash".into(), json!(hash));
        let created = hc.kernel.insert_tx(&engine, hc.tx, "BankTransaction", t).await?;
        hc.kernel.submit_tx(&engine, hc.tx, "BankTransaction", id(&created, "id").unwrap_or_default(), None).await?;
        imported += 1;
        first = Some(first.map_or(l.date, |f| f.min(l.date)));
        last = Some(last.map_or(l.date, |x| x.max(l.date)));
    }
    Ok(json!({"imported": imported, "duplicates": lines.len() - imported, "skipped": skipped, "from_date": first, "to_date": last}))
}

// ───────────────────────────── transactions ─────────────────────────────

/// A voucher's movement on a bank account, in the bank account's currency: positive money in.
#[derive(Debug, Clone)]
pub struct Voucher {
    pub entity: &'static str,
    pub id: Uuid,
    pub number: Option<String>,
    pub date: NaiveDate,
    pub amount: Decimal,
    pub party: Option<String>,
    pub reference: Option<String>,
    pub remark: Option<String>,
}

/// The bank account a transaction belongs to: (GL account, company, currency code).
pub struct Bank {
    pub gl: Uuid,
    pub company: Company,
    pub currency: String,
}

pub async fn bank(hc: &mut HookCtx<'_>, bank_account: Uuid) -> Result<Bank> {
    let ba = get(hc.kernel, hc.ctx, hc.tx, "BankAccount", bank_account).await?;
    let co = company(hc.kernel, hc.ctx, hc.tx, id(&ba, "company").unwrap_or_default()).await?;
    let (code, _) = currency(hc.kernel, hc.ctx, hc.tx, id(&ba, "currency").unwrap_or(co.currency_id)).await?;
    Ok(Bank { gl: id(&ba, "account").ok_or_else(|| Error::invalid("the bank account has no GL account"))?, company: co, currency: code })
}

/// Submitted payment and journal entries on the bank's GL account dated within `[from, to]`
/// (or just `only`), with their amount in the bank account's currency.
pub async fn vouchers(hc: &mut HookCtx<'_>, bank: &Bank, from: NaiveDate, to: NaiveDate, only: Option<Uuid>) -> Result<Vec<Voucher>> {
    let mut out = Vec::new();
    let pays: Vec<(
        Uuid,
        Option<String>,
        NaiveDate,
        Option<String>,
        Option<Decimal>,
        Option<Decimal>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(
        "select p.id, p.number, p.posting_date, p.payment_type, p.paid_amount, p.base_paid_amount, c.code, p.party_name, p.reference_no, p.remarks \
             from e_payment_entry p left join e_currency c on c.tenant_id = p.tenant_id and c.id = p.currency \
             where p.tenant_id = $1 and p.company = $2 and p.docstatus = 1 and p.bank_account = $3 and p.posting_date between $4 and $5 \
             and ($6::uuid is null or p.id = $6)",
    )
    .bind(hc.ctx.tenant)
    .bind(bank.company.id)
    .bind(bank.gl)
    .bind(from)
    .bind(to)
    .bind(only)
    .fetch_all(&mut **hc.tx)
    .await?;
    for (vid, number, date, kind, paid, base_paid, code, party, reference, remark) in pays {
        let amount = if code.as_deref() == Some(bank.currency.as_str()) {
            paid
        } else if bank.currency == bank.company.currency_code {
            base_paid
        } else {
            None
        };
        let Some(amount) = amount.filter(|a| !a.is_zero()) else { continue };
        let signed = if kind.as_deref() == Some("Pay") { -amount } else { amount };
        out.push(Voucher { entity: "PaymentEntry", id: vid, number, date, amount: signed, party, reference, remark });
    }
    // Journal entries are booked in the company currency.
    if bank.currency == bank.company.currency_code {
        let jes: Vec<(Uuid, Option<String>, NaiveDate, Decimal, Option<String>, Option<String>)> = sqlx::query_as(
            "select j.id, j.number, j.posting_date, sum(coalesce(a.debit, 0) - coalesce(a.credit, 0)), j.remark, string_agg(a.remarks, ' ') \
             from e_journal_entry j join e_journal_entry__accounts a on a.tenant_id = j.tenant_id and a.parent_id = j.id \
             where j.tenant_id = $1 and j.company = $2 and j.docstatus = 1 and a.account = $3 and j.posting_date between $4 and $5 \
             and ($6::uuid is null or j.id = $6) group by j.id, j.number, j.posting_date, j.remark having sum(coalesce(a.debit, 0) - coalesce(a.credit, 0)) <> 0",
        )
        .bind(hc.ctx.tenant)
        .bind(bank.company.id)
        .bind(bank.gl)
        .bind(from)
        .bind(to)
        .bind(only)
        .fetch_all(&mut **hc.tx)
        .await?;
        for (vid, number, date, amount, remark, line_remarks) in jes {
            out.push(Voucher { entity: "JournalEntry", id: vid, number, date, amount, party: None, reference: line_remarks, remark });
        }
    }
    Ok(out)
}

/// How much of each voucher submitted bank transactions have already cleared.
async fn cleared(hc: &mut HookCtx<'_>, ids: &[Uuid]) -> Result<HashMap<Uuid, Decimal>> {
    let keys: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    let rows: Vec<(String, Decimal)> = sqlx::query_as(
        "select a.payment_entry, sum(a.allocated_amount) from e_bank_transaction__allocations a \
         join e_bank_transaction t on t.tenant_id = a.tenant_id and t.id = a.parent_id \
         where a.tenant_id = $1 and t.docstatus = 1 and a.payment_entry = any($2) group by a.payment_entry",
    )
    .bind(hc.ctx.tenant)
    .bind(&keys)
    .fetch_all(&mut **hc.tx)
    .await?;
    Ok(rows.into_iter().filter_map(|(k, v)| Some((k.parse().ok()?, v))).collect())
}

fn signed(doc: &Doc) -> Decimal {
    d(doc, "deposit") - d(doc, "withdrawal")
}

fn words(s: &str) -> String {
    s.to_lowercase()
}

pub struct BankTransactionHook;

#[async_trait]
impl DocHook for BankTransactionHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if d(doc, "docstatus").is_zero() {
            let amount = d(doc, "deposit") + d(doc, "withdrawal");
            set(doc, "allocated_amount", money(Decimal::ZERO));
            set(doc, "unallocated_amount", money(amount));
            set(doc, "status", json!("Unreconciled"));
            set(doc, "allocations", Value::Array(vec![]));
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if let Some(ba) = id(doc, "bank_account") {
            let account = get(hc.kernel, hc.ctx, hc.tx, "BankAccount", ba).await?;
            if b(&account, "disabled") {
                return Err(Error::field("bank_account", "this bank account is disabled"));
            }
        }
        Ok(())
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !rows(doc, "allocations").is_empty() {
            return Err(Error::invalid("this transaction is reconciled; undo the reconciliation first"));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut v = Doc::new();
        v.insert("status".into(), json!("Cancelled"));
        // A cancelled line no longer blocks importing it again.
        v.insert("transaction_hash".into(), Value::Null);
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "BankTransaction", id(doc, "id").unwrap_or_default(), v).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let out = match name {
            "get_suggestions" => suggestions(hc, doc, params).await?,
            "reconcile" => {
                let list = params.get("vouchers").and_then(Value::as_array).ok_or_else(|| Error::field("vouchers", "list the vouchers to reconcile"))?;
                let mut wanted = Vec::new();
                for v in list {
                    let kind = match v.get("payment_document").and_then(Value::as_str) {
                        Some("PaymentEntry" | "Payment Entry") => "PaymentEntry",
                        Some("JournalEntry" | "Journal Entry") => "JournalEntry",
                        _ => return Err(Error::field("payment_document", "PaymentEntry or JournalEntry")),
                    };
                    let vid = v
                        .get("payment_entry")
                        .and_then(Value::as_str)
                        .and_then(|x| x.parse().ok())
                        .ok_or_else(|| Error::field("payment_entry", "not a voucher id"))?;
                    let amount = v
                        .get("allocated_amount")
                        .filter(|a| !a.is_null())
                        .map(ferrum_kernel::value::parse_decimal)
                        .transpose()
                        .map_err(|m| Error::field("allocated_amount", m))?;
                    wanted.push((kind, vid, amount));
                }
                Value::Object(reconcile(hc, doc, wanted).await?)
            }
            "unreconcile" => {
                let only = params.get("payment_entry").and_then(Value::as_str).and_then(|x| x.parse::<Uuid>().ok());
                Value::Object(unreconcile(hc, doc, only).await?)
            }
            "make_payment_entry" => make_payment(hc, doc, params).await?,
            "make_journal_entry" => make_journal(hc, doc, params).await?,
            _ => return Ok(None),
        };
        Ok(Some(out))
    }
}

/// A statement line as matching sees it.
struct Wanted {
    id: Uuid,
    on: NaiveDate,
    /// What is still to reconcile (positive).
    open: Decimal,
    /// 1 for a deposit, -1 for a withdrawal.
    direction: Decimal,
    /// Description and reference, lower case: what vouchers' references and parties are looked for in.
    text: String,
    reference: String,
}

impl Wanted {
    fn of(doc: &Doc) -> Result<Wanted> {
        Ok(Wanted {
            id: id(doc, "id").ok_or_else(|| Error::internal("no id"))?,
            on: opt_date(doc, "date").ok_or_else(|| Error::field("date", "is required"))?,
            open: d(doc, "unallocated_amount"),
            direction: signed(doc).signum(),
            text: words(&format!("{} {}", s(doc, "description").unwrap_or_default(), s(doc, "reference_number").unwrap_or_default())),
            reference: words(s(doc, "reference_number").unwrap_or_default()),
        })
    }
}

/// A voucher that could clear a line, and why it was picked.
#[derive(Debug, Clone)]
struct Candidate {
    voucher: Voucher,
    remaining: Decimal,
    exact: bool,
    reference_match: bool,
    party_match: bool,
    days: i64,
    score: Decimal,
}

impl Candidate {
    fn json(&self) -> Value {
        let v = &self.voucher;
        json!({"payment_document": v.entity, "payment_entry": v.id, "number": v.number, "posting_date": v.date, "amount": money(v.amount.abs()),
               "remaining": money(self.remaining), "party": v.party, "reference": v.reference, "exact_amount": self.exact, "reference_match": self.reference_match,
               "party_match": self.party_match, "days_apart": self.days, "score": money(self.score)})
    }
}

/// What is left of each voucher: its amount less what submitted statement lines cleared.
fn left(v: &Voucher, done: &HashMap<Uuid, Decimal>) -> Decimal {
    v.amount.abs() - done.get(&v.id).copied().unwrap_or_default()
}

/// Vouchers that could clear a line, best first: same direction, something left to clear, within
/// `window` days; scored by exact amount (100), reference (40), party name (30) and, up to 20,
/// how near the dates are.
fn rank(line: &Wanted, vouchers: &[Voucher], done: &HashMap<Uuid, Decimal>, window: i64) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for v in vouchers.iter().filter(|v| v.amount.signum() == line.direction) {
        let days = (v.date - line.on).num_days().abs();
        let remaining = left(v, done);
        if remaining <= Decimal::ZERO || days > window {
            continue;
        }
        let exact = remaining == line.open;
        let mut score = if exact { Decimal::from(100) } else { Decimal::ZERO };
        let refs: Vec<String> = [v.reference.as_deref(), v.number.as_deref()].into_iter().flatten().map(words).filter(|r| r.trim().len() >= 3).collect();
        let remark = v.remark.as_deref().map(words).unwrap_or_default();
        let reference_match = refs.iter().any(|r| line.text.contains(r.trim()) || (line.reference.trim().len() >= 3 && r.contains(line.reference.trim())))
            || (line.reference.trim().len() >= 3 && remark.contains(line.reference.trim()));
        if reference_match {
            score += Decimal::from(40);
        }
        let party_match = v.party.as_deref().map(words).is_some_and(|p| p.trim().len() >= 3 && line.text.contains(p.trim()));
        if party_match {
            score += Decimal::from(30);
        }
        if window > 0 {
            score += Decimal::from(20 * (window - days.min(window))) / Decimal::from(window);
        }
        out.push(Candidate { voucher: v.clone(), remaining, exact, reference_match, party_match, days, score: score.round_dp(2) });
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.score));
    out
}

/// Which exact match each line gets (line index → its candidate's index), so that no voucher and no
/// line is used twice: the best scores go first, then the nearest dates, then the lines in
/// statement order. Two coffees of 3.50 and two vouchers of 3.50 each pair off, rather than both
/// lines claiming the better voucher.
fn assign(ranked: &[Vec<Candidate>]) -> HashMap<usize, usize> {
    let mut pairs: Vec<(Decimal, i64, usize, usize)> = Vec::new();
    for (l, cands) in ranked.iter().enumerate() {
        for (c, cand) in cands.iter().enumerate().filter(|(_, c)| c.exact) {
            pairs.push((cand.score, cand.days, l, c));
        }
    }
    pairs.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)).then(a.3.cmp(&b.3)));
    let (mut lines, mut vouchers) = (HashSet::new(), HashSet::new());
    let mut chosen = HashMap::new();
    for (_, _, l, c) in pairs {
        let voucher = ranked[l][c].voucher.id;
        if !lines.contains(&l) && !vouchers.contains(&voucher) {
            lines.insert(l);
            vouchers.insert(voucher);
            chosen.insert(l, c);
        }
    }
    chosen
}

/// Candidate vouchers for an unreconciled transaction, best first (at most 20).
async fn suggestions(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    let window = params.get("window_days").and_then(Value::as_i64).unwrap_or(30).clamp(0, 366);
    let line = Wanted::of(doc)?;
    let bank = bank(hc, id(doc, "bank_account").unwrap_or_default()).await?;
    let candidates = vouchers(hc, &bank, line.on - chrono::Duration::days(window), line.on + chrono::Duration::days(window), None).await?;
    let ids: Vec<Uuid> = candidates.iter().map(|v| v.id).collect();
    let done = cleared(hc, &ids).await?;
    let ranked = rank(&line, &candidates, &done, window);
    Ok(json!({"unallocated_amount": money(line.open), "suggestions": ranked.iter().take(20).map(Candidate::json).collect::<Vec<_>>()}))
}

/// `workbench {as_of?, from?, window_days?, reconciled?}` on a bank account: everything the
/// reconciliation screen draws in one answer. The statement lines still to reconcile (and, with
/// `reconciled`, the cleared ones, to undo), each with the bank's balance after it and the exact
/// match it would get; the entries in the books the bank has not cleared; and the figures: the
/// statement's balance, the books', what explains the difference, and what is left unexplained.
async fn workbench(hc: &mut HookCtx<'_>, account: &Doc, params: &Value) -> Result<Value> {
    let ba = id(account, "id").ok_or_else(|| Error::internal("no id"))?;
    let date_param = |k: &str| -> Result<Option<NaiveDate>> {
        match params.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()) {
            None => Ok(None),
            Some(v) => ferrum_kernel::value::parse_date(v).map(Some).map_err(|m| Error::field(k, m)),
        }
    };
    let as_of = date_param("as_of")?.unwrap_or_else(|| hc.ctx.today());
    let from = date_param("from")?;
    let window = params.get("window_days").and_then(Value::as_i64).unwrap_or(30).clamp(0, 366);
    let with_reconciled = params.get("reconciled").and_then(Value::as_bool).unwrap_or(false);

    let raw = HashMap::from([("bank_account".to_string(), ba.to_string()), ("as_of".to_string(), as_of.to_string())]);
    let statement = bank_statement(hc.ctx, hc.tx, &Params { raw: &raw }).await?;
    let (opening, opening_date) = (d(account, "opening_balance"), opt_date(account, "opening_date"));
    let opening_counts = opening_date.is_none_or(|o| o <= as_of);

    // The balance after each line is a running sum in statement order; lines on or before the
    // opening date are inside the opening balance already.
    #[allow(clippy::type_complexity)]
    let lines: Vec<(Uuid, Option<String>, NaiveDate, Option<String>, Option<String>, Decimal, Decimal, Decimal, Decimal, Option<String>, i64, Decimal)> =
        sqlx::query_as(
            "select * from (select t.id, t.number, t.date, t.description, t.reference_number, coalesce(t.deposit, 0) as deposit, \
             coalesce(t.withdrawal, 0) as withdrawal, coalesce(t.allocated_amount, 0) as allocated_amount, \
             coalesce(t.unallocated_amount, 0) as unallocated_amount, t.status, t.version::int8 as version, \
             coalesce(sum(case when $4::date is null or t.date > $4 then coalesce(t.deposit, 0) - coalesce(t.withdrawal, 0) else 0 end) \
               over (order by t.date, t.number, t.id), 0) as running \
             from e_bank_transaction t where t.tenant_id = $1 and t.bank_account = $2 and t.docstatus = 1 and t.date <= $3) x \
             where ($5 or x.unallocated_amount > 0) and ($6::date is null or x.date >= $6) order by x.date, x.number, x.id limit 2001",
        )
        .bind(hc.ctx.tenant)
        .bind(ba)
        .bind(as_of)
        .bind(opening_date)
        .bind(with_reconciled)
        .bind(from)
        .fetch_all(&mut **hc.tx)
        .await?;
    let truncated = lines.len() > 2000;
    let lines = &lines[..lines.len().min(2000)];

    let ids: Vec<Uuid> = lines.iter().map(|l| l.0).collect();
    let allocations: Vec<(Uuid, String, String, Option<String>, Option<NaiveDate>, Decimal)> = sqlx::query_as(
        "select parent_id, payment_document, payment_entry, payment_number, posting_date, allocated_amount from e_bank_transaction__allocations \
         where tenant_id = $1 and parent_id = any($2) order by parent_id, idx",
    )
    .bind(hc.ctx.tenant)
    .bind(&ids)
    .fetch_all(&mut **hc.tx)
    .await?;

    // Entries in the books are read once for every line: those within the window of a line are its
    // candidates, and those with something left (up to the date) are what the bank has not cleared.
    let bank = bank(hc, ba).await?;
    let everything = vouchers(hc, &bank, ever().0, as_of + chrono::Duration::days(window), None).await?;
    let done = cleared(hc, &everything.iter().map(|v| v.id).collect::<Vec<_>>()).await?;

    let open: Vec<&(Uuid, Option<String>, NaiveDate, Option<String>, Option<String>, Decimal, Decimal, Decimal, Decimal, Option<String>, i64, Decimal)> =
        lines.iter().filter(|l| l.8 > Decimal::ZERO).collect();
    let wanted: Vec<Wanted> = open
        .iter()
        .map(|l| Wanted {
            id: l.0,
            on: l.2,
            open: l.8,
            direction: (l.5 - l.6).signum(),
            text: words(&format!("{} {}", l.3.as_deref().unwrap_or_default(), l.4.as_deref().unwrap_or_default())),
            reference: words(l.4.as_deref().unwrap_or_default()),
        })
        .collect();
    let ranked: Vec<Vec<Candidate>> = wanted.iter().map(|w| rank(w, &everything, &done, window)).collect();
    let chosen = assign(&ranked);
    let matched: HashMap<Uuid, &Candidate> = chosen.iter().map(|(l, c)| (wanted[*l].id, &ranked[*l][*c])).collect();

    let rows: Vec<Value> = lines
        .iter()
        .map(|l| {
            let balance = opening_date.is_none_or(|o| l.2 > o).then(|| (if opening_counts { opening } else { Decimal::ZERO }) + l.11);
            let mine: Vec<Value> = allocations
                .iter()
                .filter(|a| a.0 == l.0)
                .map(|a| json!({"payment_document": a.1, "payment_entry": a.2, "payment_number": a.3, "posting_date": a.4, "allocated_amount": money(a.5)}))
                .collect();
            json!({"id": l.0, "number": l.1, "date": l.2, "description": l.3, "reference_number": l.4, "deposit": money(l.5), "withdrawal": money(l.6),
                   "amount": money(l.5 - l.6), "allocated": money(l.7), "unallocated": money(l.8), "status": l.9, "version": l.10, "balance": balance.map(money),
                   "allocations": mine, "match": matched.get(&l.0).map(|c| c.json())})
        })
        .collect();

    let mut books: Vec<Value> = everything
        .iter()
        .filter(|v| v.date <= as_of && left(v, &done) > Decimal::ZERO)
        .map(|v| {
            json!({"payment_document": v.entity, "payment_entry": v.id, "number": v.number, "posting_date": v.date, "party": v.party, "reference": v.reference,
                   "amount": money(v.amount), "remaining": money(left(v, &done))})
        })
        .collect();
    books.sort_by(|a, b| b["posting_date"].as_str().cmp(&a["posting_date"].as_str()));
    let books_truncated = books.len() > 500;
    books.truncate(500);

    let totals = &statement["totals"];
    let (statement_balance, book_balance) = (d_of(&totals["statement_balance"]), d_of(&totals["book_balance"]));
    Ok(json!({
        "bank_account": ba, "name": s(account, "account_name"), "currency": statement["meta"]["currency"], "precision": statement["meta"]["precision"],
        "as_of": as_of, "opening_balance": money(opening), "opening_date": opening_date,
        "figures": {
            "statement_balance": money(statement_balance), "book_balance": money(book_balance), "difference": money(statement_balance - book_balance),
            "uncleared_deposits": totals["uncleared_deposits"], "uncleared_payments": totals["uncleared_payments"], "unrecorded": totals["unrecorded"],
            "expected_bank_balance": totals["expected_bank_balance"], "unexplained": totals["difference"],
        },
        "counts": {"unreconciled": open.len(), "matched": matched.len()},
        "lines": rows, "books": books, "truncated": truncated, "books_truncated": books_truncated,
    }))
}

fn d_of(v: &Value) -> Decimal {
    ferrum_kernel::value::parse_decimal(v).unwrap_or_default()
}

/// `accept_matches {matches: [{transaction, payment_document, payment_entry, allocated_amount?}]}`:
/// reconciles each line with its voucher through the line's own `reconcile`, all in one
/// transaction: a pairing that no longer holds (the voucher was cleared meanwhile, the amounts
/// moved) refuses the lot, naming the line. The caller's rights on the lines are what count.
async fn accept_matches(hc: &mut HookCtx<'_>, params: &Value) -> Result<Value> {
    let list = params.get("matches").and_then(Value::as_array).filter(|l| !l.is_empty()).ok_or_else(|| Error::field("matches", "choose at least one match"))?;
    if list.len() > 500 {
        return Err(Error::field("matches", "at most 500 matches at a time"));
    }
    let mut per_line: Vec<(Uuid, Vec<Value>)> = Vec::new();
    for (i, m) in list.iter().enumerate() {
        let line = m
            .get("transaction")
            .and_then(Value::as_str)
            .and_then(|x| x.parse::<Uuid>().ok())
            .ok_or_else(|| Error::Validation(vec![FieldError::new("transaction", "not a bank transaction id").in_row("matches", i)]))?;
        let mut voucher = Map::new();
        for k in ["payment_document", "payment_entry", "allocated_amount"] {
            if let Some(v) = m.get(k).filter(|v| !v.is_null()) {
                voucher.insert(k.to_string(), v.clone());
            }
        }
        match per_line.iter_mut().find(|(l, _)| *l == line) {
            Some((_, vs)) => vs.push(Value::Object(voucher)),
            None => per_line.push((line, vec![Value::Object(voucher)])),
        }
    }
    let mut done = Vec::new();
    for (line, vouchers) in per_line {
        let out = match hc.kernel.run_action_tx(hc.ctx, hc.tx, "BankTransaction", line, "reconcile", &json!({"vouchers": vouchers})).await {
            Ok(out) => out,
            // Say which line, in the line's own words.
            Err(e @ (Error::Invalid(_) | Error::Validation(_))) => {
                let said = match &e {
                    Error::Validation(fs) => fs.iter().map(|f| f.message.clone()).collect::<Vec<_>>().join("; "),
                    other => other.to_string(),
                };
                let number: Option<String> = sqlx::query_scalar("select number from e_bank_transaction where tenant_id = $1 and id = $2")
                    .bind(hc.ctx.tenant)
                    .bind(line)
                    .fetch_optional(&mut **hc.tx)
                    .await?
                    .flatten();
                return Err(Error::Invalid(format!("{}: {said}", number.unwrap_or_else(|| "a statement line".into()))));
            }
            Err(e) => return Err(e),
        };
        done.push(json!({"id": line, "status": out["status"], "unallocated": out["unallocated_amount"]}));
    }
    Ok(json!({"accepted": done.len(), "lines": done}))
}

/// Status of a transaction from what is still unallocated.
fn status(amount: Decimal, unallocated: Decimal) -> &'static str {
    if unallocated.is_zero() {
        "Reconciled"
    } else if unallocated == amount {
        "Unreconciled"
    } else {
        "Partially Reconciled"
    }
}

/// Writes the allocations and derived amounts of a submitted transaction (engine update of its
/// own child rows, versioned; the action itself is in the audit trail).
async fn store(hc: &mut HookCtx<'_>, old: &Doc, allocations: Vec<Value>) -> Result<Map<String, Value>> {
    let amount = d(old, "deposit") + d(old, "withdrawal");
    let allocated: Decimal = allocations.iter().filter_map(Value::as_object).map(|r| d(r, "allocated_amount")).sum();
    let mut new = old.clone();
    new.insert("allocations".into(), Value::Array(allocations));
    new.insert("allocated_amount".into(), money(allocated));
    new.insert("unallocated_amount".into(), money(amount - allocated));
    new.insert("status".into(), json!(status(amount, amount - allocated)));
    new.insert("version".into(), json!(old.get("version").and_then(Value::as_i64).unwrap_or(1) + 1));
    new.insert("updated_by".into(), if hc.ctx.user.is_nil() { Value::Null } else { json!(hc.ctx.user.to_string()) });
    let ent = hc.entity.clone();
    ferrum_kernel::store::update_row(hc.tx, &hc.ctx.schema, &ent, hc.ctx.tenant, &new, old).await?;
    let tid = id(old, "id").unwrap_or_default();
    hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "BankTransaction", tid, false).await
}

/// Sets (fully cleared) or clears a voucher's clearance date.
async fn set_clearance(hc: &mut HookCtx<'_>, entity: &str, voucher: Uuid, on: Option<NaiveDate>) -> Result<()> {
    let mut v = Doc::new();
    v.insert("fin_clearance_date".into(), on.map(|x| json!(x.to_string())).unwrap_or(Value::Null));
    hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, entity, voucher, v).await
}

/// Allocates the transaction to vouchers; `None` amounts take what both sides still have open.
async fn reconcile(hc: &mut HookCtx<'_>, doc: &Doc, wanted: Vec<(&'static str, Uuid, Option<Decimal>)>) -> Result<Map<String, Value>> {
    if wanted.is_empty() {
        return Err(Error::field("vouchers", "choose at least one voucher"));
    }
    let on = opt_date(doc, "date").ok_or_else(|| Error::field("date", "is required"))?;
    let bank = bank(hc, id(doc, "bank_account").unwrap_or_default()).await?;
    let direction = signed(doc).signum();
    let mut left = d(doc, "unallocated_amount");
    let mut allocations: Vec<Value> = rows(doc, "allocations").into_iter().map(Value::Object).collect();
    let mut seen = HashSet::new();
    let mut cleared_now = Vec::new();
    for (i, (entity, vid, amount)) in wanted.into_iter().enumerate() {
        if !seen.insert(vid) {
            return Err(Error::Validation(vec![FieldError::new("payment_entry", "listed twice").in_row("vouchers", i)]));
        }
        // Lock the voucher: two statements cannot clear the same money twice.
        let vdoc = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, entity, vid, true).await?;
        if d(&vdoc, "docstatus") != Decimal::ONE {
            return Err(Error::Validation(vec![FieldError::new("payment_entry", "is not submitted").in_row("vouchers", i)]));
        }
        let (from, to) = ever();
        let found = vouchers(hc, &bank, from, to, Some(vid)).await?.into_iter().find(|v| v.entity == entity);
        let Some(v) = found else {
            return Err(Error::Validation(vec![FieldError::new("payment_entry", "does not move money on this bank account").in_row("vouchers", i)]));
        };
        if v.amount.signum() != direction {
            let what = if direction > Decimal::ZERO { "a deposit cannot clear a payment out" } else { "a withdrawal cannot clear money received" };
            return Err(Error::Validation(vec![FieldError::new("payment_entry", what).in_row("vouchers", i)]));
        }
        let before = cleared(hc, &[vid]).await?.get(&vid).copied().unwrap_or_default();
        let remaining = v.amount.abs() - before;
        let take = amount.unwrap_or_else(|| remaining.min(left));
        if take <= Decimal::ZERO || take > remaining {
            return Err(Error::Validation(vec![
                FieldError::new("allocated_amount", format!("{} has {} left to clear", v.number.clone().unwrap_or_default(), remaining.normalize()))
                    .in_row("vouchers", i),
            ]));
        }
        if take > left {
            return Err(Error::Validation(vec![
                FieldError::new("allocated_amount", format!("only {} of this transaction is left to reconcile", left.normalize())).in_row("vouchers", i),
            ]));
        }
        left -= take;
        allocations.push(json!({"id": Uuid::now_v7().to_string(), "idx": allocations.len(), "payment_document": entity, "payment_entry": vid.to_string(),
            "payment_number": v.number, "posting_date": v.date.to_string(), "allocated_amount": money(take)}));
        if take == remaining {
            cleared_now.push((entity, vid));
        }
    }
    let stored = store(hc, doc, allocations).await?;
    for (entity, vid) in cleared_now {
        set_clearance(hc, entity, vid, Some(on)).await?;
    }
    Ok(stored)
}

/// Removes the transaction's allocations (all, or those of one voucher); vouchers no longer
/// cleared in full lose their clearance date.
async fn unreconcile(hc: &mut HookCtx<'_>, doc: &Doc, only: Option<Uuid>) -> Result<Map<String, Value>> {
    let (removed, kept): (Vec<Doc>, Vec<Doc>) =
        rows(doc, "allocations").into_iter().partition(|r| only.is_none_or(|v| s(r, "payment_entry") == Some(v.to_string().as_str())));
    if removed.is_empty() {
        return Err(Error::invalid("nothing to undo for that voucher"));
    }
    let kept: Vec<Value> = kept
        .into_iter()
        .enumerate()
        .map(|(i, mut r)| {
            r.insert("idx".into(), json!(i));
            Value::Object(r)
        })
        .collect();
    let stored = store(hc, doc, kept).await?;
    for r in removed {
        let entity = if s(&r, "payment_document") == Some("JournalEntry") { "JournalEntry" } else { "PaymentEntry" };
        if let Some(vid) = id(&r, "payment_entry") {
            set_clearance(hc, entity, vid, None).await?;
        }
    }
    Ok(stored)
}

/// `make_payment_entry {party, allocate_invoices?}`: a deposit becomes a receipt from the customer,
/// a withdrawal a payment to the supplier, for what is left to reconcile; submitted and reconciled.
/// With `allocate_invoices` it settles the party's oldest open invoices, otherwise it stays an
/// advance (see the invoices' Allocate Advance Payments action).
async fn make_payment(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    let receive = signed(doc) > Decimal::ZERO;
    // `party`, or the record page's `customer` for money received and `supplier` for money paid out.
    let uuid = |name: &str| params.get(name).and_then(Value::as_str).filter(|x| !x.is_empty()).and_then(|x| x.parse::<Uuid>().ok());
    let party = uuid("party").or_else(|| uuid(if receive { "customer" } else { "supplier" })).ok_or_else(|| {
        if receive { Error::field("customer", "choose the customer who paid") } else { Error::field("supplier", "choose the supplier who was paid") }
    })?;
    let bank = bank(hc, id(doc, "bank_account").unwrap_or_default()).await?;
    let mut pe = Doc::new();
    pe.insert("company".into(), json!(bank.company.id.to_string()));
    pe.insert("payment_type".into(), json!(if receive { "Receive" } else { "Pay" }));
    pe.insert(if receive { "customer" } else { "supplier" }.into(), json!(party.to_string()));
    pe.insert("posting_date".into(), doc.get("date").cloned().unwrap_or(Value::Null));
    pe.insert("bank_account".into(), json!(bank.gl.to_string()));
    pe.insert("currency".into(), doc.get("currency").cloned().unwrap_or(Value::Null));
    pe.insert("paid_amount".into(), money(d(doc, "unallocated_amount")));
    pe.insert("reference_no".into(), json!(s(doc, "reference_number").or(s(doc, "number")).unwrap_or_default()));
    pe.insert("reference_date".into(), doc.get("date").cloned().unwrap_or(Value::Null));
    pe.insert("remarks".into(), json!(s(doc, "description").unwrap_or_default()));
    let created = hc.kernel.insert_tx(hc.ctx, hc.tx, "PaymentEntry", pe).await?;
    let pid = id(&created, "id").ok_or_else(|| Error::internal("no id"))?;
    if params.get("allocate_invoices").and_then(Value::as_bool).unwrap_or(false) {
        hc.kernel.run_action_tx(hc.ctx, hc.tx, "PaymentEntry", pid, "get_outstanding_invoices", &json!({})).await?;
    }
    let submitted = hc.kernel.submit_tx(hc.ctx, hc.tx, "PaymentEntry", pid, None).await?;
    reconcile(hc, doc, vec![("PaymentEntry", pid, None)]).await?;
    Ok(json!({"entity": "PaymentEntry", "id": pid, "number": submitted.get("number")}))
}

/// `make_journal_entry {account, cost_center?, project?, remark?}`: books what is left to reconcile
/// between the bank and `account` (bank charges, interest, transfers), submitted and reconciled.
async fn make_journal(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    let account = params
        .get("account")
        .and_then(Value::as_str)
        .and_then(|x| x.parse::<Uuid>().ok())
        .ok_or_else(|| Error::field("account", "choose the account to book against"))?;
    let bank = bank(hc, id(doc, "bank_account").unwrap_or_default()).await?;
    if account == bank.gl {
        return Err(Error::field("account", "choose an account other than the bank's own"));
    }
    if bank.currency != bank.company.currency_code {
        return Err(Error::invalid("journal entries are in the company currency; record a payment entry for this foreign-currency account"));
    }
    let amount = money(d(doc, "unallocated_amount"));
    let dim = |k: &str| params.get(k).cloned().filter(|v| !v.is_null()).unwrap_or(Value::Null);
    let other = json!({"account": account.to_string(), "cost_center": dim("cost_center"), "project": dim("project"),
                       "remarks": s(doc, "description").unwrap_or_default()});
    let (mut debit, mut credit) = (json!({"account": bank.gl.to_string()}), other);
    if signed(doc) < Decimal::ZERO {
        std::mem::swap(&mut debit, &mut credit);
    }
    debit["debit"] = amount.clone();
    credit["credit"] = amount;
    let remark = params
        .get("remark")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("{} {}", s(doc, "reference_number").unwrap_or_default(), s(doc, "description").unwrap_or_default()).trim().to_string());
    let mut je = Doc::new();
    je.insert("company".into(), json!(bank.company.id.to_string()));
    je.insert("posting_date".into(), doc.get("date").cloned().unwrap_or(Value::Null));
    je.insert("accounts".into(), json!([debit, credit]));
    je.insert("remark".into(), json!(remark));
    let created = hc.kernel.insert_tx(hc.ctx, hc.tx, "JournalEntry", je).await?;
    let jid = id(&created, "id").ok_or_else(|| Error::internal("no id"))?;
    let submitted = hc.kernel.submit_tx(hc.ctx, hc.tx, "JournalEntry", jid, None).await?;
    reconcile(hc, doc, vec![("JournalEntry", jid, None)]).await?;
    Ok(json!({"entity": "JournalEntry", "id": jid, "number": submitted.get("number")}))
}

/// Payment and journal entries reconciled with a bank statement cannot be cancelled until the
/// reconciliation is undone: the bank still shows the money moving.
pub struct VoucherGuard;

#[async_trait]
impl DocHook for VoucherGuard {
    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !installed(hc.ctx) {
            return Ok(());
        }
        let Some(vid) = id(doc, "id") else { return Ok(()) };
        let found: Option<Option<String>> = sqlx::query_scalar(
            "select t.number from e_bank_transaction__allocations a join e_bank_transaction t on t.tenant_id = a.tenant_id and t.id = a.parent_id \
             where a.tenant_id = $1 and t.docstatus = 1 and a.payment_entry = $2 limit 1",
        )
        .bind(hc.ctx.tenant)
        .bind(vid.to_string())
        .fetch_optional(&mut **hc.tx)
        .await?;
        if let Some(number) = found {
            return Err(Error::invalid(format!(
                "this entry is reconciled with bank transaction {}; undo that reconciliation first",
                number.unwrap_or_default()
            )));
        }
        Ok(())
    }
}
