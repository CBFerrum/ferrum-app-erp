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
    let (mut good, mut bad) = (Vec::new(), Vec::new());
    for code in codes {
        let found = if valid_code(code) { find(k, ctx, tx, "PromotionCode", vec![Filter::eq("code", code.as_str())], None, 1).await? } else { vec![] };
        let open = match found.first() {
            Some(c) if !b(c, "disabled") && has_uses(c) => match id(c, "promotion") {
                Some(p) => {
                    let promo = get(k, ctx, tx, "Promotion", p).await?;
                    (!b(&promo, "disabled") && has_uses(&promo)).then_some(p)
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

/// A promotion record as the computation takes it; amounts from the company's currency into the
/// document's (`rate`: document to company).
fn to_engine(p: &Doc, rate: Decimal) -> Option<Promotion> {
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

/// The promotions that could apply to a company's sale on `on` (`YYYY-MM-DD`), whole: enabled,
/// for the company or every company, in their dates, not used up. What the till takes when it opens.
pub async fn candidates(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, on: &str) -> Result<Vec<Doc>> {
    if ctx.schema.entity("Promotion").is_none() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for p in find(k, ctx, tx, "Promotion", vec![Filter::eq("disabled", false)], Some("priority desc"), 500).await? {
        let dated = s(&p, "valid_from").is_none_or(|f| f <= on) && s(&p, "valid_upto").is_none_or(|u| u >= on);
        if dated && id(&p, "company").is_none_or(|c| c == company) && has_uses(&p) {
            out.push(get(k, ctx, tx, "Promotion", id(&p, "id").unwrap_or_default()).await?);
        }
    }
    Ok(out)
}

/// Whether the computation runs on this document: a draft sale of its own.
fn evaluates(ent: &EffEntity, doc: &Doc, side: Side) -> bool {
    let derived = rows(doc, "items").iter().any(|r| ["so_line", "dn_line", "return_line"].iter().any(|f| s(r, f).is_some()));
    side == Side::Selling
        && ent.field("applied_promotions").is_some()
        && d(doc, "docstatus").is_zero()
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
    if !evaluates(ent, doc, Side::Selling) {
        return Ok(false);
    }
    for row in items.iter_mut() {
        set(row, "promotion_percent", Value::Null);
        if s(row, "id").is_none() {
            set(row, "id", Uuid::now_v7().to_string());
        }
    }
    let (date, weekday, time) = moment(ctx, co, doc)?;
    let found = candidates(k, ctx, tx, co.id, &date).await?;
    let (codes, bad) = check_codes(k, ctx, tx, &given_codes(s(doc, "promotion_codes"))).await?;
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
    let before = ferrum_calc::compute(&calc_input(items, &[], Decimal::ZERO, co, precision, rate, false))
        .map_err(|e| Error::Validation(vec![ferrum_kernel::FieldError::new("items", e.to_string())]))?;
    let lines: Vec<Line> = items
        .iter()
        .zip(&before.lines)
        .map(|(r, out)| {
            let item = id(r, "item").and_then(|i| item_docs.get(&i));
            Line {
                key: s(r, "id").unwrap_or_default().to_string(),
                item: id(r, "item").map(|i| i.to_string()),
                item_group: item.and_then(|i| id(i, "item_group")).map(|g| g.to_string()),
                qty: d(r, "qty"),
                amount: out.amount,
                line_discount: d(r, "discount_percent") > Decimal::ZERO,
                delivery: item.is_some_and(|i| b(i, "is_delivery_charge")),
                added_by: s(r, "added_by_promotion").map(str::to_string),
            }
        })
        .collect();
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

/// Counts each code a submitted document used, and each promotion it applied, once (`step` 1), or
/// gives them back when it is cancelled (`step` -1). Counting refuses a code or promotion that is
/// used up meanwhile: of two documents taking the last use at once, the first to commit has it.
pub async fn count_uses(hc: &mut HookCtx<'_>, doc: &Doc, step: i64) -> Result<()> {
    let applied = rows(doc, "applied_promotions");
    if applied.is_empty() {
        return Ok(());
    }
    let table = |e: &str| hc.ctx.schema.entity(e).map(|x| ferrum_kernel::ddl::q(&x.table));
    let (Some(codes), Some(promos)) = (table("PromotionCode"), table("Promotion")) else { return Ok(()) };
    let used: BTreeSet<String> = applied.iter().filter_map(|a| s(a, "code")).map(str::to_string).collect();
    let promotions: BTreeSet<Uuid> = applied.iter().filter_map(|a| id(a, "promotion")).collect();
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
