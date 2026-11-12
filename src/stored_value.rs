//! Stored value: gift cards, store credit and prepaid packages, built once beside the ledger so the
//! till, the web shop and Bookings use the same thing.
//!
//! A kind (`StoredValueKind`) says what its accounts hold (money, or units such as ten sessions),
//! the liability they sit in and every rule a business varies: top-up, transfer, cash-out, use in
//! part, expiry and what happens to unused value, limits, tax when sold or when redeemed, and who
//! may issue by hand. An account (`StoredValueAccount`) is one card or credit. Every change to it
//! is an entry in `stored_value_entry`, append-only, tied to the document that caused it and to
//! its journal entry; an account's `balance` (money or units) and `value` (what the books owe, in
//! the company's currency) are always the sums of its entries, which [`check`] proves along with
//! the liability account against the accounts' values.
//!
//! Codes at rest: an account keeps sha-256 of its code salted with the workspace's id, and its last
//! four characters; a PIN is kept as sha-256 of the code's hash and the PIN. A code is shown once,
//! to whoever issues it. Guessing is limited: five wrong PINs on one account in fifteen minutes, or
//! twenty unknown codes or wrong PINs by one person, and further tries are refused for that time.
//!
//! Concurrency: every change locks the account's row first and reads its balance under the lock, so
//! of two tills taking from one card at once the second waits, then sees what the first left; the
//! database also refuses any entry that would take an account below nothing.

use crate::ledger::{self, Entry, Line};
use crate::util::{Company, b, company, d, get, id, money, round, rows, s, sys};
use chrono::NaiveDate;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const ACCOUNT: &str = "StoredValueAccount";
/// Where an operation's own journal entry says it comes from (its `op_id` is the source id).
pub const SOURCE: &str = "StoredValueEntry";
/// How long wrong guesses count, and how many are let through in that time.
const GUESS_WINDOW_MINUTES: i32 = 15;
const WRONG_PINS: i64 = 5;
const WRONG_GUESSES: i64 = 20;

/// What happens to value left when an account expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnExpiry {
    StaysOwed,
    IncomeAtExpiry,
    IncomeOverTime,
}

/// A kind as the engine reads it.
#[derive(Debug, Clone)]
pub struct Kind {
    pub id: Uuid,
    pub name: String,
    pub company: Uuid,
    pub units: bool,
    pub unit_name: Option<String>,
    pub items: Vec<Uuid>,
    pub bearer: bool,
    pub liability: Uuid,
    pub income: Option<Uuid>,
    pub adjustment: Option<Uuid>,
    pub tax_at_redemption: bool,
    pub top_up: bool,
    pub transfer: bool,
    pub cash_out: bool,
    pub partial: bool,
    pub refund_back: bool,
    pub requires_pin: bool,
    pub issue_role: String,
    pub min_load: Option<Decimal>,
    pub max_load: Option<Decimal>,
    pub max_balance: Option<Decimal>,
    /// (from last use rather than from issue, days)
    pub expiry: Option<(bool, i64)>,
    pub warn_days: Option<i64>,
    pub on_expiry: OnExpiry,
    /// The expected unused share, 0 to 0.99.
    pub breakage: Decimal,
    pub disabled: bool,
}

fn opt(doc: &Doc, f: &str) -> Option<Decimal> {
    doc.get(f).filter(|v| !v.is_null()).map(|_| d(doc, f))
}

fn int(doc: &Doc, f: &str) -> Option<i64> {
    doc.get(f).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
}

pub async fn kind(k: &Kernel, ctx: &Ctx, tx: &mut Tx, kind: Uuid) -> Result<Kind> {
    let doc = get(k, ctx, tx, "StoredValueKind", kind).await?;
    let days = int(&doc, "expiry_days").filter(|n| *n > 0);
    Ok(Kind {
        id: kind,
        name: s(&doc, "kind_name").unwrap_or("Stored value").to_string(),
        company: id(&doc, "company").ok_or_else(|| Error::invalid("the stored value kind has no company"))?,
        units: s(&doc, "holds") == Some("Units"),
        unit_name: s(&doc, "unit_name").map(str::to_string),
        items: rows(&doc, "items").iter().filter_map(|r| id(r, "item")).collect(),
        bearer: s(&doc, "held_by") != Some("Customer"),
        liability: id(&doc, "liability_account").ok_or_else(|| Error::invalid("the stored value kind has no liability account"))?,
        income: id(&doc, "income_account"),
        adjustment: id(&doc, "adjustment_account"),
        tax_at_redemption: s(&doc, "tax_charged") == Some("When Redeemed"),
        top_up: b(&doc, "can_top_up"),
        transfer: b(&doc, "can_transfer"),
        cash_out: b(&doc, "can_cash_out"),
        partial: b(&doc, "partial_use"),
        refund_back: b(&doc, "refund_to_account"),
        requires_pin: b(&doc, "requires_pin"),
        issue_role: s(&doc, "issue_role").filter(|r| !r.trim().is_empty()).unwrap_or("Accounts Manager").to_string(),
        min_load: opt(&doc, "min_load").filter(|v| *v > Decimal::ZERO),
        max_load: opt(&doc, "max_load").filter(|v| *v > Decimal::ZERO),
        max_balance: opt(&doc, "max_balance").filter(|v| *v > Decimal::ZERO),
        expiry: match (s(&doc, "expiry"), days) {
            (Some("From Issue"), Some(n)) => Some((false, n)),
            (Some("From Last Use"), Some(n)) => Some((true, n)),
            _ => None,
        },
        warn_days: int(&doc, "warn_days").filter(|n| *n > 0),
        on_expiry: match s(&doc, "on_expiry") {
            Some("Income at Expiry") => OnExpiry::IncomeAtExpiry,
            Some("Income Over Time") => OnExpiry::IncomeOverTime,
            _ => OnExpiry::StaysOwed,
        },
        breakage: (d(&doc, "breakage_percent") / Decimal::ONE_HUNDRED).clamp(Decimal::ZERO, Decimal::new(99, 2)),
        disabled: b(&doc, "disabled"),
    })
}

impl Kind {
    #[allow(dead_code)] // read from redemptions and expiry on
    fn income(&self) -> Result<Uuid> {
        self.income.ok_or_else(|| Error::invalid(format!("set the Income Account of {}", self.name)))
    }
    fn adjustment(&self) -> Result<Uuid> {
        self.adjustment.ok_or_else(|| Error::invalid(format!("set the Adjustment Account of {}: value given by hand is booked against it", self.name)))
    }
    /// What a load (issue or top-up) of `amount` onto a balance of `balance` must respect.
    fn check_load(&self, balance: Decimal, amount: Decimal) -> Result<()> {
        if amount <= Decimal::ZERO {
            return Err(Error::invalid(format!("{}: what is loaded must be more than nothing", self.name)));
        }
        if let Some(min) = self.min_load.filter(|m| amount < *m) {
            return Err(Error::invalid(format!("{}: at least {} is loaded at once", self.name, min.normalize())));
        }
        if let Some(max) = self.max_load.filter(|m| amount > *m) {
            return Err(Error::invalid(format!("{}: at most {} is loaded at once", self.name, max.normalize())));
        }
        if let Some(max) = self.max_balance.filter(|m| balance + amount > *m) {
            return Err(Error::invalid(format!("{}: an account holds at most {} ({} is on it)", self.name, max.normalize(), balance.normalize())));
        }
        Ok(())
    }
    fn may_issue(&self, ctx: &Ctx) -> Result<()> {
        if ctx.is_admin || ctx.is_system || ctx.has_role(&self.issue_role) {
            Ok(())
        } else {
            Err(Error::Forbidden(format!("issuing or adjusting {} by hand is for {}", self.name, self.issue_role)))
        }
    }
}

// ───────────────────────────── codes ─────────────────────────────

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A new code: 80 random bits from the operating system's generator, as sixteen characters in four
/// groups (`7KQ2-M9XD-4TRB-H0WE`). Unguessable: a guess is right once in 2^80.
pub fn new_code() -> String {
    let (a, b) = (Uuid::new_v4().into_bytes(), Uuid::new_v4().into_bytes());
    // Bytes 0-5 of a version 4 id are random throughout; ten of them make the 80 bits.
    let mut bits: u128 = 0;
    for byte in a[..5].iter().chain(&b[..5]) {
        bits = bits << 8 | *byte as u128;
    }
    let chars: Vec<u8> = (0..16).map(|i| ALPHABET[((bits >> (5 * (15 - i))) & 31) as usize]).collect();
    chars.chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("-")
}

/// A code as typed or scanned, without spaces or dashes, in capitals.
fn normalized(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

/// What an account keeps of its code and PIN.
#[derive(Debug, Clone)]
pub struct Secret {
    pub hash: String,
    pub hint: String,
    pub pin_hash: Option<String>,
}

fn sha(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// A code's hash, salted with the workspace so the same card number elsewhere matches nothing here.
pub fn secret(tenant: Uuid, code: &str, pin: Option<&str>) -> Result<Secret> {
    let code = normalized(code);
    if code.len() < 8 {
        return Err(Error::invalid("a stored value code has at least eight letters or digits"));
    }
    let hash = sha(&format!("ferrum.stored_value:{tenant}:{code}"));
    let pin_hash = pin.map(str::trim).filter(|p| !p.is_empty()).map(|p| sha(&format!("{hash}:{p}")));
    Ok(Secret { hint: code[code.len() - 4..].to_string(), hash, pin_hash })
}

/// How a code is shown once it is kept: its last four characters.
pub fn masked(hint: &str) -> String {
    format!("••••{hint}")
}

// ───────────────────────────── accounts and entries ─────────────────────────────

/// An account as locked for a change.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Account {
    pub id: Uuid,
    pub kind: Uuid,
    pub company: Uuid,
    pub holder: Option<Uuid>,
    pub code_hint: Option<String>,
    pub code_hash: Option<String>,
    pub pin_hash: Option<String>,
    pub balance: Option<Decimal>,
    pub value: Option<Decimal>,
    pub state: Option<String>,
    pub expires_on: Option<NaiveDate>,
}

impl Account {
    pub fn balance(&self) -> Decimal {
        self.balance.unwrap_or_default()
    }
    fn value(&self) -> Decimal {
        self.value.unwrap_or_default()
    }
    fn state(&self) -> &str {
        self.state.as_deref().unwrap_or("Active")
    }
    fn label(&self, kind: &Kind) -> String {
        format!("{} {}", kind.name, masked(self.code_hint.as_deref().unwrap_or("")))
    }
    /// Refuses a use (taking value off, or loading it) unless it is active and in date.
    #[allow(dead_code)] // read from sales and redemptions on
    fn usable(&self, kind: &Kind, on: NaiveDate) -> Result<()> {
        match self.state() {
            "Blocked" => return Err(Error::invalid(format!("{} is blocked", self.label(kind)))),
            "Expired" => return Err(Error::invalid(format!("{} has expired", self.label(kind)))),
            _ => {}
        }
        if self.expires_on.is_some_and(|e| e < on) {
            return Err(Error::invalid(format!("{} expired on {}", self.label(kind), self.expires_on.unwrap_or(on))));
        }
        Ok(())
    }
    /// The value that leaves with `amount` of the balance: all of it with the whole balance, else
    /// its share, so units are valued at what was paid for them.
    fn release(&self, amount: Decimal, precision: u32) -> Decimal {
        if amount >= self.balance() { self.value() } else { round(self.value() * amount / self.balance(), precision) }
    }
}

fn table(ctx: &Ctx, entity: &str) -> Result<String> {
    Ok(ferrum_kernel::ddl::q(&ctx.schema.entity(entity).ok_or_else(|| Error::NotFound(entity.into()))?.table))
}

const COLUMNS: &str = "id, kind, company, holder, code_hint, code_hash, pin_hash, balance, value, state, expires_on";

/// Locks an account for a change: whoever changes it next waits until this transaction ends.
pub async fn lock(ctx: &Ctx, tx: &mut Tx, account: Uuid) -> Result<Account> {
    let sql = format!("select {COLUMNS} from {} where tenant_id = $1 and id = $2 for update", table(ctx, ACCOUNT)?);
    sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(ctx.tenant)
        .bind(account)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Error::NotFound("stored value account".into()))
}

/// A change to one account.
#[derive(Debug, Clone, Default)]
pub(crate) struct Move {
    pub entry_type: &'static str,
    /// Balance change: money, or units.
    pub amount: Decimal,
    /// Liability change in company currency: positive is owed more.
    pub value: Decimal,
    pub on: Option<NaiveDate>,
    pub source: Option<(String, Uuid)>,
    pub line: Option<String>,
    pub against: Option<Uuid>,
    pub journal: Option<Uuid>,
    pub reversal_of: Option<Uuid>,
    pub remarks: Option<String>,
    /// The caller's key for doing it once, and a digest of what it asked.
    pub key: Option<(String, String)>,
}

/// Appends one entry to a locked account and brings the account up to it: balance, value, last use,
/// expiry and state. Refuses to take it below nothing, telling what it holds.
pub(crate) async fn record(k: &Kernel, ctx: &Ctx, tx: &mut Tx, acc: &mut Account, kind: &Kind, op: Uuid, m: Move) -> Result<Uuid> {
    let on = m.on.unwrap_or_else(|| chrono::Utc::now().date_naive());
    let balance = acc.balance() + m.amount;
    if balance < Decimal::ZERO {
        return Err(Error::invalid(format!(
            "{} holds {}{}: {} cannot be taken from it",
            acc.label(kind),
            acc.balance().normalize(),
            kind.unit_name.as_deref().filter(|_| kind.units).map(|u| format!(" {u}")).unwrap_or_default(),
            (-m.amount).normalize()
        )));
    }
    let value = acc.value() + m.value;
    let entry = Uuid::now_v7();
    let (key, digest) = m.key.map(|(k, d)| (Some(k), Some(d))).unwrap_or_default();
    sqlx::query(
        "insert into stored_value_entry (tenant_id, id, op_id, account_id, entry_type, amount, value, posting_date, source_entity, source_id, source_line, \
         against_id, journal_entry_id, reversal_of, op_key, op_digest, remarks, created_by) \
         values ($1, $2, $3, $4, $5, $6::numeric, $7::numeric, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
    )
    .bind(ctx.tenant)
    .bind(entry)
    .bind(op)
    .bind(acc.id)
    .bind(m.entry_type)
    .bind(m.amount.to_string())
    .bind(m.value.to_string())
    .bind(on)
    .bind(m.source.as_ref().map(|s| s.0.clone()))
    .bind(m.source.as_ref().map(|s| s.1))
    .bind(m.line)
    .bind(m.against)
    .bind(m.journal)
    .bind(m.reversal_of)
    .bind(key)
    .bind(digest)
    .bind(m.remarks)
    .bind((!ctx.user.is_nil()).then_some(ctx.user))
    .execute(&mut **tx)
    .await?;
    let mut v = Doc::new();
    v.insert("balance".into(), money(balance));
    v.insert("value".into(), money(value));
    let used = matches!(m.entry_type, "issue" | "top_up" | "redeem" | "refund" | "transfer");
    if used {
        v.insert("last_used_on".into(), json!(on));
        // From last use, every use moves the expiry; from issue, only the first.
        if let Some((from_use, days)) = kind.expiry
            && (from_use || acc.expires_on.is_none())
        {
            let expires = on + chrono::Duration::days(days);
            if acc.expires_on != Some(expires) {
                v.insert("expires_on".into(), json!(expires));
                v.insert("warned_on".into(), Value::Null);
                acc.expires_on = Some(expires);
            }
        }
    }
    let state = match (m.entry_type, acc.state()) {
        ("expire", _) => "Expired",
        (_, "Active") if balance.is_zero() => "Used Up",
        (_, "Used Up") if balance > Decimal::ZERO => "Active",
        (_, st) => st,
    }
    .to_string();
    v.insert("state".into(), json!(state));
    k.set_values_tx(&sys(ctx), tx, ACCOUNT, acc.id, v).await?;
    acc.balance = Some(balance);
    acc.value = Some(value);
    acc.state = Some(state);
    Ok(entry)
}

/// Opens an account of a kind, with nothing on it yet. Without a secret it gets a new code, which is
/// returned (the only time anyone sees it).
pub(crate) async fn open(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut Tx,
    kind: &Kind,
    holder: Option<Uuid>,
    secret: Option<Secret>,
    on: NaiveDate,
) -> Result<(Account, Option<String>)> {
    if kind.disabled {
        return Err(Error::invalid(format!("{} is disabled: nothing more is issued", kind.name)));
    }
    let (secret, code) = match secret {
        Some(s) => (s, None),
        None => {
            let code = new_code();
            (crate::stored_value::secret(ctx.tenant, &code, None)?, Some(code))
        }
    };
    if kind.requires_pin && secret.pin_hash.is_none() && code.is_none() {
        return Err(Error::invalid(format!("{} is used with a PIN: give one with its code", kind.name)));
    }
    let taken: Option<Uuid> =
        sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select id from {} where tenant_id = $1 and code_hash = $2", table(ctx, ACCOUNT)?)))
            .bind(ctx.tenant)
            .bind(&secret.hash)
            .fetch_optional(&mut **tx)
            .await?;
    if taken.is_some() {
        return Err(Error::invalid(format!("the code {} is already a stored value account's", masked(&secret.hint))));
    }
    let doc = k
        .insert_tx(
            &sys(ctx),
            tx,
            ACCOUNT,
            serde_json::from_value(json!({
                "kind": kind.id, "company": kind.company, "holder": holder,
                "code_hint": secret.hint, "code_hash": secret.hash, "pin_hash": secret.pin_hash,
                "balance": "0", "value": "0", "issued_on": on, "state": "Active",
            }))
            .map_err(Error::internal)?,
        )
        .await?;
    let acc = lock(ctx, tx, id(&doc, "id").ok_or_else(|| Error::internal("no id"))?).await?;
    Ok((acc, code))
}

/// The account a code (and PIN) name, of `kind` when given; every miss counts against the person
/// and, for a wrong PIN, against the account.
pub async fn find(k: &Kernel, ctx: &Ctx, tx: &mut Tx, kind: Option<Uuid>, code: &str, pin: Option<&str>) -> Result<Uuid> {
    let guesses: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select count(*) from stored_value_attempt where tenant_id = $1 and user_id is not distinct from $2 and at > now() - interval '{GUESS_WINDOW_MINUTES} minutes'"
    )))
    .bind(ctx.tenant)
    .bind((!ctx.user.is_nil()).then_some(ctx.user))
    .fetch_one(&mut **tx)
    .await?;
    if guesses >= WRONG_GUESSES {
        return Err(Error::TooManyRequests(format!("too many codes or PINs that did not match: try again in {GUESS_WINDOW_MINUTES} minutes")));
    }
    let sec = secret(ctx.tenant, code, pin)?;
    let found: Option<(Uuid, Uuid, Option<String>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!("select id, kind, pin_hash from {} where tenant_id = $1 and code_hash = $2", table(ctx, ACCOUNT)?)))
            .bind(ctx.tenant)
            .bind(&sec.hash)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((acc, _, pin_hash)) = found.filter(|(_, kd, _)| kind.is_none_or(|want| want == *kd)) else {
        missed(k, ctx, None).await?;
        return Err(Error::invalid("no stored value account of this kind has this code"));
    };
    if let Some(want) = pin_hash {
        let wrong: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select count(*) from stored_value_attempt where tenant_id = $1 and account_id = $2 and at > now() - interval '{GUESS_WINDOW_MINUTES} minutes'"
        )))
        .bind(ctx.tenant)
        .bind(acc)
        .fetch_one(&mut **tx)
        .await?;
        if wrong >= WRONG_PINS {
            return Err(Error::TooManyRequests(format!("too many wrong PINs for this card: try again in {GUESS_WINDOW_MINUTES} minutes")));
        }
        if sec.pin_hash.as_deref() != Some(want.as_str()) {
            missed(k, ctx, Some(acc)).await?;
            return Err(Error::invalid(if pin.is_none_or(|p| p.trim().is_empty()) { "this card needs its PIN" } else { "the PIN is wrong" }));
        }
    }
    Ok(acc)
}

/// A miss, kept whatever becomes of the caller's transaction (which a refusal rolls back).
async fn missed(k: &Kernel, ctx: &Ctx, account: Option<Uuid>) -> Result<()> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    sqlx::query("insert into stored_value_attempt (tenant_id, account_id, user_id) values ($1, $2, $3)")
        .bind(ctx.tenant)
        .bind(account)
        .bind((!ctx.user.is_nil()).then_some(ctx.user))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Posts an operation's own journal entry, its source the operation.
async fn post(ctx: &Ctx, tx: &mut Tx, co: &Company, on: NaiveDate, op: Uuid, memo: &str, lines: Vec<Line>) -> Result<Option<Uuid>> {
    let mut e = Entry::new(co.id, on, SOURCE, op, None);
    e.memo = Some(memo.to_string());
    e.lines = lines;
    ledger::post(tx, ctx, e, co.precision).await
}

/// A caller's key used before: what was done then, or a refusal when it asked something else.
async fn repeated(ctx: &Ctx, tx: &mut Tx, key: Option<&str>, digest: &str) -> Result<Option<Value>> {
    let Some(key) = key else { return Ok(None) };
    let was: Option<(Uuid, Uuid, Option<String>)> =
        sqlx::query_as("select op_id, account_id, op_digest from stored_value_entry where tenant_id = $1 and op_key = $2")
            .bind(ctx.tenant)
            .bind(key)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((op, account, was_digest)) = was else { return Ok(None) };
    if was_digest.as_deref() != Some(digest) {
        return Err(Error::Conflict(format!("the key {key} was used for another stored value request: use a new key for a new request")));
    }
    let acc = lock(ctx, tx, account).await?;
    Ok(Some(json!({"account": account, "entry": op, "balance": money(acc.balance()), "repeated": true})))
}

fn digest(parts: &Value) -> String {
    sha(&parts.to_string())
}

// ───────────────────────────── by hand ─────────────────────────────

/// Value issued without a sale (goodwill, a prize), by someone the kind allows: money is booked
/// against the kind's Adjustment Account; units carry no value in the books until they are used.
/// Returns the account, the entry and, when it made one, the new code.
#[allow(clippy::too_many_arguments)]
pub async fn issue(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut Tx,
    kind_id: Uuid,
    amount: Decimal,
    reason: &str,
    holder: Option<Uuid>,
    code: Option<&str>,
    pin: Option<&str>,
    key: Option<&str>,
) -> Result<Value> {
    let kind = kind(k, ctx, tx, kind_id).await?;
    kind.may_issue(ctx)?;
    let dg = digest(&json!(["issue", kind_id, amount.normalize().to_string(), reason, holder, code.map(normalized)]));
    if let Some(done) = repeated(ctx, tx, key, &dg).await? {
        return Ok(done);
    }
    if reason.trim().is_empty() {
        return Err(Error::field("reason", "say why it is issued without a sale"));
    }
    kind.check_load(Decimal::ZERO, amount)?;
    let co = company(k, ctx, tx, kind.company).await?;
    let on = chrono::Utc::now().date_naive();
    let sec = code.map(|c| secret(ctx.tenant, c, pin)).transpose()?;
    let (mut acc, made) = open(k, ctx, tx, &kind, holder, sec, on).await?;
    let value = if kind.units { Decimal::ZERO } else { round(amount, co.precision) };
    let op = Uuid::now_v7();
    let journal = if value.is_zero() {
        None
    } else {
        let lines =
            vec![Line::new(kind.adjustment()?, value, &co.currency_code).remarks(reason), Line::new(kind.liability, -value, &co.currency_code).remarks(reason)];
        post(ctx, tx, &co, on, op, &format!("{} issued by hand", acc.label(&kind)), lines).await?
    };
    let m = Move {
        entry_type: "issue",
        amount,
        value,
        on: Some(on),
        source: Some(("StoredValueKind".into(), kind.id)),
        journal,
        remarks: Some(reason.to_string()),
        key: key.map(|k| (k.to_string(), dg)),
        ..Default::default()
    };
    record(k, ctx, tx, &mut acc, &kind, op, m).await?;
    Ok(json!({"account": acc.id, "entry": op, "code": made, "balance": money(acc.balance())}))
}

/// A correction by hand, either way: money against the Adjustment Account; units change the count
/// and, taken away, the value that leaves with them.
pub async fn adjust(k: &Kernel, ctx: &Ctx, tx: &mut Tx, account: Uuid, amount: Decimal, reason: &str) -> Result<Value> {
    let mut acc = lock(ctx, tx, account).await?;
    let kind = kind(k, ctx, tx, acc.kind).await?;
    kind.may_issue(ctx)?;
    if amount.is_zero() || reason.trim().is_empty() {
        return Err(Error::invalid("an adjustment needs an amount and a reason"));
    }
    let co = company(k, ctx, tx, kind.company).await?;
    let value = match (amount > Decimal::ZERO, kind.units) {
        (true, true) => Decimal::ZERO,
        (true, false) => round(amount, co.precision),
        (false, _) => -acc.release(-amount, co.precision),
    };
    let on = chrono::Utc::now().date_naive();
    let op = Uuid::now_v7();
    let lines =
        vec![Line::new(kind.adjustment()?, value, &co.currency_code).remarks(reason), Line::new(kind.liability, -value, &co.currency_code).remarks(reason)];
    let journal = if value.is_zero() { None } else { post(ctx, tx, &co, on, op, &format!("{} adjusted", acc.label(&kind)), lines).await? };
    let m = Move {
        entry_type: "adjust",
        amount,
        value,
        on: Some(on),
        source: Some((ACCOUNT.into(), acc.id)),
        journal,
        remarks: Some(reason.to_string()),
        ..Default::default()
    };
    record(k, ctx, tx, &mut acc, &kind, op, m).await?;
    Ok(json!({"account": acc.id, "entry": op, "balance": money(acc.balance())}))
}

// ───────────────────────────── the check ─────────────────────────────

/// Every way the books and the accounts disagree: an account whose balance or value is not the sum
/// of its entries, or a liability account whose balance in the common books is not the sum of the
/// values of the accounts of the kinds that use it. Empty when they agree.
pub async fn check(ctx: &Ctx, tx: &mut Tx) -> Result<Vec<String>> {
    let (accounts, kinds) = (table(ctx, ACCOUNT)?, table(ctx, "StoredValueKind")?);
    let mut out: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select format('account %s (%s): balance %s, value %s, entries %s and %s', a.id, a.code_hint, coalesce(a.balance, 0), coalesce(a.value, 0), \
         coalesce(e.amount, 0), coalesce(e.value, 0)) from {accounts} a \
         left join (select account_id, sum(amount) as amount, sum(value) as value from stored_value_entry where tenant_id = $1 group by 1) e on e.account_id = a.id \
         where a.tenant_id = $1 and (coalesce(a.balance, 0) <> coalesce(e.amount, 0) or coalesce(a.value, 0) <> coalesce(e.value, 0))"
    )))
    .bind(ctx.tenant)
    .fetch_all(&mut **tx)
    .await?;
    let ledger: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "with owed as (select k.company, k.liability_account, sum(coalesce(a.value, 0)) as value from {kinds} k \
           left join {accounts} a on a.tenant_id = k.tenant_id and a.kind = k.id where k.tenant_id = $1 group by 1, 2), \
         books as (select l.company_id, l.account_id, sum(l.credit - l.debit) as value from journal_line l \
           where l.tenant_id = $1 and l.finance_book_id is null and (l.company_id, l.account_id) in (select company, liability_account from owed) group by 1, 2) \
         select format('liability account %s: %s in the books, %s on the accounts', o.liability_account, coalesce(b.value, 0), o.value) \
         from owed o left join books b on b.company_id = o.company and b.account_id = o.liability_account where coalesce(b.value, 0) <> o.value"
    )))
    .bind(ctx.tenant)
    .fetch_all(&mut **tx)
    .await?;
    out.extend(ledger);
    Ok(out)
}

// ───────────────────────────── hooks ─────────────────────────────

fn param<'a>(params: &'a Value, name: &str) -> Option<&'a str> {
    params.get(name).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

fn amount_param(params: &Value, name: &str) -> Result<Option<Decimal>> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.to_string().parse().map(Some).map_err(|_| Error::field(name, "is not a number")),
        Some(Value::String(t)) if t.trim().is_empty() => Ok(None),
        Some(Value::String(t)) => t.trim().parse().map(Some).map_err(|_| Error::field(name, "is not a number")),
        _ => Err(Error::field(name, "is not a number")),
    }
}

fn uuid_param(params: &Value, name: &str) -> Result<Option<Uuid>> {
    param(params, name).map(|v| v.parse().map_err(|_| Error::field(name, "is not a record id"))).transpose()
}

/// A kind's capabilities (its contracted actions).
pub struct KindHook;

#[async_trait::async_trait]
impl DocHook for KindHook {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let cid = id(doc, "company");
        for f in ["liability_account", "income_account", "adjustment_account"] {
            if let Some(acc) = id(doc, f)
                && id(&get(hc.kernel, hc.ctx, hc.tx, "Account", acc).await?, "company") != cid
            {
                return Err(Error::field(f, "belongs to another company"));
            }
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let kind_id = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let (k, ctx) = (hc.kernel, hc.ctx);
        let out = match name {
            "balance" => {
                let code = param(params, "code").ok_or_else(|| Error::field("code", "is required"))?;
                let acc = find(k, ctx, hc.tx, Some(kind_id), code, param(params, "pin")).await?;
                let sql = format!("select {COLUMNS} from {} where tenant_id = $1 and id = $2", table(ctx, ACCOUNT)?);
                let a: Account = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(ctx.tenant).bind(acc).fetch_one(&mut **hc.tx).await?;
                json!({"account": a.id, "balance": money(a.balance()), "holds": if s(doc, "holds") == Some("Units") { "Units" } else { "Money" },
                       "state": a.state(), "expires_on": a.expires_on})
            }
            "issue" => {
                let amount = amount_param(params, "amount")?.ok_or_else(|| Error::field("amount", "is required"))?;
                let holder = uuid_param(params, "holder")?;
                issue(k, ctx, hc.tx, kind_id, amount, param(params, "reason").unwrap_or_default(), holder, param(params, "code"), param(params, "pin"), None)
                    .await?
            }
            _ => return Ok(None),
        };
        Ok(Some(out))
    }
}

/// An account's own actions.
pub struct AccountHook;

#[async_trait::async_trait]
impl DocHook for AccountHook {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let account = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let (k, ctx) = (hc.kernel, hc.ctx);
        let out = match name {
            "block" | "unblock" => {
                let acc = lock(ctx, hc.tx, account).await?;
                let (from, to) = if name == "block" { ("Active", "Blocked") } else { ("Blocked", "Active") };
                if acc.state() != from {
                    return Err(Error::invalid(format!("it is {}, not {from}", acc.state())));
                }
                let mut v = Doc::new();
                v.insert("state".into(), json!(if to == "Active" && acc.balance().is_zero() { "Used Up" } else { to }));
                k.set_values_tx(&sys(ctx), hc.tx, ACCOUNT, account, v).await?;
                json!({"account": account, "state": to})
            }
            "new_code" => {
                let acc = lock(ctx, hc.tx, account).await?;
                let kind = kind(k, ctx, hc.tx, acc.kind).await?;
                let pin = param(params, "pin");
                if kind.requires_pin && pin.is_none() {
                    return Err(Error::field("pin", format!("{} is used with a PIN: give the new one", kind.name)));
                }
                let code = new_code();
                let sec = secret(ctx.tenant, &code, pin)?;
                let mut v = Doc::new();
                v.insert("code_hint".into(), json!(sec.hint));
                v.insert("code_hash".into(), json!(sec.hash));
                v.insert("pin_hash".into(), json!(sec.pin_hash));
                k.set_values_tx(&sys(ctx), hc.tx, ACCOUNT, account, v).await?;
                json!({"account": account, "code": code})
            }
            "adjust" => {
                let amount = amount_param(params, "amount")?.ok_or_else(|| Error::field("amount", "is required"))?;
                adjust(k, ctx, hc.tx, account, amount, param(params, "reason").unwrap_or_default()).await?
            }
            _ => return Ok(None),
        };
        Ok(Some(out))
    }
}
