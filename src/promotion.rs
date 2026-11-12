//! Promotions on sales documents (the `Promotion` and `PromotionCode` records): the server's side of
//! `ferrum-erp-promotions`, whose pure computation the till runs too. Here: which promotions could
//! apply (by company and date, enabled, not used up), which codes the customer gave are good, who
//! buys, where and when, and what the computation says, kept on the document; counting a code's use
//! once when a document is submitted and giving it back when it is cancelled; and pricing lines for
//! other apps without a document (the `price_lines` query on a company).
//!
//! Promotions run on a draft quotation, order or invoice of their own (an entity with
//! `applied_promotions`), never on a return, a debit note or lines made from an order or a delivery:
//! those keep each line's share as it was copied, so a credit note gives back what was taken off.

use crate::pricing::{Side, calc_input};
use crate::util::{Company, b, d, find, get, id, money, rows, s, set};
use chrono::Datelike;
use ferrum_erp_promotions::{self as engine, Code, GetAt, Gives, Line, Promotion, When};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, Filter, HookCtx, Kernel, Result, Tx};
use ferrum_meta::EffEntity;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use uuid::Uuid;

/// Codes as kept: trimmed, in capitals.
pub fn normalize_code(raw: &str) -> String {
    raw.trim().to_uppercase()
}

fn valid_code(code: &str) -> bool {
    (3..=64).contains(&code.len()) && code.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'-' || c == b'_')
}

/// The codes a document gives, as kept, each once.
fn given_codes(raw: Option<&str>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    raw.unwrap_or_default().split([',', ' ', ';', '\n']).map(normalize_code).filter(|c| !c.is_empty() && seen.insert(c.clone())).collect()
}

/// The answer for a code that cannot be used, the same whether it does not exist, is used up or
/// switched off, so trying codes tells nothing about which exist.
fn refused(code: &str) -> String {
    format!("{code} cannot be used")
}

/// Splits `codes` into those that open a promotion now (code, promotion) and those that do not.
pub async fn check_codes(k: &Kernel, ctx: &Ctx, tx: &mut Tx, codes: &[String]) -> Result<(Vec<Code>, Vec<String>)> {
    check_codes_holding(k, ctx, tx, codes, &BTreeSet::new()).await
}

/// [`check_codes`], where the codes in `held` are the document's own already (a submitted order
/// priced again): their use is counted, so being used up does not refuse them.
async fn check_codes_holding(k: &Kernel, ctx: &Ctx, tx: &mut Tx, codes: &[String], held: &BTreeSet<String>) -> Result<(Vec<Code>, Vec<String>)> {
    let (mut good, mut bad) = (Vec::new(), Vec::new());
    for code in codes {
        let found = if valid_code(code) { find(k, ctx, tx, "PromotionCode", vec![Filter::eq("code", code.as_str())], None, 1).await? } else { vec![] };
        let open = match found.first() {
            Some(c) if !b(c, "disabled") && (held.contains(code) || has_uses(c)) => match id(c, "promotion") {
                Some(p) => {
                    let promo = get(k, ctx, tx, "Promotion", p).await?;
                    (!b(&promo, "disabled") && (held.contains(code) || has_uses(&promo))).then_some(p)
                }
                None => None,
            },
            _ => None,
        };
        match open {
            Some(p) => good.push(Code { promotion: p.to_string(), code: code.clone() }),
            None => bad.push(code.clone()),
        }
    }
    Ok((good, bad))
}

fn int(doc: &Doc, k: &str) -> i64 {
    doc.get(k).and_then(Value::as_i64).unwrap_or(0)
}

fn has_uses(doc: &Doc) -> bool {
    int(doc, "max_uses") <= 0 || int(doc, "used_count") < int(doc, "max_uses")
}

fn set_of(rows: &[Doc]) -> engine::Set {
    engine::Set {
        items: rows.iter().filter_map(|r| id(r, "item")).map(|u| u.to_string()).collect(),
        item_groups: rows.iter().filter_map(|r| id(r, "item_group")).map(|u| u.to_string()).collect(),
    }
}

fn list(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default().split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

/// A promotion record (a `Promotion` with its lines, as `get` gives it) as the computation takes it, or `None` when
/// its `gives` is one this build does not know. Amounts of the record are in the company's currency and are taken
/// into the document's: `rate` is the document's `conversion_rate` (document to company), `Decimal::ONE` when the
/// sale is in the company's currency, as a till's is. This is the one conversion every app prices promotions with:
/// a stable signature, kept across releases.
pub fn to_engine(p: &Doc, rate: Decimal) -> Option<Promotion> {
    let doc_money = |v: Decimal| if rate == Decimal::ONE { v } else { (v / rate).round_dp(9) };
    let (percent, amount) = (d(p, "percent"), doc_money(d(p, "amount")));
    let gives = match s(p, "gives")? {
        "Percent Off the Document" => Gives::PercentOffDocument { percent },
        "Amount Off the Document" => Gives::AmountOffDocument { amount },
        "Percent Off Qualifying Lines" => Gives::PercentOffLines { percent },
        "Amount Off Qualifying Lines" => Gives::AmountOffLines { amount },
        "Buy and Get" => Gives::BuyGet {
            buy: d(p, "buy_qty"),
            get: d(p, "get_qty"),
            get_set: set_of(&rows(p, "get_items")),
            at: match s(p, "get_at") {
                Some("Percent Off") => GetAt::PercentOff { percent },
                Some("Price") => GetAt::Price { price: amount },
                _ => GetAt::Free,
            },
            cheapest_first: p.get("cheapest_first").and_then(Value::as_bool).unwrap_or(true),
        },
        "Bundle Price" => {
            Gives::Bundle { parts: rows(p, "bundle_items").iter().filter_map(|r| Some((id(r, "item")?.to_string(), d(r, "qty")))).collect(), price: amount }
        }
        "Free Delivery" => Gives::FreeDelivery,
        "Free Item" => Gives::FreeItem { item: id(p, "free_item")?.to_string(), qty: d(p, "free_qty").max(Decimal::ONE) },
        _ => return None,
    };
    let parties = rows(p, "customers");
    let places = rows(p, "places");
    Some(Promotion {
        id: s(p, "id")?.to_string(),
        title: s(p, "title").unwrap_or_default().to_string(),
        priority: int(p, "priority"),
        valid_from: s(p, "valid_from").map(str::to_string),
        valid_upto: s(p, "valid_upto").map(str::to_string),
        weekdays: list(s(p, "weekdays")).iter().filter_map(|w| w.parse().ok()).collect(),
        from_time: s(p, "from_time").map(str::to_string),
        to_time: s(p, "to_time").map(str::to_string),
        customers: parties.iter().filter_map(|r| id(r, "customer")).map(|u| u.to_string()).collect(),
        customer_groups: parties.iter().filter_map(|r| id(r, "customer_group")).map(|u| u.to_string()).collect(),
        channels: list(s(p, "channels")),
        places: places.iter().filter_map(|r| s(r, "warehouse").or(s(r, "place"))).map(str::to_string).collect(),
        requires_code: b(p, "requires_code"),
        first_order_only: b(p, "first_order_only"),
        min_amount: doc_money(d(p, "min_amount")),
        min_qty: d(p, "min_qty"),
        qualifying: set_of(&rows(p, "qualifying_items")),
        gives,
        combinable: p.get("combinable").and_then(Value::as_bool).unwrap_or(true),
        with_line_discounts: p.get("with_line_discounts").and_then(Value::as_bool).unwrap_or(true),
        max_discount: p.get("max_discount").filter(|v| !v.is_null()).map(|_| doc_money(d(p, "max_discount"))),
    })
}

/// A document line as the computation takes it: `row` is the line, `item` its item record (for the item group and
/// whether it is a delivery charge) and `amount` what the line comes to before promotions, in the document's
/// currency (what `ferrum_calc` gives for it with no promotion). The line's `id` is its key.
pub fn engine_line(row: &Doc, item: Option<&Doc>, amount: Decimal) -> Line {
    Line {
        key: s(row, "id").unwrap_or_default().to_string(),
        item: id(row, "item").map(|i| i.to_string()),
        item_group: item.and_then(|i| id(i, "item_group")).map(|g| g.to_string()),
        qty: d(row, "qty"),
        amount,
        line_discount: d(row, "discount_percent") > Decimal::ZERO,
        delivery: item.is_some_and(|i| b(i, "is_delivery_charge")),
        added_by: s(row, "added_by_promotion").map(str::to_string),
    }
}

/// The ISO weekday (1 Monday to 7) of a `YYYY-MM-DD` date, for [`engine::When::weekday`].
pub fn iso_weekday(date: &str) -> Option<u8> {
    chrono::NaiveDate::parse_from_str(date.get(..10)?, "%Y-%m-%d").ok().map(|d| d.weekday().number_from_monday() as u8)
}

/// The promotions that could apply to a company's sale on `on` (`YYYY-MM-DD`), whole: enabled,
/// for the company or every company, in their dates, not used up. What the till takes when it opens.
pub async fn candidates(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, on: &str) -> Result<Vec<Doc>> {
    candidates_holding(k, ctx, tx, company, on, &BTreeSet::new()).await
}

/// [`candidates`], keeping the promotions in `held` (a submitted order's own, already counted) even
/// when they are used up since.
async fn candidates_holding(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, on: &str, held: &BTreeSet<Uuid>) -> Result<Vec<Doc>> {
    if ctx.schema.entity("Promotion").is_none() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for p in find(k, ctx, tx, "Promotion", vec![Filter::eq("disabled", false)], Some("priority desc"), 500).await? {
        let dated = s(&p, "valid_from").is_none_or(|f| f <= on) && s(&p, "valid_upto").is_none_or(|u| u >= on);
        if dated && id(&p, "company").is_none_or(|c| c == company) && (has_uses(&p) || id(&p, "id").is_some_and(|i| held.contains(&i))) {
            out.push(get(k, ctx, tx, "Promotion", id(&p, "id").unwrap_or_default()).await?);
        }
    }
    Ok(out)
}

/// Set (in memory only) on a submitted order whose lines Update Items changes, so its promotions
/// are worked out again; never stored.
pub const REPRICE: &str = "__reprice_promotions";

/// Whether the computation runs on this document: a draft sale of its own, or a submitted order
/// whose lines are being changed ([`REPRICE`]).
fn evaluates(ent: &EffEntity, doc: &Doc, side: Side) -> bool {
    let derived = rows(doc, "items").iter().any(|r| ["so_line", "dn_line", "return_line"].iter().any(|f| s(r, f).is_some()));
    side == Side::Selling
        && ent.field("applied_promotions").is_some()
        && (d(doc, "docstatus").is_zero() || b(doc, REPRICE))
        && !b(doc, "is_return")
        && !b(doc, "is_debit_note")
        && !derived
}

/// The company's time zone (the workspace's when it names none) and the moment the document is
/// for: its date, and its time when it has one, else now.
fn moment(ctx: &Ctx, co: &Company, doc: &Doc) -> Result<(String, u8, String)> {
    let day = crate::util::date(doc, "posting_date")?;
    let now = match s(&co.doc, "time_zone") {
        Some(tz) => ferrum_kernel::ctx::local_now(tz),
        None => ctx.now_local(),
    };
    let time = s(doc, "posting_time").map(str::to_string).unwrap_or_else(|| now.format("%H:%M").to_string());
    Ok((day.format("%Y-%m-%d").to_string(), day.weekday().number_from_monday() as u8, time))
}

async fn first_order(ctx: &Ctx, tx: &mut Tx, customer: Uuid, doc: Option<Uuid>) -> Result<bool> {
    let mut seen = false;
    for ent in ["SalesOrder", "SalesInvoice"] {
        let Some(e) = ctx.schema.entity(ent) else { continue };
        let returns = if e.field("is_return").is_some() { " and not coalesce(is_return, false)" } else { "" };
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select count(*) from {} where tenant_id = $1 and customer = $2 and docstatus = 1 and id is distinct from $3{returns}",
            ferrum_kernel::ddl::q(&e.table)
        )))
        .bind(ctx.tenant)
        .bind(customer)
        .bind(doc)
        .fetch_one(&mut **tx)
        .await?;
        seen |= n > 0;
    }
    Ok(!seen)
}

/// What each line comes to before promotions ([`ferrum_erp_pricing::before_promotions`]).
fn before_promotions(items: &[Doc], co: &Company, precision: u32, rate: Decimal) -> Result<Vec<Decimal>> {
    ferrum_erp_pricing::before_promotions(&calc_input(items, &[], Decimal::ZERO, co, precision, rate, false))
        .map_err(|e| Error::Validation(vec![ferrum_kernel::FieldError::new("items", e)]))
}

/// Runs the document's promotions over its priced `items` (before taxes): each line's share
/// (`promotion_percent`), what each promotion took off each line (`applied_promotions`), and the
/// free items they add or no longer add. Returns whether lines were added or removed, so the
/// document is priced again. Does nothing on a document [`evaluates`] rejects.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut Tx,
    ent: &EffEntity,
    doc: &mut Doc,
    items: &mut Vec<Doc>,
    item_docs: &HashMap<Uuid, Doc>,
    co: &Company,
    precision: u32,
    rate: Decimal,
) -> Result<bool> {
    let given = given_of(ent, doc)?;
    if !evaluates(ent, doc, Side::Selling) {
        if given.is_some() && d(doc, "docstatus").is_zero() {
            return Err(Error::field(
                "given_promotions",
                "promotions already applied can be passed only on a sale of its own, not on a return, a debit note or lines made from another document",
            ));
        }
        return Ok(false);
    }
    for row in items.iter_mut() {
        // An app that priced the sale itself may give each line's share as it priced it (the line's rate then
        // carries the promotions as ERP's own would); otherwise the share is worked out below.
        if given.is_none() {
            set(row, "promotion_percent", Value::Null);
        }
        if s(row, "id").is_none() {
            set(row, "id", Uuid::now_v7().to_string());
        }
    }
    let (date, weekday, time) = match given.as_ref().and_then(|g| g.at.as_deref()) {
        Some(at) => {
            let (day, time) = at.split_once([' ', 'T']).unwrap_or((at, ""));
            let weekday = iso_weekday(day).ok_or_else(|| Error::field("given_promotions", "at is a moment as YYYY-MM-DD HH:MM"))?;
            (day.to_string(), weekday, time.get(..5).unwrap_or(time).to_string())
        }
        None => moment(ctx, co, doc)?,
    };
    if let Some(given) = given {
        return record_given(k, ctx, tx, ent, doc, items, item_docs, co, precision, rate, (date, weekday, time), given).await;
    }
    // A submitted order priced again holds the codes and promotions it counted.
    let (held_codes, held_promotions) = if d(doc, "docstatus").is_zero() { Default::default() } else { held(doc) };
    let found = candidates_holding(k, ctx, tx, co.id, &date, &held_promotions).await?;
    let (codes, bad) = check_codes_holding(k, ctx, tx, &given_codes(s(doc, "promotion_codes")), &held_codes).await?;
    if !bad.is_empty() {
        return Err(Error::field("promotion_codes", bad.iter().map(|c| refused(c)).collect::<Vec<_>>().join("; ")));
    }
    let customer = id(doc, "customer");
    let party = match customer {
        Some(c) => Some(get(k, ctx, tx, "Customer", c).await?),
        None => None,
    };
    let first = match customer {
        Some(c) if found.iter().any(|p| b(p, "first_order_only")) => first_order(ctx, tx, c, id(doc, "id")).await?,
        _ => false,
    };
    let when = When {
        date,
        weekday,
        time,
        customer: customer.map(|c| c.to_string()),
        customer_group: party.as_ref().and_then(|p| id(p, "customer_group")).map(|g| g.to_string()),
        channel: Some(s(doc, "channel").unwrap_or(&ent.def.label).to_string()),
        place: s(doc, "set_warehouse").map(str::to_string),
        first_order: first,
        codes,
    };
    // The lines' amounts before promotions, as the engine works them out.
    let before = before_promotions(items, co, precision, rate)?;
    let lines: Vec<Line> = items.iter().zip(&before).map(|(r, amount)| engine_line(r, id(r, "item").and_then(|i| item_docs.get(&i)), *amount)).collect();
    let promotions: Vec<Promotion> = found.iter().filter_map(|p| to_engine(p, rate)).collect();
    let input = engine::Input { lines, promotions, when, precision, rounding: co.rounding };
    let out = engine::apply(&input).map_err(|e| Error::field("items", e))?;
    for (row, line) in items.iter_mut().zip(&out.lines) {
        set(row, "promotion_percent", if line.percent.is_zero() { Value::Null } else { json!(line.percent.normalize().to_string()) });
    }
    let applied: Vec<Value> = out
        .applied
        .iter()
        .map(|a| json!({"id": Uuid::now_v7().to_string(), "promotion": a.promotion, "title": a.title, "line": a.line, "amount": money(a.amount), "code": a.code}))
        .collect();
    set(doc, "applied_promotions", Value::Array(applied));
    // Free items: those of promotions that no longer apply go, those of new ones come.
    let count = items.len();
    items.retain(|r| s(r, "added_by_promotion").is_none_or(|p| out.fired.iter().any(|f| f == p)));
    let removed = items.len() != count;
    for add in &out.add {
        let name = match add.item.parse() {
            Ok(i) => get(k, ctx, tx, "Item", i).await?.get("item_name").cloned().unwrap_or(Value::Null),
            Err(_) => Value::Null,
        };
        // Placed last (the kernel orders lines by `idx`), named as its item is.
        let row = json!({"id": Uuid::now_v7().to_string(), "idx": items.len(), "item": add.item, "item_name": name, "qty": money(add.qty), "added_by_promotion": add.promotion});
        items.push(row.as_object().cloned().unwrap_or_default());
    }
    Ok(removed || !out.add.is_empty())
}

/// Promotions an app already applied to a sale it passes to ERP (a till's, priced from its own snapshot): the
/// document's `given_promotions` (a JSON object, kept as given),
///
/// ```json
/// {"snapshot": "<version the till priced with>", "source": "pos:<the app's own id for the sale>", "counted": true,
///  "at": "2026-11-12 10:31", "applied": [{"promotion": "<id>", "line": 0, "amount": "12.50", "code": null}]}
/// ```
///
/// `applied`: each promotion's amount off each line (`line`: the line's place in `items` from 0, or its id), as the
/// lines are priced *after* it (their rates already carry the discount). `source` (optional) names the sale: the same
/// source is recorded on one submitted document only. `counted: true` says the app already counted the promotions'
/// and codes' uses, so ERP records them for the report but counts none. `at` (optional) is the sale's moment in the
/// company's time zone, else the document's date and time. The set the document is priced with is not looked for
/// again: what is given is checked ([`record_given`]) and kept in `applied_promotions`.
struct Given {
    source: Option<String>,
    counted: bool,
    at: Option<String>,
    applied: Vec<(Uuid, Value, Decimal, Option<String>)>,
}

fn given_of(ent: &EffEntity, doc: &Doc) -> Result<Option<Given>> {
    let Some(v) = doc.get("given_promotions").filter(|v| !v.is_null()).filter(|_| ent.field("given_promotions").is_some()) else { return Ok(None) };
    let bad = |m: &str| Error::field("given_promotions", m.to_string());
    let o = v.as_object().ok_or_else(|| bad("is a JSON object: {snapshot, source, counted, at, applied: [{promotion, line, amount, code}]}"))?;
    let text = |k: &str| o.get(k).and_then(Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string);
    let mut applied = Vec::new();
    for a in o.get("applied").and_then(Value::as_array).ok_or_else(|| bad("applied is a list of {promotion, line, amount}"))? {
        let promotion = a.get("promotion").and_then(Value::as_str).and_then(|p| p.parse().ok()).ok_or_else(|| bad("a promotion is its id"))?;
        let amount = match a.get("amount") {
            Some(Value::String(x)) => x.parse().ok(),
            Some(Value::Number(x)) => x.to_string().parse().ok(),
            _ => None,
        }
        .filter(|x: &Decimal| *x > Decimal::ZERO)
        .ok_or_else(|| bad("an amount is a number above zero"))?;
        let code = a.get("code").and_then(Value::as_str).map(normalize_code).filter(|c| !c.is_empty());
        applied.push((promotion, a.get("line").cloned().unwrap_or(Value::Null), amount, code));
    }
    Ok(Some(Given { source: text("source"), counted: o.get("counted").and_then(Value::as_bool).unwrap_or(false), at: text("at"), applied }))
}

/// Checks what an app says it applied against the promotion records and records it. Each promotion must exist, be the
/// company's (or every company's), have been in force at the sale's moment (dates, weekdays, hours, customer,
/// channel, place and, when it needs one, a good code: [`engine::admits`]; a promotion switched off since still counts,
/// the sale was made while it ran), and no amount may be more than the computation gives for that promotion on that
/// line, working over the lines as they were before the promotion (their amounts now, plus what was given), with only
/// the promotions given, plus one smallest unit of the document's currency for the rounding the computation does. What is
/// given becomes the document's `applied_promotions` (so the report counts them and a credit note gives back its share);
/// the lines are not discounted again.
#[allow(clippy::too_many_arguments)]
async fn record_given(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut Tx,
    ent: &EffEntity,
    doc: &mut Doc,
    items: &mut [Doc],
    item_docs: &HashMap<Uuid, Doc>,
    co: &Company,
    precision: u32,
    rate: Decimal,
    (date, weekday, time): (String, u8, String),
    given: Given,
) -> Result<bool> {
    let bad = |m: String| Error::field("given_promotions", m);
    if !d(doc, "docstatus").is_zero() {
        return Err(bad("a sale with promotions given by an app is not priced again".into()));
    }
    let mut at_line = Vec::new();
    for (_, line, _, _) in &given.applied {
        let place = match line {
            Value::Number(n) => n.as_u64().map(|n| n as usize).filter(|n| *n < items.len()),
            Value::String(x) => items.iter().position(|r| s(r, "id") == Some(x.as_str())),
            _ => None,
        };
        at_line.push(place.ok_or_else(|| bad(format!("line {line} is not a line of this document")))?);
    }
    let before = before_promotions(items, co, precision, rate)?;
    let mut again = vec![Decimal::ZERO; items.len()];
    for ((.., amount, _), at) in given.applied.iter().zip(&at_line) {
        again[*at] += amount;
    }
    let lines: Vec<Line> =
        items.iter().zip(&before).zip(&again).map(|((r, amount), more)| engine_line(r, id(r, "item").and_then(|i| item_docs.get(&i)), amount + more)).collect();
    let mut promotions: Vec<Promotion> = Vec::new();
    let mut titles: HashMap<Uuid, String> = HashMap::new();
    for (p, ..) in &given.applied {
        if titles.contains_key(p) {
            continue;
        }
        let rec = get(k, ctx, tx, "Promotion", *p).await.map_err(|_| bad(format!("promotion {p} does not exist here")))?;
        if id(&rec, "company").is_some_and(|c| c != co.id) {
            return Err(bad(format!("{} is another company's promotion", s(&rec, "title").unwrap_or_default())));
        }
        titles.insert(*p, s(&rec, "title").unwrap_or_default().to_string());
        promotions
            .push(to_engine(&rec, rate).ok_or_else(|| bad(format!("{} gives something this version does not know", s(&rec, "title").unwrap_or_default())))?);
    }
    let customer = id(doc, "customer");
    let party = match customer {
        Some(c) => Some(get(k, ctx, tx, "Customer", c).await?),
        None => None,
    };
    let wanted: Vec<String> = given.applied.iter().filter_map(|a| a.3.clone()).collect::<BTreeSet<_>>().into_iter().collect();
    let all: BTreeSet<String> = if given.counted { wanted.iter().cloned().collect() } else { BTreeSet::new() };
    let (codes, refused_codes) = check_codes_holding(k, ctx, tx, &wanted, &all).await?;
    if !refused_codes.is_empty() {
        return Err(bad(refused_codes.iter().map(|c| refused(c)).collect::<Vec<_>>().join("; ")));
    }
    let first = match customer {
        Some(c) if promotions.iter().any(|p| p.first_order_only) => first_order(ctx, tx, c, id(doc, "id")).await?,
        _ => false,
    };
    let when = When {
        date,
        weekday,
        time,
        customer: customer.map(|c| c.to_string()),
        customer_group: party.as_ref().and_then(|p| id(p, "customer_group")).map(|g| g.to_string()),
        channel: Some(s(doc, "channel").unwrap_or(&ent.def.label).to_string()),
        place: s(doc, "set_warehouse").map(str::to_string),
        first_order: first,
        codes,
    };
    for p in &promotions {
        if !engine::admits(p, &when) {
            return Err(bad(format!(
                "{} was not in force at {} {} for this sale (its dates, hours, customers, channel, place or code)",
                p.title, when.date, when.time
            )));
        }
    }
    let input = engine::Input { lines, promotions, when, precision, rounding: co.rounding };
    let out = engine::apply(&input).map_err(|e| Error::field("items", e))?;
    // The engine's own rounding spreads a whole by largest remainder: a part may differ from an app's by a unit.
    let unit = Decimal::new(1, precision);
    let mut asked: HashMap<(Uuid, usize), Decimal> = HashMap::new();
    for ((p, _, amount, _), at) in given.applied.iter().zip(&at_line) {
        *asked.entry((*p, *at)).or_default() += amount;
    }
    for ((p, at), amount) in &asked {
        let key = s(&items[*at], "id").unwrap_or_default();
        let most: Decimal = out.applied.iter().filter(|a| a.promotion == p.to_string() && a.line == key).map(|a| a.amount).sum();
        if *amount > most + unit {
            return Err(bad(format!(
                "{} took {} off line {}; the most it can there is {}",
                titles.get(p).map(String::as_str).unwrap_or_default(),
                amount.normalize(),
                at + 1,
                most.normalize()
            )));
        }
    }
    let applied: Vec<Value> = given
        .applied
        .iter()
        .zip(&at_line)
        .map(|((p, _, amount, code), at)| {
            json!({"id": Uuid::now_v7().to_string(), "promotion": p.to_string(), "title": titles.get(p), "line": s(&items[*at], "id"), "amount": money(*amount),
                   "code": code, "source": given.source, "counted_elsewhere": given.counted})
        })
        .collect();
    set(doc, "applied_promotions", Value::Array(applied));
    Ok(false)
}

/// A sale's promotions an app passed are recorded on one submitted document: holding a lock on the source for the
/// rest of the transaction, so two documents made from the same sale at once take turns, and the second, seeing the
/// first's, is refused. Called on submission, before the uses are counted.
async fn claim_sources(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
    let own = id(doc, "id");
    for source in rows(doc, "applied_promotions").iter().filter_map(|a| s(a, "source")).collect::<BTreeSet<_>>() {
        sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("promotions:{}:{source}", hc.ctx.tenant))
            .execute(&mut **hc.tx)
            .await?;
        for entity in ["SalesOrder", "SalesInvoice"] {
            let Some(e) = hc.ctx.schema.entity(entity) else { continue };
            let Some(lines) = e.children.get("applied_promotions") else { continue };
            let other: Option<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "select i.id from {docs} i join {lines} l on l.tenant_id = i.tenant_id and l.parent_id = i.id \
                 where i.tenant_id = $1 and i.docstatus = 1 and l.source = $2 and i.id is distinct from $3 limit 1",
                docs = ferrum_kernel::ddl::q(&e.table),
                lines = ferrum_kernel::ddl::q(&lines.table),
            )))
            .bind(hc.ctx.tenant)
            .bind(source)
            .bind(own)
            .fetch_optional(&mut **hc.tx)
            .await?;
            if other.is_some() {
                return Err(Error::field("given_promotions", format!("the promotions of {source} are already recorded on another submitted document")));
            }
        }
    }
    Ok(())
}

/// The codes and the promotions a document's `applied_promotions` count.
fn held(doc: &Doc) -> (BTreeSet<String>, BTreeSet<Uuid>) {
    held_of(rows(doc, "applied_promotions"))
}

/// What ERP counts the use of: the promotions an app passed and counted itself are left out.
fn counted(doc: &Doc) -> (BTreeSet<String>, BTreeSet<Uuid>) {
    held_of(rows(doc, "applied_promotions").into_iter().filter(|a| !b(a, "counted_elsewhere")).collect())
}

fn held_of(applied: Vec<Doc>) -> (BTreeSet<String>, BTreeSet<Uuid>) {
    (applied.iter().filter_map(|a| s(a, "code")).map(str::to_string).collect(), applied.iter().filter_map(|a| id(a, "promotion")).collect())
}

/// Counts each code a submitted document used, and each promotion it applied, once (`step` 1), or
/// gives them back when it is cancelled (`step` -1). Counting refuses a code or promotion that is
/// used up meanwhile: of two documents taking the last use at once, the first to commit has it.
/// A return counts nothing: its promotions are what it gives back of its original's.
pub async fn count_uses(hc: &mut HookCtx<'_>, doc: &Doc, step: i64) -> Result<()> {
    if b(doc, "is_return") {
        return Ok(());
    }
    if step > 0 {
        claim_sources(hc, doc).await?;
    }
    let (codes, promotions) = counted(doc);
    count(hc, &codes, &promotions, step).await
}

/// After a submitted order is priced again (Update Items): a code or promotion it now applies and
/// did not is counted, one it no longer applies is given back, and those it keeps are left alone.
pub async fn recount(hc: &mut HookCtx<'_>, was: &Doc, now: &Doc) -> Result<()> {
    let ((c0, p0), (c1, p1)) = (counted(was), counted(now));
    count(hc, &c0.difference(&c1).cloned().collect(), &p0.difference(&p1).copied().collect(), -1).await?;
    count(hc, &c1.difference(&c0).cloned().collect(), &p1.difference(&p0).copied().collect(), 1).await
}

async fn count(hc: &mut HookCtx<'_>, used: &BTreeSet<String>, promotions: &BTreeSet<Uuid>, step: i64) -> Result<()> {
    if used.is_empty() && promotions.is_empty() {
        return Ok(());
    }
    let table = |e: &str| hc.ctx.schema.entity(e).map(|x| ferrum_kernel::ddl::q(&x.table));
    let (Some(codes), Some(promos)) = (table("PromotionCode"), table("Promotion")) else { return Ok(()) };
    for (table, key, column) in used.iter().map(|c| (&codes, Value::from(c.as_str()), "code")).chain(promotions.iter().map(|p| (&promos, json!(p), "id"))) {
        let sql = if step > 0 {
            format!(
                "update {table} set used_count = coalesce(used_count, 0) + 1 where tenant_id = $1 and {column}::text = $2 \
                 and (coalesce(max_uses, 0) = 0 or coalesce(used_count, 0) < max_uses)"
            )
        } else {
            format!("update {table} set used_count = greatest(coalesce(used_count, 0) - 1, 0) where tenant_id = $1 and {column}::text = $2")
        };
        let text = key.as_str().unwrap_or_default().to_string();
        let n = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(hc.ctx.tenant).bind(&text).execute(&mut **hc.tx).await?.rows_affected();
        if n == 0 && step > 0 {
            let what = if column == "code" { refused(&text) } else { "a promotion on it is used up: save it again to price it without".to_string() };
            return Err(Error::field("promotion_codes", what));
        }
    }
    Ok(())
}

/// What a credit note gives back of the promotions on the lines it returns, on its submission
/// (`sign` 1) or cancellation (-1). `taken`: each original line and the quantity returned of it.
/// The promotions are the original invoice's on the line, else its order's; each part returned
/// takes the promotion's amount in proportion, rounded, and the part that returns the rest of the
/// line takes exactly what is left, so the parts sum to the line's amount to the cent. What was
/// given back is kept on that line (`promotion_returned`), and the credit note's
/// `applied_promotions` carry it, negative, for the promotions report to net.
pub async fn give_back(hc: &mut HookCtx<'_>, doc: &Doc, original: &Doc, taken: &[(Uuid, Decimal)], sign: Decimal, precision: u32) -> Result<()> {
    let sctx = crate::util::sys(hc.ctx);
    let rid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let lines = rows(doc, "items");
    let mine = rows(doc, "applied_promotions");
    let mut out: Vec<Doc> = Vec::new();
    for (line, q) in taken {
        let Some(o) = rows(original, "items").into_iter().find(|o| id(o, "id") == Some(*line)) else { continue };
        // Where the line's promotions are kept: the invoice's own, else its order's.
        let (entity, parent, src) = if rows(original, "applied_promotions").iter().any(|a| id(a, "line") == Some(*line)) {
            ("SalesInvoice", original.clone(), o)
        } else if let (Some(so), Some(sl)) = (id(&o, "sales_order"), id(&o, "so_line")) {
            let order = hc.kernel.get_tx(&sctx, hc.tx, "SalesOrder", so, true).await?;
            let Some(src) = rows(&order, "items").into_iter().find(|r| id(r, "id") == Some(sl)) else { continue };
            ("SalesOrder", order, src)
        } else {
            continue;
        };
        let sid = id(&src, "id").unwrap_or_default();
        let applied: Vec<Doc> = rows(&parent, "applied_promotions").into_iter().filter(|a| id(a, "line") == Some(sid)).collect();
        if applied.is_empty() {
            continue;
        }
        let mut back: Doc = src.get("promotion_returned").and_then(Value::as_object).cloned().unwrap_or_default();
        let mut amounts: Doc = back.get("amounts").and_then(Value::as_object).cloned().unwrap_or_default();
        let (had, done) = (d(&src, "qty"), d(&back, "qty"));
        // The credit note's lines for this original line.
        let here: Vec<String> = lines.iter().filter(|r| id(r, "return_line") == Some(*line)).filter_map(|r| s(r, "id").map(str::to_string)).collect();
        let mut seen = BTreeSet::new();
        for a in &applied {
            let Some(p) = id(a, "promotion") else { continue };
            if !seen.insert(p) {
                continue;
            }
            let key = p.to_string();
            let total: Decimal = applied.iter().filter(|x| id(x, "promotion") == Some(p)).map(|x| d(x, "amount")).sum();
            let given = d(&amounts, &key);
            let share = if sign > Decimal::ZERO {
                let share = if done + q >= had - crate::flow::QTY_TOLERANCE { total - given } else { (total * q / had).round_dp(precision).min(total - given) };
                let row = json!({"id": Uuid::now_v7().to_string(), "promotion": key, "title": a.get("title"), "line": here.first(), "amount": money(-share), "code": a.get("code")});
                out.push(row.as_object().cloned().unwrap_or_default());
                share
            } else {
                // Cancelled: what this credit note gave back of it comes back.
                -mine
                    .iter()
                    .filter(|x| id(x, "promotion") == Some(p) && s(x, "line").is_some_and(|l| here.iter().any(|h| h == l)))
                    .map(|x| -d(x, "amount"))
                    .sum::<Decimal>()
            };
            amounts.insert(key, money(given + share));
        }
        back.insert("qty".into(), money((done + sign * q).max(Decimal::ZERO)));
        back.insert("amounts".into(), Value::Object(amounts));
        let mut v = Doc::new();
        v.insert("promotion_returned".into(), Value::Object(back));
        hc.kernel.set_child_values_tx(&sctx, hc.tx, entity, "items", sid, v).await?;
    }
    if sign > Decimal::ZERO && !out.is_empty() {
        hc.kernel.set_child_rows_tx(&sctx, hc.tx, "SalesInvoice", "applied_promotions", rid, out).await?;
    }
    Ok(())
}

/// Keeps a promotion code in capitals and in the characters a code may have.
pub struct CodeHook;

#[async_trait::async_trait]
impl DocHook for CodeHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if let Some(code) = s(doc, "code").map(normalize_code) {
            set(doc, "code", code);
        }
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !s(doc, "code").is_some_and(valid_code) {
            return Err(Error::field("code", "use 3 to 64 letters, digits, '-' or '_'"));
        }
        Ok(())
    }
}

/// Lines priced for a customer on a channel at a moment, with codes, as a draft sales order of the
/// company would be (price lists, pricing rules, promotions, taxes), without saving anything or
/// counting a code. `req`: `lines` (`[{item, qty, uom?, price_list_rate?, discount_percent?}]`),
/// `customer`, and optionally `channel`, `place`, `at` (`YYYY-MM-DD HH:MM` in the company's time
/// zone), `codes`, `currency`, `price_list`. Codes that cannot be used come back in `rejected_codes`
/// and price as if not given.
pub async fn price_lines(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, req: &Value) -> Result<Value> {
    let ent = ctx.schema.entity("SalesOrder").cloned().ok_or_else(|| Error::invalid("selling is not installed here"))?;
    let lines = match req.get("lines") {
        Some(Value::String(text)) => serde_json::from_str::<Value>(text).map_err(|_| Error::field("lines", "is not a JSON list of lines"))?,
        Some(v) => v.clone(),
        None => Value::Null,
    };
    let Value::Array(lines) = lines else { return Err(Error::field("lines", "is required: a list of lines")) };
    let text = |k: &str| req.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
    let co = crate::util::company(k, ctx, tx, company).await?;
    let now = match s(&co.doc, "time_zone") {
        Some(tz) => ferrum_kernel::ctx::local_now(tz),
        None => ctx.now_local(),
    };
    let at = text("at").map(str::to_string).unwrap_or_else(|| now.format("%Y-%m-%d %H:%M").to_string());
    let (date, time) = at.split_once([' ', 'T']).unwrap_or((at.as_str(), ""));
    let (codes, rejected) = check_codes(k, ctx, tx, &given_codes(text("codes"))).await?;
    let mut doc = Doc::new();
    for (f, v) in [
        ("company", json!(company.to_string())),
        ("customer", json!(text("customer").ok_or_else(|| Error::field("customer", "is required"))?)),
        ("posting_date", json!(date)),
        ("posting_time", if time.is_empty() { Value::Null } else { json!(time) }),
        ("channel", json!(text("channel"))),
        ("set_warehouse", json!(text("place"))),
        ("currency", json!(text("currency"))),
        ("price_list", json!(text("price_list"))),
        ("promotion_codes", json!(codes.iter().map(|c| c.code.as_str()).collect::<Vec<_>>().join(","))),
        ("docstatus", json!(0)),
        ("items", Value::Array(lines.into_iter().filter(Value::is_object).collect())),
    ] {
        doc.insert(f.into(), v);
    }
    crate::pricing::calculate(k, ctx, tx, &ent, &mut doc, Side::Selling).await?;
    let items = rows(&doc, "items");
    let index: HashMap<&str, usize> = items.iter().enumerate().filter_map(|(i, r)| Some((s(r, "id")?, i))).collect();
    let promotions: Vec<Value> = rows(&doc, "applied_promotions")
        .iter()
        .map(|a| json!({"promotion": a.get("promotion"), "title": a.get("title"), "line": s(a, "line").and_then(|l| index.get(l)), "amount": a.get("amount"), "code": a.get("code")}))
        .collect();
    let keep = [
        "item",
        "item_name",
        "qty",
        "uom",
        "price_list_rate",
        "discount_percent",
        "promotion_percent",
        "added_by_promotion",
        "rate",
        "amount",
        "net_amount",
        "item_tax_rates",
    ];
    let lines: Vec<Value> = items.iter().map(|r| Value::Object(keep.iter().filter_map(|f| Some((f.to_string(), r.get(*f)?.clone()))).collect())).collect();
    let mut out = json!({"lines": lines, "promotions": promotions, "rejected_codes": rejected, "currency": doc.get("currency")});
    for f in ["total", "net_total", "total_taxes", "grand_total", "rounded_total"] {
        out[f] = doc.get(f).cloned().unwrap_or(Value::Null);
    }
    Ok(out)
}

/// The company's `price_lines` query (a contracted capability, read only).
pub struct PriceLinesHook;

#[async_trait::async_trait]
impl DocHook for PriceLinesHook {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "price_lines" {
            return Ok(None);
        }
        let company = id(doc, "id").ok_or_else(|| Error::internal("company without id"))?;
        price_lines(hc.kernel, hc.ctx, hc.tx, company, params).await.map(Some)
    }
}
