//! Bank accounts, statement import and reconciliation.
//!
//! A bank account ties a company's account at a bank to its GL account. Statements are imported
//! from any file a `StatementFormat` describes (`statement`); every line becomes a submitted bank
//! transaction carrying a hash of the bank's reference for it, or of its content, so importing the
//! same (or an overlapping) statement again skips what is there.
//! Reconciling allocates a transaction to payment entries and journal entries on the bank's GL
//! account - several at once, partly if need be - never beyond what either side has left; a
//! voucher cleared in full gets its clearance date. An unmatched line becomes a new payment or
//! journal entry, reconciled in the same step.

use crate::common::{installed, opt_date};
use crate::reports::bank_statement;
use crate::rules;
use crate::statement::{self, Line, Reading};
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_domain::reports::Params;
use ferrum_domain::util::{Company, b, company, currency, d, elevated, get, id, money, rows, s, set, sys};
use ferrum_kernel::{Action, Doc, DocHook, Error, FieldError, HookCtx, Result};
use regex::Regex;
use rust_decimal::Decimal;
use rust_decimal::prelude::Signed;
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

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
            if s(&acc, "account_type") != Some("Bank") {
                errors.push(FieldError::new("account", "choose a GL account of type Bank"));
            }
        }
        if let Some(iban) = s(doc, "iban")
            && !rules::iban_valid(iban)
        {
            errors.push(FieldError::new("iban", "is not a valid IBAN (check the digits)"));
        }
        // A feed names one bank account: its connection and the provider's id of the account, both or neither.
        match (s(doc, "feed_connection"), s(doc, "feed_account_ref")) {
            (Some(_), None) => errors.push(FieldError::new("feed_account_ref", "give the provider's id of this account too")),
            (None, Some(_)) => errors.push(FieldError::new("feed_connection", "give the feed's connection too")),
            (Some(c), Some(r)) => {
                let taken: Option<String> = sqlx::query_scalar(
                    "select account_name from e_bank_account where tenant_id = $1 and feed_connection = $2 and feed_account_ref = $3 and id <> $4 limit 1",
                )
                .bind(hc.ctx.tenant)
                .bind(c)
                .bind(r)
                .bind(id(doc, "id").unwrap_or_default())
                .fetch_optional(&mut **hc.tx)
                .await?;
                if let Some(other) = taken {
                    errors.push(FieldError::new("feed_account_ref", format!("{other} already takes this feed's account")));
                }
            }
            (None, None) => {}
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
            "preview_statement" => statement(hc, doc, params, false).await.map(Some),
            "import_transactions" => statement(hc, doc, params, true).await.map(Some),
            "workbench" => workbench(hc, doc, params).await.map(Some),
            "accept_matches" => accept_matches(hc, params).await.map(Some),
            "apply_rules" => apply_rules(hc, params).await.map(Some),
            "take_feed_lines" => take_feed_lines(hc, doc).await.map(Some),
            _ => Ok(None),
        }
    }
}

/// The format a statement is read with: the one named, the columns chosen on the screen, the
/// first enabled format (by name) whose sniff fits the text, or the account's own. `None`: no
/// format fits.
async fn choose(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value, text: &str) -> Result<Option<(Option<(Uuid, String)>, Reading)>> {
    let formats: Vec<(Uuid, String, String, Value, bool)> =
        sqlx::query_as("select id, format_name, kind, reading, coalesce(disabled, false) from e_statement_format where tenant_id = $1 order by format_name")
            .bind(hc.ctx.tenant)
            .fetch_all(&mut **hc.tx)
            .await?;
    let checked = |(fid, name, kind, reading, _): &(Uuid, String, String, Value, bool)| {
        statement::reading(kind, reading).map(|r| (Some((*fid, name.clone())), r)).map_err(|m| Error::field("format", format!("{name}: {m}")))
    };
    if let Some(asked) = params.get("format").and_then(Value::as_str).filter(|v| !v.trim().is_empty()) {
        let f = formats
            .iter()
            .find(|f| f.0.to_string() == asked.trim())
            .filter(|f| !f.4)
            .ok_or_else(|| Error::field("format", "choose an enabled statement format"))?;
        return checked(f).map(Some);
    }
    if let Some(inline) = params.get("inline").filter(|v| !v.is_null() && v.as_str().is_none_or(|t| !t.trim().is_empty())) {
        return statement::reading("Delimited", inline).map(|r| Some((None, r))).map_err(|m| Error::field("inline", m));
    }
    for f in formats.iter().filter(|f| !f.4) {
        if let Ok(r) = statement::reading(&f.2, &f.3)
            && r.fits(text)
        {
            return Ok(Some((Some((f.0, f.1.clone())), r)));
        }
    }
    match id(doc, "statement_format").and_then(|own| formats.iter().find(|f| f.0 == own && !f.4)) {
        Some(f) => checked(f).map(Some),
        None => Ok(None),
    }
}

/// `preview_statement` (`write` false) and `import_transactions` (`write` true), both `{text, format?, inline?}`.
/// The text is read with the format chosen (`choose`); statements the file says are another account's are left
/// out. The preview answers what the import would do (new lines, duplicates, the first lines as read) and
/// lists lines it cannot read; the import refuses the whole file for any such line, and otherwise stores each
/// new line as a submitted bank transaction, all in the action's one transaction.
async fn statement(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value, write: bool) -> Result<Value> {
    if write {
        hc.ctx.require("BankTransaction", Action::Create)?;
        hc.ctx.require("BankTransaction", Action::Submit)?;
    }
    if b(doc, "disabled") {
        return Err(Error::invalid("this bank account is disabled"));
    }
    let bank = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let text =
        params.get("text").and_then(Value::as_str).filter(|t| !t.trim().is_empty()).ok_or_else(|| Error::field("text", "give the statement file's text"))?;
    if text.len() > MAX_IMPORT_BYTES {
        return Err(Error::field("text", "the statement is larger than 5 MB; split it"));
    }
    let Some((format, reading)) = choose(hc, doc, params, text).await? else {
        return if write { Err(Error::field("format", "choose how this statement is read")) } else { Ok(json!({"format": null})) };
    };
    let format = format.map(|(fid, name)| json!({"id": fid, "name": name}));
    let read = reading.read(text).map_err(|m| Error::field("text", m))?;
    // Statements of another account are left out, when both the file and the account name one.
    let (account_name, iban, number, company): (String, Option<String>, Option<String>, Uuid) =
        sqlx::query_as("select account_name, iban, account_number, company from e_bank_account where tenant_id = $1 and id = $2")
            .bind(hc.ctx.tenant)
            .bind(bank)
            .fetch_one(&mut **hc.tx)
            .await?;
    let ours: Vec<String> = [iban, number].into_iter().flatten().filter(|n| !n.trim().is_empty()).collect();
    let (mut lines, mut other_accounts, mut foreign) = (Vec::new(), 0usize, None);
    for (account, statement_lines) in read.statements {
        match account {
            Some(a) if !ours.is_empty() && !ours.iter().any(|o| rules::same_account(&a, o)) => {
                other_accounts += 1;
                foreign.get_or_insert(a);
            }
            _ => lines.extend(statement_lines),
        }
    }
    if let Some(a) = foreign.filter(|_| lines.is_empty()) {
        return Err(Error::field("text", format!("this statement is for account {}, not {account_name}", rules::mask(&a))));
    }
    if !read.errors.is_empty() {
        if write {
            return Err(Error::Validation(read.errors.iter().take(50).map(|m| FieldError::new("text", m.clone())).collect()));
        }
        return Ok(json!({"format": format, "imported": 0, "duplicates": 0, "skipped": read.skipped, "other_accounts": other_accounts,
                         "errors": read.errors.iter().take(50).collect::<Vec<_>>(), "from_date": null, "to_date": null, "preview": []}));
    }
    let ids = statement::identities(bank, &lines);
    let hashes: Vec<String> = ids.iter().map(|(h, _)| h.clone()).collect();
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
    let fresh: Vec<(&Line, &String)> = lines.iter().zip(&ids).filter(|(_, (h, again))| !again && !existing.contains(h)).map(|(l, (h, _))| (l, h)).collect();
    let (from_date, to_date) = (fresh.iter().map(|(l, _)| l.date).min(), fresh.iter().map(|(l, _)| l.date).max());
    let duplicates = lines.len() - fresh.len();
    if !write {
        let preview: Vec<Value> = lines
            .iter()
            .zip(&ids)
            .take(10)
            .map(|(l, (h, again))| {
                json!({"date": l.date, "description": l.description, "reference": l.reference, "deposit": money(l.deposit), "withdrawal": money(l.withdrawal),
                       "duplicate": *again || existing.contains(h)})
            })
            .collect();
        return Ok(json!({"format": format, "imported": fresh.len(), "duplicates": duplicates, "skipped": read.skipped, "other_accounts": other_accounts,
                         "errors": [], "from_date": from_date, "to_date": to_date, "preview": preview}));
    }
    // Hashes are engine-set (read-only), so the lines are created with engine rights after the
    // permission checks above; the audit trail still names the user. A line another import stored
    // meanwhile fails on the unique key [bank_account, transaction_hash], and the whole import with it.
    let mut stored = Vec::new();
    for (l, hash) in &fresh {
        stored.push((store_line(hc, bank, l, hash).await?, *l));
    }
    let (ruled, rule_failures) = apply_automatic(hc, company, bank, &stored).await?;
    Ok(json!({"format": format, "imported": fresh.len(), "duplicates": duplicates, "skipped": read.skipped, "other_accounts": other_accounts,
              "from_date": from_date, "to_date": to_date, "ruled": ruled, "rule_failures": rule_failures}))
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
        // A transfer between the company's own accounts is money out of the one it leaves and money
        // into the one it goes to, each in that account's currency.
        "select p.id, p.number, p.posting_date, case when p.bank_account = $3 then p.payment_type else 'Receive' end, \
             case when p.bank_account = $3 then p.paid_amount else p.received_amount end, \
             case when p.bank_account = $3 then p.base_paid_amount else p.base_received_amount end, \
             c.code, p.party_name, p.reference_no, p.remarks \
             from e_payment_entry p left join e_currency c on c.tenant_id = p.tenant_id \
             and c.id = case when p.bank_account = $3 then p.currency else p.received_currency end \
             where p.tenant_id = $1 and p.company = $2 and p.docstatus = 1 \
             and (p.bank_account = $3 or (p.payment_type = 'Internal Transfer' and p.paid_to = $3)) and p.posting_date between $4 and $5 \
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
        let signed = if matches!(kind.as_deref(), Some("Pay" | "Internal Transfer")) { -amount } else { amount };
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

/// How much of each voucher submitted bank transactions of `bank` have already cleared (a transfer
/// is cleared once on each of its two accounts).
async fn cleared(hc: &mut HookCtx<'_>, bank: &Bank, ids: &[Uuid]) -> Result<HashMap<Uuid, Decimal>> {
    let keys: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    let rows: Vec<(String, Decimal)> = sqlx::query_as(
        "select a.payment_entry, sum(a.allocated_amount) from e_bank_transaction__allocations a \
         join e_bank_transaction t on t.tenant_id = a.tenant_id and t.id = a.parent_id \
         join e_bank_account b on b.tenant_id = t.tenant_id and b.id = t.bank_account \
         where a.tenant_id = $1 and t.docstatus = 1 and a.payment_entry = any($2) and b.account = $3 group by a.payment_entry",
    )
    .bind(hc.ctx.tenant)
    .bind(&keys)
    .bind(bank.gl)
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
            "apply_rule" => apply_rule(hc, doc, params).await?,
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
    let done = cleared(hc, &bank, &ids).await?;
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
    let done = cleared(hc, &bank, &everything.iter().map(|v| v.id).collect::<Vec<_>>()).await?;

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
    // The rule that would book a line no entry in the books matches exactly.
    let rules = rules_of(hc, bank.company.id, false).await?;
    let ruled: HashMap<Uuid, Value> = wanted
        .iter()
        .zip(&ranked)
        .zip(&open)
        .filter(|((_, cands), _)| !cands.iter().any(|c| c.exact))
        .filter_map(|((w, _), l)| {
            let fits = RuleLine { bank_account: ba, deposit: l.5, withdrawal: l.6, unreconciled: l.9.as_deref() == Some("Unreconciled"), text: &w.text };
            rule_for(&fits, &rules).map(|r| (w.id, r.hint()))
        })
        .collect();

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
                   "allocations": mine, "match": matched.get(&l.0).map(|c| c.json()), "rule": ruled.get(&l.0)})
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
        let before = cleared(hc, &bank, &[vid]).await?.get(&vid).copied().unwrap_or_default();
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

// ───────────────────────────── bank rules ─────────────────────────────

/// A Bank Rule as matching sees it.
#[derive(Debug)]
struct Rule {
    id: Uuid,
    name: String,
    bank_account: Option<Uuid>,
    priority: i64,
    money_in: bool,
    contains: Option<String>,
    pattern: Option<Regex>,
    min: Option<Decimal>,
    max: Option<Decimal>,
    journal: bool,
    account: Option<Uuid>,
    cost_center: Option<Uuid>,
    project: Option<Uuid>,
    party: Option<Uuid>,
    allocate_invoices: bool,
    /// What it books against, as people read it: the account's or the party's name.
    target: Option<String>,
}

impl Rule {
    fn hint(&self) -> Value {
        json!({"id": self.id, "name": self.name, "book_as": if self.journal { "Journal Entry" } else { "Payment Entry" }, "target": self.target})
    }
}

/// A statement line as a rule sees it.
struct RuleLine<'a> {
    bank_account: Uuid,
    deposit: Decimal,
    withdrawal: Decimal,
    unreconciled: bool,
    /// Description and reference, lower case (`Wanted.text`).
    text: &'a str,
}

/// A rule's pattern, compiled as statement formats' are: any case, linear-time, size-limited.
fn rule_pattern(p: &str) -> std::result::Result<Regex, String> {
    if p.chars().count() > 300 {
        return Err("is longer than 300 characters".into());
    }
    regex::RegexBuilder::new(p).case_insensitive(true).size_limit(1 << 20).build().map_err(|e| format!("does not compile: {e}"))
}

/// The rule that fits a wholly unreconciled line: its account (or any), direction, amount range,
/// text and pattern, each where the rule gives one; the lowest priority, then the name.
fn rule_for<'r>(line: &RuleLine, rules: &'r [Rule]) -> Option<&'r Rule> {
    if !line.unreconciled {
        return None;
    }
    let amount = line.deposit + line.withdrawal;
    let money_in = line.deposit > Decimal::ZERO;
    rules
        .iter()
        .filter(|r| r.bank_account.is_none_or(|b| b == line.bank_account) && r.money_in == money_in)
        .filter(|r| r.min.is_none_or(|m| amount >= m) && r.max.is_none_or(|m| amount <= m))
        .filter(|r| r.contains.as_deref().is_none_or(|c| line.text.contains(&c.to_lowercase())))
        .filter(|r| r.pattern.as_ref().is_none_or(|p| p.is_match(line.text)))
        .min_by(|a, b| a.priority.cmp(&b.priority).then_with(|| a.name.cmp(&b.name)))
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    id: Uuid,
    rule_name: String,
    bank_account: Option<Uuid>,
    priority: Option<i64>,
    direction: String,
    text_contains: Option<String>,
    text_pattern: Option<String>,
    amount_min: Option<Decimal>,
    amount_max: Option<Decimal>,
    book_as: String,
    account: Option<Uuid>,
    cost_center: Option<Uuid>,
    project: Option<Uuid>,
    customer: Option<Uuid>,
    supplier: Option<Uuid>,
    allocate_invoices: Option<bool>,
    target: Option<String>,
}

/// The enabled rules of `company` (only the automatic ones when `automatic`), with what each books against named.
async fn rules_of(hc: &mut HookCtx<'_>, company: Uuid, automatic: bool) -> Result<Vec<Rule>> {
    let rows: Vec<RuleRow> = sqlx::query_as(
        "select r.id, r.rule_name, r.bank_account, r.priority::int8 as priority, r.direction, r.text_contains, r.text_pattern, r.amount_min, r.amount_max, \
         r.book_as, r.account, r.cost_center, r.project, r.customer, r.supplier, r.allocate_invoices, \
         case when r.book_as = 'Journal Entry' then a.account_name when r.direction = 'Money In' then c.customer_name else p.supplier_name end as target \
         from e_bank_rule r left join e_account a on a.tenant_id = r.tenant_id and a.id = r.account \
         left join e_customer c on c.tenant_id = r.tenant_id and c.id = r.customer left join e_supplier p on p.tenant_id = r.tenant_id and p.id = r.supplier \
         where r.tenant_id = $1 and r.company = $2 and not coalesce(r.disabled, false) and (not $3 or coalesce(r.automatic, false))",
    )
    .bind(hc.ctx.tenant)
    .bind(company)
    .bind(automatic)
    .fetch_all(&mut **hc.tx)
    .await?;
    Ok(rows
        .into_iter()
        // A pattern that no longer compiles fits nothing (the hook refuses saving one).
        .filter_map(|r| {
            let pattern = match r.text_pattern.as_deref().filter(|p| !p.trim().is_empty()) {
                Some(p) => Some(rule_pattern(p).ok()?),
                None => None,
            };
            let money_in = r.direction == "Money In";
            Some(Rule {
                id: r.id,
                name: r.rule_name,
                bank_account: r.bank_account,
                priority: r.priority.unwrap_or(10),
                money_in,
                contains: r.text_contains.filter(|c| !c.trim().is_empty()),
                pattern,
                min: r.amount_min,
                max: r.amount_max,
                journal: r.book_as == "Journal Entry",
                account: r.account,
                cost_center: r.cost_center,
                project: r.project,
                party: if money_in { r.customer } else { r.supplier },
                allocate_invoices: r.allocate_invoices.unwrap_or(false),
                target: r.target,
            })
        })
        .collect())
}

/// `apply_rule {rule?}` on a submitted, wholly unreconciled line: books it as the rule says (the rule given,
/// or the first that fits) through `make_journal` or `make_payment`, as the caller, and marks the line with
/// the rule. Refused while an entry in the books matches the line exactly: the rule would count that money
/// twice.
async fn apply_rule(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    let line_id = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    // The line's row is locked for the transaction: a second caller waits, then finds it reconciled.
    let line = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "BankTransaction", line_id, true).await?;
    if d(&line, "docstatus") != Decimal::ONE || s(&line, "status") != Some("Unreconciled") {
        return Err(Error::invalid("only a submitted line with nothing reconciled yet is booked by a rule"));
    }
    let bank_account = id(&line, "bank_account").unwrap_or_default();
    let bank = bank(hc, bank_account).await?;
    let rules = rules_of(hc, bank.company.id, false).await?;
    let wanted = Wanted::of(&line)?;
    let fits = RuleLine { bank_account, deposit: d(&line, "deposit"), withdrawal: d(&line, "withdrawal"), unreconciled: true, text: &wanted.text };
    let rule = match params.get("rule").and_then(Value::as_str).filter(|v| !v.is_empty()) {
        Some(asked) => {
            let r = rules.iter().find(|r| r.id.to_string() == asked).ok_or_else(|| Error::field("rule", "choose an enabled rule of this company"))?;
            if rule_for(&fits, std::slice::from_ref(r)).is_none() {
                return Err(Error::field("rule", format!("{} does not fit this line", r.name)));
            }
            r
        }
        None => rule_for(&fits, &rules).ok_or_else(|| Error::invalid("no rule fits this line"))?,
    };
    let window = 30;
    let near = vouchers(hc, &bank, wanted.on - chrono::Duration::days(window), wanted.on + chrono::Duration::days(window), None).await?;
    let done = cleared(hc, &bank, &near.iter().map(|v| v.id).collect::<Vec<_>>()).await?;
    if let Some(c) = rank(&wanted, &near, &done, window).into_iter().find(|c| c.exact) {
        return Err(Error::invalid(format!("this line matches {} in the books: reconcile it instead", c.voucher.number.unwrap_or_default())));
    }
    let made = if rule.journal {
        let remark = format!("{} {} (rule {})", s(&line, "reference_number").unwrap_or_default(), s(&line, "description").unwrap_or_default(), rule.name);
        let params = json!({"account": rule.account, "cost_center": rule.cost_center, "project": rule.project, "remark": remark.trim()});
        make_journal(hc, &line, &params).await?
    } else {
        make_payment(hc, &line, &json!({"party": rule.party, "allocate_invoices": rule.allocate_invoices})).await?
    };
    let mut v = Doc::new();
    v.insert("rule".into(), json!(rule.id.to_string()));
    hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "BankTransaction", line_id, v).await?;
    Ok(json!({"rule": rule.id, "entity": made["entity"], "id": made["id"], "number": made["number"]}))
}

/// The number of a statement line, for saying which one a refusal is about.
async fn line_number(hc: &mut HookCtx<'_>, line: Uuid) -> Result<String> {
    let number: Option<String> = sqlx::query_scalar("select number from e_bank_transaction where tenant_id = $1 and id = $2")
        .bind(hc.ctx.tenant)
        .bind(line)
        .fetch_optional(&mut **hc.tx)
        .await?
        .flatten();
    Ok(number.unwrap_or_else(|| "a statement line".into()))
}

/// A refusal's words, for naming the line it is about.
fn said(e: &Error) -> String {
    match e {
        Error::Validation(fs) => fs.iter().map(|f| f.message.clone()).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

/// `apply_rules {lines: "[{transaction, rule}]"}`: each line through its own `apply_rule` (the caller's
/// rights on the lines count), all in the action's one transaction; the first refusal refuses the lot,
/// naming the line.
async fn apply_rules(hc: &mut HookCtx<'_>, params: &Value) -> Result<Value> {
    let list: Vec<Value> = match params.get("lines") {
        Some(Value::String(t)) => serde_json::from_str(t).map_err(|e| Error::field("lines", format!("is not a JSON list: {e}")))?,
        Some(Value::Array(a)) => a.clone(),
        _ => return Err(Error::field("lines", "list the lines and their rules")),
    };
    if list.is_empty() || list.len() > 500 {
        return Err(Error::field("lines", "1 to 500 lines at a time"));
    }
    let mut done = Vec::new();
    for (i, item) in list.iter().enumerate() {
        let uuid = |k: &str| item.get(k).and_then(Value::as_str).and_then(|x| x.parse::<Uuid>().ok());
        let line =
            uuid("transaction").ok_or_else(|| Error::Validation(vec![FieldError::new("transaction", "not a bank transaction id").in_row("lines", i)]))?;
        let rule = uuid("rule").ok_or_else(|| Error::Validation(vec![FieldError::new("rule", "not a bank rule id").in_row("lines", i)]))?;
        let out = match hc.kernel.run_action_tx(hc.ctx, hc.tx, "BankTransaction", line, "apply_rule", &json!({"rule": rule.to_string()})).await {
            Ok(out) => out,
            Err(e @ (Error::Invalid(_) | Error::Validation(_))) => return Err(Error::Invalid(format!("{}: {}", line_number(hc, line).await?, said(&e)))),
            Err(e) => return Err(e),
        };
        done.push(json!({"id": line, "rule": rule, "entity": out["entity"], "number": out["number"]}));
    }
    Ok(json!({"applied": done.len(), "lines": done}))
}

/// The automatic rules on lines just imported, each in a savepoint and as the importing person: a line a
/// rule cannot book is left to reconcile and listed, and the import goes on.
async fn apply_automatic(hc: &mut HookCtx<'_>, company: Uuid, bank_account: Uuid, lines: &[(Uuid, &Line)]) -> Result<(usize, Vec<Value>)> {
    let rules = rules_of(hc, company, true).await?;
    let (mut ruled, mut failures) = (0usize, Vec::new());
    if rules.is_empty() {
        return Ok((ruled, failures));
    }
    for (tid, l) in lines {
        let text = words(&format!("{} {}", l.description, l.reference));
        let fits = RuleLine { bank_account, deposit: l.deposit, withdrawal: l.withdrawal, unreconciled: true, text: &text };
        let Some(rule) = rule_for(&fits, &rules) else { continue };
        sqlx::query("savepoint bank_rule").execute(&mut **hc.tx).await?;
        match hc.kernel.run_action_tx(hc.ctx, hc.tx, "BankTransaction", *tid, "apply_rule", &json!({"rule": rule.id.to_string()})).await {
            Ok(_) => {
                sqlx::query("release savepoint bank_rule").execute(&mut **hc.tx).await?;
                ruled += 1;
            }
            Err(e) => {
                sqlx::query("rollback to savepoint bank_rule").execute(&mut **hc.tx).await?;
                if failures.len() < 50 {
                    let number = line_number(hc, *tid).await?;
                    failures.push(json!({"number": number, "rule": rule.name, "why": said(&e)}));
                }
            }
        }
    }
    Ok((ruled, failures))
}

// ───────────────────────────── bank feeds ─────────────────────────────

/// Stores one statement line as a submitted bank transaction of `bank` with engine rights (its hash is engine-set);
/// the caller's rights were checked by the caller. A line whose hash another transaction took meanwhile fails on the
/// unique key `[bank_account, transaction_hash]`.
async fn store_line(hc: &mut HookCtx<'_>, bank: Uuid, l: &Line, hash: &str) -> Result<Uuid> {
    let engine = elevated(hc.ctx);
    let text_or_null = |v: &str| if v.is_empty() { Value::Null } else { json!(v) };
    let mut t = Doc::new();
    t.insert("bank_account".into(), json!(bank.to_string()));
    t.insert("date".into(), json!(l.date.to_string()));
    t.insert("deposit".into(), money(l.deposit));
    t.insert("withdrawal".into(), money(l.withdrawal));
    t.insert("description".into(), json!(l.description));
    t.insert("reference_number".into(), text_or_null(&l.reference));
    t.insert("bank_reference".into(), text_or_null(&l.bank_reference));
    t.insert("transaction_hash".into(), json!(hash));
    let created = hc.kernel.insert_tx(&engine, hc.tx, "BankTransaction", t).await?;
    let tid = id(&created, "id").unwrap_or_default();
    hc.kernel.submit_tx(&engine, hc.tx, "BankTransaction", tid, None).await?;
    Ok(tid)
}

/// The bank account a feed's account reference names: `(id, company)`, not disabled.
async fn fed_account(hc: &mut HookCtx<'_>, connection: &str, account_ref: &str) -> Result<Option<(Uuid, Uuid)>> {
    Ok(sqlx::query_as(
        "select id, company from e_bank_account where tenant_id = $1 and feed_connection = $2 and feed_account_ref = $3 and not coalesce(disabled, false)",
    )
    .bind(hc.ctx.tenant)
    .bind(connection)
    .bind(account_ref)
    .fetch_optional(&mut **hc.tx)
    .await?)
}

/// Takes one feed line onto `bank`: a duplicate of a transaction already there, or a new submitted transaction with
/// the provider's id as its bank reference, the automatic rule that fits applied. `true` when it made one.
async fn take(hc: &mut HookCtx<'_>, bank: Uuid, company: Uuid, line: &Doc) -> Result<bool> {
    let fid = id(line, "id").ok_or_else(|| Error::internal("no id"))?;
    let external = s(line, "external_id").unwrap_or_default().to_string();
    let hash = rules::reference_hash(bank, &external);
    let there: bool = sqlx::query_scalar(
        "select exists (select 1 from e_bank_transaction where tenant_id = $1 and bank_account = $2 and transaction_hash = $3 and docstatus < 2)",
    )
    .bind(hc.ctx.tenant)
    .bind(bank)
    .bind(&hash)
    .fetch_one(&mut **hc.tx)
    .await?;
    let mut v = Doc::new();
    let made = if there {
        v.insert("state".into(), json!("Duplicate"));
        false
    } else {
        let date = opt_date(line, "date").ok_or_else(|| Error::field("date", "is required"))?;
        let l = Line::new(date, d(line, "amount"), s(line, "description").unwrap_or_default(), s(line, "reference").map(str::to_string), Some(external));
        let tid = store_line(hc, bank, &l, &hash).await?;
        apply_automatic(hc, company, bank, &[(tid, &l)]).await?;
        v.insert("state".into(), json!("Imported"));
        v.insert("bank_transaction".into(), json!(tid.to_string()));
        true
    };
    hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "BankFeedLine", fid, v).await?;
    Ok(made)
}

/// `take_feed_lines` on a linked bank account: its waiting feed lines, oldest first, 500 at most, each in a
/// savepoint (one that fails stays Waiting and is counted in `left`).
async fn take_feed_lines(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
    hc.ctx.require("BankTransaction", Action::Create)?;
    hc.ctx.require("BankTransaction", Action::Submit)?;
    if b(doc, "disabled") {
        return Err(Error::invalid("this bank account is disabled"));
    }
    let bank = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let company = id(doc, "company").ok_or_else(|| Error::internal("no company"))?;
    let (Some(connection), Some(account_ref)) = (s(doc, "feed_connection"), s(doc, "feed_account_ref")) else {
        return Err(Error::invalid("link this account to a feed first"));
    };
    let waiting: Vec<Uuid> = sqlx::query_scalar(
        "select id from e_bank_feed_line where tenant_id = $1 and connection = $2 and account_ref = $3 and state = 'Waiting' \
         order by date, created_at, id limit 501",
    )
    .bind(hc.ctx.tenant)
    .bind(connection)
    .bind(account_ref)
    .fetch_all(&mut **hc.tx)
    .await?;
    let (mut imported, mut duplicates, mut left) = (0usize, 0usize, waiting.len().saturating_sub(500));
    for fid in waiting.into_iter().take(500) {
        let line = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "BankFeedLine", fid, true).await?;
        if s(&line, "state") != Some("Waiting") {
            continue;
        }
        sqlx::query("savepoint feed_line").execute(&mut **hc.tx).await?;
        match take(hc, bank, company, &line).await {
            Ok(made) => {
                sqlx::query("release savepoint feed_line").execute(&mut **hc.tx).await?;
                if made { imported += 1 } else { duplicates += 1 }
            }
            Err(e) => {
                sqlx::query("rollback to savepoint feed_line").execute(&mut **hc.tx).await?;
                tracing::warn!(line = %fid, "a feed line could not be taken: {e}");
                left += 1;
            }
        }
    }
    Ok(json!({"imported": imported, "duplicates": duplicates, "left": left}))
}

/// A feed line, as a connection's pull writes it: it has an amount, and on arrival becomes a bank transaction of the
/// bank account linked to its connection and account reference (or waits for one to be linked).
pub struct BankFeedLineHook;

#[async_trait]
impl DocHook for BankFeedLineHook {
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if d(doc, "amount").is_zero() {
            return Err(Error::field("amount", "a feed line has an amount"));
        }
        Ok(())
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        if old.is_some() || s(doc, "state").is_some_and(|st| st != "Waiting") {
            return Ok(());
        }
        let (connection, account_ref) = (s(doc, "connection").unwrap_or_default().to_string(), s(doc, "account_ref").unwrap_or_default().to_string());
        let Some((bank, company)) = fed_account(hc, &connection, &account_ref).await? else { return Ok(()) };
        take(hc, bank, company, doc).await.map(|_| ())
    }
}

/// A Bank Rule's own checks: its pattern compiles (at most 300 characters), and a journal rule does not book
/// to its bank account's own GL account.
pub struct BankRuleHook;

#[async_trait]
impl DocHook for BankRuleHook {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if let Some(p) = s(doc, "text_pattern")
            && let Err(m) = rule_pattern(p)
        {
            return Err(Error::field("text_pattern", format!("the pattern {m}")));
        }
        if let (Some(ba), Some(account)) = (id(doc, "bank_account"), id(doc, "account"))
            && id(&get(hc.kernel, hc.ctx, hc.tx, "BankAccount", ba).await?, "account") == Some(account)
        {
            return Err(Error::field("account", "choose an account other than the bank's own"));
        }
        Ok(())
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn n(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }
    fn rule(name: &str, priority: i64, money_in: bool) -> Rule {
        Rule {
            id: Uuid::now_v7(),
            name: name.into(),
            bank_account: None,
            priority,
            money_in,
            contains: None,
            pattern: None,
            min: None,
            max: None,
            journal: true,
            account: None,
            cost_center: None,
            project: None,
            party: None,
            allocate_invoices: false,
            target: None,
        }
    }

    #[test]
    fn the_first_rule_that_fits_wins() {
        let (ours, other) = (Uuid::now_v7(), Uuid::now_v7());
        let fee = |text: &'static str, out: &str| RuleLine { bank_account: ours, deposit: Decimal::ZERO, withdrawal: n(out), unreconciled: true, text };
        let mut fees = rule("Bank fees", 10, false);
        fees.contains = Some("BANK FEE".into());
        fees.max = Some(n("50"));
        let mut any_fee = rule("Any fee", 20, false);
        any_fee.contains = Some("fee".into());
        let mut b_first = rule("B first", 5, false);
        b_first.contains = Some("bank fee".into());
        b_first.bank_account = Some(other);
        let mut stripe = rule("Stripe", 10, true);
        stripe.pattern = Some(rule_pattern("^stripe payout").unwrap());
        let rules = [fees, any_fee, b_first, stripe];
        let name = |l: &RuleLine| rule_for(l, &rules).map(|r| r.name.as_str());
        assert_eq!(name(&fee("bank fee march", "12.50")), Some("Bank fees"), "the lowest priority of those that fit; another account's rule does not");
        assert_eq!(name(&fee("bank fee march", "50.01")), Some("Any fee"), "above the range");
        assert_eq!(name(&fee("card fee", "5")), Some("Any fee"));
        assert_eq!(name(&fee("lunch", "5")), None);
        let mut deposit = fee("stripe payout 2026-03", "0");
        deposit.deposit = n("900");
        assert_eq!(name(&deposit), Some("Stripe"), "money in, by pattern in any case");
        deposit.text = "payout stripe";
        assert_eq!(name(&deposit), None, "the pattern is anchored");
        let mut partly = fee("bank fee", "5");
        partly.unreconciled = false;
        assert_eq!(name(&partly), None, "only a wholly unreconciled line");
        // Two of one priority: by name.
        let mut z = rule("Zeta", 1, false);
        z.min = Some(n("1"));
        let mut a = rule("Alpha", 1, false);
        a.min = Some(n("1"));
        assert_eq!(rule_for(&fee("x", "5"), &[z, a]).map(|r| r.name.as_str()), Some("Alpha"));
    }

    #[test]
    fn a_rule_needs_a_condition_and_a_pattern_that_compiles() {
        assert!(rule_pattern("^STRIPE (PAYOUT|TRANSFER)").unwrap().is_match("stripe payout"), "any case");
        assert!(rule_pattern("(unclosed").unwrap_err().contains("does not compile"));
        assert!(rule_pattern(&"a".repeat(301)).unwrap_err().contains("300"));
        assert!(rule_pattern("(\\w{1000}){1000}").unwrap_err().contains("does not compile"), "too big once compiled");
    }
}
