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
    /// What someone taking more than it holds is told.
    fn short(&self, kind: &Kind, amount: Decimal) -> Error {
        Error::invalid(format!(
            "{} holds {}{}: {} cannot be taken from it",
            self.label(kind),
            self.balance().normalize(),
            kind.unit_name.as_deref().filter(|_| kind.units).map(|u| format!(" {u}")).unwrap_or_default(),
            amount.normalize()
        ))
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
        return Err(acc.short(kind, -m.amount));
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
    let sec = secret(ctx.tenant, code, pin)?;
    match lookup(k, ctx, tx, kind, &sec).await? {
        Some(acc) => Ok(acc),
        None => {
            missed(k, ctx, None).await?;
            Err(Error::invalid("no stored value account of this kind has this code"))
        }
    }
}

/// The account a kept code names, with the guessing limits and its PIN checked; `None` when no
/// account (of `kind`) has it, which the caller counts as a miss or not.
async fn lookup(k: &Kernel, ctx: &Ctx, tx: &mut Tx, kind: Option<Uuid>, sec: &Secret) -> Result<Option<Uuid>> {
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
    let found: Option<(Uuid, Uuid, Option<String>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!("select id, kind, pin_hash from {} where tenant_id = $1 and code_hash = $2", table(ctx, ACCOUNT)?)))
            .bind(ctx.tenant)
            .bind(&sec.hash)
            .fetch_optional(&mut **tx)
            .await?;
    let Some((acc, _, pin_hash)) = found.filter(|(_, kd, _)| kind.is_none_or(|want| want == *kd)) else { return Ok(None) };
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
            return Err(Error::invalid(if sec.pin_hash.is_none() { "this card needs its PIN" } else { "the PIN is wrong" }));
        }
    }
    Ok(Some(acc))
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

// ───────────────────────────── selling it ─────────────────────────────

/// Prices a sales line whose item sells stored value: it goes to the kind's liability instead of
/// income, never deferred. Returns whether the line is left untaxed (tax charged when redeemed).
pub(crate) async fn price_line(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut Tx,
    item: &Doc,
    row: &mut Doc,
    kinds: &mut std::collections::HashMap<Uuid, Kind>,
) -> Result<bool> {
    let Some(kid) = id(item, "stored_value_kind") else {
        crate::util::set(row, "stored_value_kind", Value::Null);
        return Ok(false);
    };
    if let std::collections::hash_map::Entry::Vacant(e) = kinds.entry(kid) {
        e.insert(kind(k, ctx, tx, kid).await?);
    }
    let kd = &kinds[&kid];
    crate::util::set(row, "stored_value_kind", kid.to_string());
    crate::util::set(row, "income_account", kd.liability.to_string());
    crate::util::set(row, "enable_deferred_revenue", false);
    Ok(kd.tax_at_redemption)
}

/// A line's own rates when its tax is charged on redemption: nothing for each of the document's taxes.
pub(crate) fn untaxed_rates(taxes: &[Doc]) -> Vec<Doc> {
    taxes
        .iter()
        .filter_map(|t| {
            let acc = s(t, "account")?;
            serde_json::from_value(json!({"tax_account": acc, "rate": "0", "exemption_reason": "Stored value: taxed when it is redeemed"})).ok()
        })
        .collect()
}

/// A code typed on a line or a payment is replaced, before anything is stored, by its last four
/// characters, and kept as a hash in `secret_field` (`hash:hint:pin hash`); a PIN is not kept.
fn mask(ctx: &Ctx, row: &mut Doc, secret_field: &str, at: &str) -> Result<()> {
    let typed_pin = s(row, "stored_value_pin").map(str::to_string);
    match s(row, "stored_value_code").map(str::to_string) {
        Some(code) if !code.starts_with('•') => {
            let sec = secret(ctx.tenant, &code, typed_pin.as_deref())?;
            crate::util::set(row, secret_field, format!("{}:{}:{}", sec.hash, sec.hint, sec.pin_hash.unwrap_or_default()));
            crate::util::set(row, "stored_value_code", masked(&sec.hint));
        }
        Some(_) if typed_pin.is_some() => return Err(Error::field(at, "type the card's code again with its PIN")),
        Some(_) => {}
        None => crate::util::set(row, secret_field, Value::Null),
    }
    if row.contains_key("stored_value_pin") {
        crate::util::set(row, "stored_value_pin", Value::Null);
    }
    Ok(())
}

fn kept_secret(row: &Doc, field: &str) -> Option<Secret> {
    let mut parts = s(row, field)?.splitn(3, ':');
    let (hash, hint, pin) = (parts.next()?, parts.next()?, parts.next().unwrap_or(""));
    Some(Secret { hash: hash.into(), hint: hint.into(), pin_hash: (!pin.is_empty()).then(|| pin.to_string()) })
}

/// Hides the codes typed on a sales invoice's lines (before the draft is stored).
pub(crate) fn mask_lines(ctx: &Ctx, doc: &mut Doc) -> Result<()> {
    let mut items = rows(doc, "items");
    if !items.iter().any(|r| r.contains_key("stored_value_code") || r.contains_key("stored_value_pin")) {
        return Ok(());
    }
    for (i, r) in items.iter_mut().enumerate() {
        mask(ctx, r, "stored_value_secret", &format!("items.{i}.stored_value_code"))?;
    }
    crate::util::set(doc, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
    Ok(())
}

/// What one sales line loads: its booked value for money, its quantity times the item's units for units.
async fn line_load(k: &Kernel, ctx: &Ctx, tx: &mut Tx, kd: &Kind, row: &Doc, base: Decimal, i: usize) -> Result<Decimal> {
    if !kd.units {
        return Ok(base);
    }
    let per = match id(row, "item") {
        Some(item) => d(&get(k, ctx, tx, "Item", item).await?, "stored_value_units"),
        None => Decimal::ZERO,
    };
    if per <= Decimal::ZERO {
        return Err(Error::field(
            format!("items.{i}.item"),
            format!("set the item's Units Loaded: how many {} one of it loads", kd.unit_name.as_deref().unwrap_or("units")),
        ));
    }
    Ok(crate::util::stock_qty(row) * per)
}

/// A sales invoice's stored value lines are loadable: not on a return, of the invoice's company,
/// onto an account that can take it, within the kind's limits.
pub(crate) async fn check_invoice(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
    let bk = crate::pricing::breakup(doc);
    for (i, (row, base)) in bk.lines.iter().enumerate() {
        let Some(kid) = id(row, "stored_value_kind") else { continue };
        let at = format!("items.{i}.item");
        if b(doc, "is_return") || b(doc, "is_debit_note") {
            return Err(Error::field(
                at,
                "stored value is not credited or charged on a note: take it off its account (cash out or adjust), or cancel the sale while it is unused",
            ));
        }
        let kd = kind(hc.kernel, hc.ctx, hc.tx, kid).await?;
        if Some(kd.company) != id(doc, "company") {
            return Err(Error::field(at, format!("{} is another company's", kd.name)));
        }
        let amount = line_load(hc.kernel, hc.ctx, hc.tx, &kd, row, *base, i).await?;
        match id(row, "stored_value_account") {
            Some(acc) => {
                let a = get(hc.kernel, hc.ctx, hc.tx, ACCOUNT, acc).await?;
                if id(&a, "kind") != Some(kid) {
                    return Err(Error::field(format!("items.{i}.stored_value_account"), format!("is not a {}", kd.name)));
                }
                if !kd.top_up {
                    return Err(Error::field(format!("items.{i}.stored_value_account"), format!("{} cannot be topped up: sell a new one", kd.name)));
                }
                if matches!(s(&a, "state"), Some("Blocked" | "Expired")) {
                    return Err(Error::field(format!("items.{i}.stored_value_account"), format!("is {}", s(&a, "state").unwrap_or_default().to_lowercase())));
                }
                kd.check_load(d(&a, "balance"), amount)?;
            }
            None => {
                if kd.disabled {
                    return Err(Error::field(at, format!("{} is disabled: nothing more is issued", kd.name)));
                }
                if s(row, "stored_value_code").is_some() && s(row, "stored_value_secret").is_none() {
                    return Err(Error::field(format!("items.{i}.stored_value_code"), "type the card's code again"));
                }
                if kd.requires_pin && kept_secret(row, "stored_value_secret").is_some_and(|x| x.pin_hash.is_none()) {
                    return Err(Error::field(format!("items.{i}.stored_value_pin"), format!("{} is used with a PIN", kd.name)));
                }
                kd.check_load(Decimal::ZERO, amount)?;
            }
        }
    }
    Ok(())
}

async fn journal_of(ctx: &Ctx, tx: &mut Tx, entity: &str, source: Uuid) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar(
        "select id from journal_entry where tenant_id = $1 and source_entity = $2 and source_id = $3 and reversal_of is null order by created_at limit 1",
    )
    .bind(ctx.tenant)
    .bind(entity)
    .bind(source)
    .fetch_optional(&mut **tx)
    .await?)
}

/// A submitted sales invoice opens or loads an account per stored value line, at what the line was
/// booked at (its share of the document's discount included), tied to the invoice's posting.
pub(crate) async fn invoice_submitted(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
    let iid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let on = crate::util::date(doc, "posting_date")?;
    let bk = crate::pricing::breakup(doc);
    let mut journal = None;
    for (i, (row, base)) in bk.lines.iter().enumerate() {
        let Some(kid) = id(row, "stored_value_kind") else { continue };
        let (k, ctx) = (hc.kernel, hc.ctx);
        if journal.is_none() {
            journal = journal_of(ctx, hc.tx, "SalesInvoice", iid).await?;
        }
        let kd = kind(k, ctx, hc.tx, kid).await?;
        let amount = line_load(k, ctx, hc.tx, &kd, row, *base, i).await?;
        let (mut acc, entry_type) = match id(row, "stored_value_account") {
            Some(a) => {
                let acc = lock(ctx, hc.tx, a).await?;
                acc.usable(&kd, on)?;
                kd.check_load(acc.balance(), amount)?;
                (acc, "top_up")
            }
            None => {
                let holder = if kd.bearer { None } else { id(doc, "customer") };
                (open(k, ctx, hc.tx, &kd, holder, kept_secret(row, "stored_value_secret"), on).await?.0, "issue")
            }
        };
        let m = Move {
            entry_type,
            amount,
            value: *base,
            on: Some(on),
            source: Some(("SalesInvoice".into(), iid)),
            line: s(row, "id").map(str::to_string),
            journal,
            remarks: s(doc, "number").map(str::to_string),
            ..Default::default()
        };
        record(k, ctx, hc.tx, &mut acc, &kd, Uuid::now_v7(), m).await?;
    }
    Ok(())
}

/// Entries a document made that are not yet reversed: (entry, account, amount, value).
async fn live_entries(ctx: &Ctx, tx: &mut Tx, entity: &str, source: Uuid) -> Result<Vec<(Uuid, Uuid, Decimal, Decimal)>> {
    Ok(sqlx::query_as(
        "select e.id, e.account_id, e.amount, e.value from stored_value_entry e where e.tenant_id = $1 and e.source_entity = $2 and e.source_id = $3 \
         and e.entry_type <> 'reverse' and not exists (select 1 from stored_value_entry r where r.tenant_id = e.tenant_id and r.reversal_of = e.id) order by e.created_at",
    )
    .bind(ctx.tenant)
    .bind(entity)
    .bind(source)
    .fetch_all(&mut **tx)
    .await?)
}

/// Takes back what a document did to its accounts, each entry by its own reversing entry tied to
/// `journal` (the document's reversal). What was loaded and has been spent since cannot be taken
/// back: the refusal says what is left.
pub(crate) async fn reverse_entries(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, source: Uuid, on: NaiveDate, what: &str) -> Result<usize> {
    let entries = live_entries(ctx, tx, entity, source).await?;
    let journal: Option<Uuid> = sqlx::query_scalar(
        "select r.id from journal_entry r join journal_entry e on e.tenant_id = r.tenant_id and e.id = r.reversal_of \
         where e.tenant_id = $1 and e.source_entity = $2 and e.source_id = $3 order by r.created_at desc limit 1",
    )
    .bind(ctx.tenant)
    .bind(entity)
    .bind(source)
    .fetch_optional(&mut **tx)
    .await?;
    for (entry, account, amount, value) in &entries {
        let mut acc = lock(ctx, tx, *account).await?;
        let kd = kind(k, ctx, tx, acc.kind).await?;
        if acc.balance() < *amount {
            return Err(Error::invalid(format!(
                "{} was loaded with {} by {what} and has been used since: it holds {}. Value already spent cannot be taken back; leave {what} standing, and cash out or adjust what is left if it must go",
                acc.label(&kd),
                amount.normalize(),
                acc.balance().normalize()
            )));
        }
        let m = Move {
            entry_type: "reverse",
            amount: -*amount,
            value: -*value,
            on: Some(on),
            source: Some((entity.to_string(), source)),
            journal,
            reversal_of: Some(*entry),
            remarks: Some(format!("Reversal: {what} cancelled")),
            ..Default::default()
        };
        record(k, ctx, tx, &mut acc, &kd, Uuid::now_v7(), m).await?;
    }
    Ok(entries.len())
}

// ───────────────────────────── paying with it ─────────────────────────────

/// Whether a payment entry takes from or gives back onto stored value.
pub(crate) fn pays(doc: &Doc) -> bool {
    id(doc, "stored_value_account").is_some() || id(doc, "stored_value_kind").is_some()
}

/// A payment's card: a code typed in this save finds its account (PIN and guessing limits checked)
/// and is hidden; the kind's liability account takes the bank account's place.
pub(crate) async fn payment_fill(hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
    let fresh = s(doc, "stored_value_code").is_some_and(|c| !c.starts_with('•'));
    mask(hc.ctx, doc, "stored_value_secret", "stored_value_code")?;
    if fresh && let Some(sec) = kept_secret(doc, "stored_value_secret") {
        match lookup(hc.kernel, hc.ctx, hc.tx, None, &sec).await? {
            Some(acc) => {
                crate::util::set(doc, "stored_value_account", acc.to_string());
                crate::util::set(doc, "stored_value_secret", Value::Null);
            }
            // A new card's code, for a refund into new store credit.
            None if b(doc, "is_refund") && id(doc, "stored_value_kind").is_some() => {}
            None => {
                missed(hc.kernel, hc.ctx, None).await?;
                return Err(Error::field("stored_value_code", "no stored value account has this code"));
            }
        }
    }
    let kid = match id(doc, "stored_value_account") {
        Some(acc) => id(&get(hc.kernel, hc.ctx, hc.tx, ACCOUNT, acc).await?, "kind"),
        None => id(doc, "stored_value_kind"),
    };
    if let Some(kid) = kid {
        let kd = kind(hc.kernel, hc.ctx, hc.tx, kid).await?;
        crate::util::set(doc, "bank_account", kd.liability.to_string());
    }
    Ok(())
}

/// The invoice a units redemption pays: its lines of the kind's items, less what was redeemed on it.
async fn covered(ctx: &Ctx, tx: &mut Tx, k: &Kernel, kd: &Kind, invoice: Uuid) -> Result<Decimal> {
    let inv = get(k, ctx, tx, "SalesInvoice", invoice).await?;
    let lines: Decimal = rows(&inv, "items").iter().filter(|r| id(r, "item").is_some_and(|i| kd.items.contains(&i))).map(crate::util::stock_qty).sum();
    let used: Decimal = sqlx::query_scalar(
        "select coalesce(-sum(e.amount), 0) from stored_value_entry e where e.tenant_id = $1 and e.against_id = $2 and e.entry_type = 'redeem' \
         and not exists (select 1 from stored_value_entry r where r.tenant_id = e.tenant_id and r.reversal_of = e.id)",
    )
    .bind(ctx.tenant)
    .bind(invoice)
    .fetch_one(&mut **tx)
    .await?;
    Ok(lines - used)
}

/// A stored value payment: a customer's receipt that pays their invoices from an account, or a
/// refund to a customer back onto one (or onto new store credit), in the company's currency.
pub(crate) async fn check_payment(hc: &mut HookCtx<'_>, doc: &Doc, co: &Company) -> Result<()> {
    if !pays(doc) {
        return Ok(());
    }
    let refund = b(doc, "is_refund");
    let redeem = s(doc, "payment_type") == Some("Receive") && !refund;
    if !(redeem || (s(doc, "payment_type") == Some("Pay") && refund)) || id(doc, "customer").is_none() {
        return Err(Error::field("stored_value_account", "stored value pays a customer's invoices, or takes back a refund to a customer"));
    }
    if id(doc, "currency") != Some(co.currency_id) {
        return Err(Error::field("currency", format!("stored value is paid in the company's currency, {}", co.currency_code)));
    }
    if !rows(doc, "deductions").is_empty() {
        return Err(Error::field("deductions", "a stored value payment has no deductions"));
    }
    if d(doc, "unallocated_amount") > Decimal::ZERO {
        return Err(Error::field("references", "stored value pays invoices: allocate all of it"));
    }
    let on = crate::util::date(doc, "posting_date")?;
    let (acc, kd) = match id(doc, "stored_value_account") {
        Some(a) => {
            let sql = format!("select {COLUMNS} from {} where tenant_id = $1 and id = $2", table(hc.ctx, ACCOUNT)?);
            let acc: Account = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(hc.ctx.tenant).bind(a).fetch_one(&mut **hc.tx).await?;
            let kd = kind(hc.kernel, hc.ctx, hc.tx, acc.kind).await?;
            acc.usable(&kd, on)?;
            (Some(acc), kd)
        }
        None => (None, kind(hc.kernel, hc.ctx, hc.tx, id(doc, "stored_value_kind").unwrap_or_default()).await?),
    };
    if kd.company != co.id {
        return Err(Error::field("stored_value_account", format!("{} is another company's", kd.name)));
    }
    let units = d(doc, "stored_value_units");
    if kd.units && units <= Decimal::ZERO {
        return Err(Error::field("stored_value_units", format!("how many {} are taken or given back", kd.unit_name.as_deref().unwrap_or("units"))));
    }
    let amount = if kd.units { units } else { d(doc, "paid_amount") };
    if let (true, Some(acc)) = (redeem, &acc) {
        if amount > acc.balance() {
            return Err(Error::field("paid_amount", format!("{} holds {}", acc.label(&kd), acc.balance().normalize())));
        }
        if !kd.partial && amount != acc.balance() {
            return Err(Error::field("paid_amount", format!("{} is used whole: take {} from it", acc.label(&kd), acc.balance().normalize())));
        }
        if kd.units {
            let refs = rows(doc, "references");
            let [r] = refs.as_slice() else { return Err(Error::field("references", "units pay one invoice at a time")) };
            let inv = id(r, "invoice").ok_or_else(|| Error::field("references", "name the invoice"))?;
            let open = covered(hc.ctx, hc.tx, hc.kernel, &kd, inv).await?;
            if units > open {
                return Err(Error::field("stored_value_units", format!("the invoice has {} of {}'s items left to pay with units", open.normalize(), kd.name)));
            }
        }
    }
    if refund && acc.is_none() && kd.requires_pin && kept_secret(doc, "stored_value_secret").is_none_or(|x| x.pin_hash.is_none()) {
        return Err(Error::field("stored_value_pin", format!("{} is used with a PIN: give the new card's code and PIN", kd.name)));
    }
    Ok(())
}

/// A submitted stored value payment, under the account's lock. A receipt takes from the account the
/// value its amount carries (units at what they were bought for); the payment posted the money to
/// the liability, so the difference goes to the kind's Income Account, and so does the expected
/// unused share a kind recognising it over time sets aside. A refund loads the account, or a new
/// one held by the customer.
pub(crate) async fn payment_submitted(hc: &mut HookCtx<'_>, doc: &Doc, co: &Company) -> Result<()> {
    if !pays(doc) {
        return Ok(());
    }
    let (k, ctx) = (hc.kernel, hc.ctx);
    let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let on = crate::util::date(doc, "posting_date")?;
    let paid = d(doc, "base_paid_amount");
    let journal = journal_of(ctx, hc.tx, "PaymentEntry", pid).await?;
    let against = rows(doc, "references").first().and_then(|r| id(r, "invoice"));
    let source = Some(("PaymentEntry".to_string(), pid));
    let op = Uuid::now_v7();
    if !b(doc, "is_refund") {
        let mut acc = lock(ctx, hc.tx, id(doc, "stored_value_account").unwrap_or_default()).await?;
        let kd = kind(k, ctx, hc.tx, acc.kind).await?;
        acc.usable(&kd, on)?;
        let amount = if kd.units { d(doc, "stored_value_units") } else { paid };
        if amount > acc.balance() {
            return Err(acc.short(&kd, amount));
        }
        if !kd.partial && amount != acc.balance() {
            return Err(Error::invalid(format!("{} is used whole: take {} from it", acc.label(&kd), acc.balance().normalize())));
        }
        if kd.units && amount > covered(ctx, hc.tx, k, &kd, against.unwrap_or_default()).await? {
            return Err(Error::invalid(format!("the invoice has no more of {}'s items to pay with units", kd.name)));
        }
        let released = acc.release(amount, co.precision);
        let breakage = if kd.on_expiry == OnExpiry::IncomeOverTime && amount < acc.balance() {
            round(released * kd.breakage / (Decimal::ONE - kd.breakage), co.precision).min(acc.value() - released).max(Decimal::ZERO)
        } else {
            Decimal::ZERO
        };
        // The payment debited the liability with the money; it owes `released` (and `breakage`) less.
        let difference = paid - released - breakage;
        if !difference.is_zero() {
            let mut e = Entry::new(co.id, on, "PaymentEntry", pid, s(doc, "number").map(str::to_string));
            e.memo = Some(format!("{} at what it was bought for", acc.label(&kd)));
            e.lines = vec![
                Line::new(kd.liability, -difference, &co.currency_code).remarks("Stored value at what it was bought for"),
                Line::new(kd.income()?, difference, &co.currency_code).remarks("Stored value at what it was bought for"),
            ];
            ledger::post(hc.tx, ctx, e, co.precision).await?;
        }
        let m = Move { entry_type: "redeem", amount: -amount, value: -released, on: Some(on), source: source.clone(), against, journal, ..Default::default() };
        record(k, ctx, hc.tx, &mut acc, &kd, op, m).await?;
        if breakage > Decimal::ZERO {
            let m = Move {
                entry_type: "breakage",
                value: -breakage,
                on: Some(on),
                source,
                against,
                journal,
                remarks: Some("Expected unused share".into()),
                ..Default::default()
            };
            record(k, ctx, hc.tx, &mut acc, &kd, op, m).await?;
        }
        return Ok(());
    }
    let (mut acc, kd) = match id(doc, "stored_value_account") {
        Some(a) => {
            let acc = lock(ctx, hc.tx, a).await?;
            let kd = kind(k, ctx, hc.tx, acc.kind).await?;
            acc.usable(&kd, on)?;
            (acc, kd)
        }
        None => {
            let kd = kind(k, ctx, hc.tx, id(doc, "stored_value_kind").unwrap_or_default()).await?;
            // Without a code typed, the one made here is shown to no one: the account's New Code gives one.
            (open(k, ctx, hc.tx, &kd, id(doc, "customer"), kept_secret(doc, "stored_value_secret"), on).await?.0, kd)
        }
    };
    let amount = if kd.units { d(doc, "stored_value_units") } else { paid };
    let m = Move { entry_type: "refund", amount, value: paid, on: Some(on), source, against, journal, ..Default::default() };
    record(k, ctx, hc.tx, &mut acc, &kd, op, m).await?;
    Ok(())
}

/// Where a refund of `invoice` goes back by default: the account that paid it, when its kind says
/// refunds go back onto it.
pub(crate) async fn refund_onto(k: &Kernel, ctx: &Ctx, tx: &mut Tx, invoice: Uuid) -> Result<Option<Uuid>> {
    let paid_by: Option<(Uuid, Uuid)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select e.account_id, a.kind from stored_value_entry e join {} a on a.tenant_id = e.tenant_id and a.id = e.account_id \
         where e.tenant_id = $1 and e.against_id = $2 and e.entry_type = 'redeem' \
         and not exists (select 1 from stored_value_entry r where r.tenant_id = e.tenant_id and r.reversal_of = e.id) order by e.created_at desc limit 1",
        table(ctx, ACCOUNT)?
    )))
    .bind(ctx.tenant)
    .bind(invoice)
    .fetch_optional(&mut **tx)
    .await?;
    match paid_by {
        Some((acc, kid)) if kind(k, ctx, tx, kid).await?.refund_back => Ok(Some(acc)),
        _ => Ok(None),
    }
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

pub(crate) fn uuid_param(params: &Value, name: &str) -> Result<Option<Uuid>> {
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
