//! Prices, discounts, taxes and totals for transaction documents, computed by `ferrum-calc`
//! (the same engine the browser can run as WASM), so every total is exact and reproducible.

use crate::util::{Company, b, company, d, date, exchange_rate, find, get, id, money, rows, s, set, to_stock};
use ferrum_calc::{CalcInput, CalcOutput, ChargeType, DocDiscount, LineIn, TaxIn, TaxRounding};
use ferrum_kernel::{Ctx, Doc, Error, FieldError, Filter, Kernel, Op, Result, Tx};
use ferrum_meta::{EffEntity, Expr, Scope};
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Selling,
    Buying,
}

impl Side {
    fn party_field(self) -> &'static str {
        match self {
            Side::Selling => "customer",
            Side::Buying => "supplier",
        }
    }
    fn party_entity(self) -> &'static str {
        match self {
            Side::Selling => "Customer",
            Side::Buying => "Supplier",
        }
    }
    fn template_kind(self) -> &'static str {
        match self {
            Side::Selling => "Sales",
            Side::Buying => "Purchase",
        }
    }
}

fn charge_type(s: &str) -> ChargeType {
    match s {
        "On Previous Row Amount" => ChargeType::OnPreviousRowAmount,
        "On Previous Row Total" => ChargeType::OnPreviousRowTotal,
        "Actual" => ChargeType::Actual,
        "On Item Quantity" => ChargeType::OnItemQuantity,
        _ => ChargeType::OnNetTotal,
    }
}

/// The price list's rate for one `uom` of an item: a price kept for that unit, else one for the
/// stock unit (or for no unit in particular) times `factor`.
#[allow(clippy::too_many_arguments)]
async fn item_price(k: &Kernel, ctx: &Ctx, tx: &mut Tx, item: &Doc, price_list: Uuid, on: &str, uom: Option<Uuid>, factor: Decimal) -> Result<Option<Decimal>> {
    let Some(item_id) = id(item, "id") else { return Ok(None) };
    let prices = find(
        k,
        ctx,
        tx,
        "ItemPrice",
        vec![
            Filter::eq("item", item_id.to_string()),
            Filter::eq("price_list", price_list.to_string()),
            Filter { field: "valid_from".into(), op: Op::Lte, value: json!(on) },
        ],
        Some("valid_from desc"),
        20,
    )
    .await?;
    let valid: Vec<&Doc> = prices.iter().filter(|p| s(p, "valid_upto").is_none_or(|u| u >= on)).collect();
    if let Some(p) = valid.iter().find(|p| uom.is_some() && id(p, "uom") == uom) {
        return Ok(Some(d(p, "rate")));
    }
    let stock = id(item, "stock_uom");
    Ok(valid.iter().find(|p| id(p, "uom").is_none() || id(p, "uom") == stock).map(|p| (d(p, "rate") * factor).round_dp(9)))
}

/// The line's unit (its item's stock unit unless another is chosen) and how many stock units it
/// holds, from the item's units. A unit the item has no conversion for is refused, unless it was
/// chosen for the item the line had before (then the line goes back to the stock unit).
fn line_unit(row: &mut Doc, item: &Doc, i: usize, item_changed: bool) -> Result<(Option<Uuid>, Decimal)> {
    let stock = id(item, "stock_uom");
    let factor_of = |uom: Option<Uuid>| {
        if uom == stock {
            Some(Decimal::ONE)
        } else {
            rows(item, "uoms").iter().find(|u| uom.is_some() && id(u, "uom") == uom).map(|u| d(u, "conversion_factor")).filter(|f| *f > Decimal::ZERO)
        }
    };
    let mut uom = id(row, "uom").or(stock);
    let factor = match factor_of(uom) {
        Some(f) => f,
        None if item_changed => {
            uom = stock;
            Decimal::ONE
        }
        None => {
            let name = s(item, "item_name").unwrap_or("this item");
            return Err(Error::Validation(vec![
                FieldError::new("uom", format!("{name} has no conversion from this unit to its stock unit; add the unit to the item")).in_row("items", i),
            ]));
        }
    };
    set(row, "uom", uom.map(|u| json!(u.to_string())).unwrap_or(Value::Null));
    set(row, "conversion_factor", money(factor));
    Ok((uom, factor))
}

/// What a line's price was set for: `item|uom|factor|price`.
fn price_basis(item: Uuid, uom: Option<Uuid>, factor: Decimal, price: Decimal) -> String {
    format!("{item}|{}|{}|{}", uom.map(|u| u.to_string()).unwrap_or_default(), factor.normalize(), price.normalize())
}

fn parse_basis(text: &str) -> Option<(Uuid, Option<Uuid>, Decimal, Decimal)> {
    let mut p = text.split('|');
    let item = p.next()?.parse().ok()?;
    let uom = match p.next()? {
        "" => None,
        u => Some(u.parse().ok()?),
    };
    Some((item, uom, p.next()?.parse().ok()?, p.next()?.parse().ok()?))
}

/// A price set for another item or unit, and not changed since, no longer applies: it is looked
/// up again (another item) or converted (another unit). A price the user changed stays theirs.
fn restate_price(row: &mut Doc, item: Uuid, uom: Option<Uuid>, factor: Decimal) {
    let price = d(row, "price_list_rate");
    let Some((was_item, was_uom, was_factor, was_price)) = s(row, "price_basis").and_then(parse_basis) else { return };
    if price.is_zero() || price != was_price {
        return;
    }
    if was_item != item {
        set(row, "price_list_rate", money(Decimal::ZERO));
    } else if was_uom != uom && was_factor > Decimal::ZERO {
        set(row, "price_list_rate", money((price / was_factor * factor).round_dp(9)));
    }
}

async fn default_price_list(k: &Kernel, ctx: &Ctx, tx: &mut Tx, side: Side) -> Result<Option<Uuid>> {
    let flag = match side {
        Side::Selling => "selling",
        Side::Buying => "buying",
    };
    let lists = find(k, ctx, tx, "PriceList", vec![Filter::eq(flag, true), Filter::eq("enabled", true)], Some("created_at asc"), 1).await?;
    Ok(lists.first().and_then(|l| id(l, "id")))
}

struct Rule {
    doc: Doc,
    expr: Option<Expr>,
}

async fn pricing_rules(k: &Kernel, ctx: &Ctx, tx: &mut Tx) -> Result<Vec<Rule>> {
    if ctx.schema.entity("PricingRule").is_none() {
        return Ok(vec![]);
    }
    let docs = find(k, ctx, tx, "PricingRule", vec![Filter::eq("disabled", false)], Some("created_at asc"), 2000).await?;
    Ok(docs
        .into_iter()
        .map(|doc| {
            let expr = s(&doc, "condition").and_then(|c| Expr::compile(c).ok());
            Rule { doc, expr }
        })
        .collect())
}

#[allow(clippy::too_many_arguments)]
fn best_rule<'a>(rules: &'a [Rule], item: &Doc, qty: Decimal, party: Option<&Doc>, on: &str, cel_doc: &Value, user: &Value) -> Option<&'a Doc> {
    rules
        .iter()
        .filter(|r| {
            let rd = &r.doc;
            let applies = match s(rd, "apply_on") {
                Some("Item") => id(rd, "item") == id(item, "id"),
                Some("Item Group") => id(rd, "item_group").is_some() && id(rd, "item_group") == id(item, "item_group"),
                _ => true,
            };
            let party_ok = id(rd, "customer").is_none_or(|c| party.and_then(|p| id(p, "id")) == Some(c))
                && id(rd, "customer_group").is_none_or(|g| party.and_then(|p| id(p, "customer_group")) == Some(g));
            let qty_ok = qty.abs() >= d(rd, "min_qty") && (d(rd, "max_qty").is_zero() || qty.abs() <= d(rd, "max_qty"));
            let date_ok = s(rd, "valid_from").is_none_or(|f| f <= on) && s(rd, "valid_upto").is_none_or(|u| u >= on);
            let cond_ok = r.expr.as_ref().is_none_or(|e| e.eval_bool(&Scope { doc: cel_doc, old: None, parent: None, user }).unwrap_or(false));
            applies && party_ok && qty_ok && date_ok && cond_ok
        })
        .max_by(|a, b| {
            let pa = a.doc.get("priority").and_then(Value::as_i64).unwrap_or(1);
            let pb = b.doc.get("priority").and_then(Value::as_i64).unwrap_or(1);
            pa.cmp(&pb).then(d(&a.doc, "discount_percent").cmp(&d(&b.doc, "discount_percent")))
        })
        .map(|r| &r.doc)
}

/// Picks the default tax template for the company/kind whose condition matches the document.
async fn default_template(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, side: Side, cel_doc: &Value) -> Result<Option<Doc>> {
    let templates = find(
        k,
        ctx,
        tx,
        "TaxTemplate",
        vec![Filter::eq("company", company.to_string()), Filter::eq("kind", side.template_kind()), Filter::eq("is_default", true)],
        Some("created_at asc"),
        50,
    )
    .await?;
    let user = ctx.user_value();
    let chosen = templates.into_iter().find(|t| match s(t, "condition") {
        None => true,
        Some(c) => Expr::compile(c).ok().is_some_and(|e| e.eval_bool(&Scope { doc: cel_doc, old: None, parent: None, user: &user }).unwrap_or(false)),
    });
    // Lists carry no child rows: load the chosen template whole, with its tax rows.
    match chosen.and_then(|t| id(&t, "id")) {
        Some(t) => Ok(Some(get(k, ctx, tx, "TaxTemplate", t).await?)),
        None => Ok(None),
    }
}

fn copy_tax_rows(template: &Doc) -> Vec<Value> {
    rows(template, "rows")
        .into_iter()
        .map(|r| {
            let mut out = Map::new();
            for k in [
                "charge_type",
                "account",
                "description",
                "rate",
                "amount",
                "row_ref",
                "included_in_price",
                "deduct",
                "reverse_charge",
                "reverse_charge_account",
                "category",
                "exemption_reason",
                "cost_center",
            ] {
                if let Some(v) = r.get(k) {
                    out.insert(k.into(), v.clone());
                }
            }
            out.insert("id".into(), json!(Uuid::now_v7().to_string()));
            Value::Object(out)
        })
        .collect()
}

/// The engine's tax rows for a document's tax rows (`taxes`), keyed by each row's id (`t{i}` for
/// rows not saved yet). Apps that price with ERP's tax templates (Commerce's catalog) use it too,
/// so a tax row means the same everywhere.
// The engine's inputs gain fields over time; those not named here take their defaults.
#[allow(clippy::needless_update)]
pub fn tax_in(taxes: &[Doc]) -> Vec<TaxIn> {
    taxes
        .iter()
        .enumerate()
        .map(|(i, t)| TaxIn {
            key: tax_key(t, i),
            charge_type: charge_type(s(t, "charge_type").unwrap_or("On Net Total")),
            rate: d(t, "rate"),
            amount: d(t, "amount"),
            row_ref: t.get("row_ref").and_then(Value::as_i64).filter(|r| *r >= 1).map(|r| (r - 1) as usize),
            included_in_price: b(t, "included_in_price"),
            deduct: b(t, "deduct"),
            reverse_charge: b(t, "reverse_charge"),
            ..Default::default()
        })
        .collect()
}

fn tax_key(t: &Doc, i: usize) -> String {
    s(t, "id").map(str::to_string).unwrap_or_else(|| format!("t{i}"))
}

/// A line's own rates for the document's tax rows, from its item's tax rates (by tax account),
/// keyed like [`tax_in`].
pub fn line_tax_rates(line: &Doc, taxes: &[Doc]) -> BTreeMap<String, Decimal> {
    let Some(m) = line.get("item_tax_rates").and_then(Value::as_object) else { return BTreeMap::new() };
    taxes
        .iter()
        .enumerate()
        .filter_map(|(i, t)| {
            let rate = m.get(s(t, "account")?)?.as_str()?.parse().ok()?;
            Some((tax_key(t, i), rate))
        })
        .collect()
}

/// The engine's input for a document's priced lines (prices, line discounts, item tax rates) and
/// tax rows: what [`calculate`] computes totals from, and what a register prices with offline (the
/// same engine in the browser, `ferrum-calc` built for WASM). `rounded_total`: the document has a
/// cash-rounded total.
// The engine's inputs gain fields over time; those not named here take their defaults.
#[allow(clippy::needless_update)]
pub fn calc_input(items: &[Doc], taxes: &[Doc], discount_percent: Decimal, co: &Company, precision: u32, rate: Decimal, rounded_total: bool) -> CalcInput {
    let lines: Vec<LineIn> = items
        .iter()
        .map(|r| LineIn {
            key: s(r, "id").unwrap_or_default().to_string(),
            qty: d(r, "qty"),
            price: d(r, "price_list_rate"),
            // Its own discount, then the share promotions take of what that leaves.
            discount_percent: ferrum_erp_promotions::combined_percent(d(r, "discount_percent"), d(r, "promotion_percent")),
            tax_rates: line_tax_rates(r, taxes),
            // Per-unit taxes count stock units.
            tax_qty: r.get("stock_qty").is_some_and(|v| !v.is_null()).then(|| d(r, "stock_qty")),
            ..Default::default()
        })
        .collect();
    CalcInput {
        precision,
        base_precision: Some(co.precision),
        conversion_rate: rate,
        rounding: co.rounding,
        lines,
        taxes: tax_in(taxes),
        discount: (discount_percent > Decimal::ZERO).then_some(DocDiscount::Percent { value: discount_percent }),
        round_total_to: if rounded_total { co.round_total_to } else { None },
        tax_rounding: if s(&co.doc, "tax_rounding") == Some("Per Line") { TaxRounding::PerLine } else { TaxRounding::PerDocument },
        ..Default::default()
    }
}

/// A scalar SQL subquery for the account in `column` (`default_income_account` or
/// `default_expense_account`) on item group `group` (an SQL expression), else on its nearest parent
/// group that has one; `$1` is the tenant. Shared with SQL that needs the same answer in a query.
pub fn group_account_sql(group: &str, column: &str) -> String {
    format!(
        "(with recursive gw(parent, acc, depth) as (select g0.parent_group, g0.{column}, 0 from e_item_group g0 where g0.tenant_id = $1 and g0.id = {group} \
         union all select gp.parent_group, gp.{column}, gw.depth + 1 from gw join e_item_group gp on gp.tenant_id = $1 and gp.id = gw.parent \
         where gw.acc is null and gw.depth < 32) select acc from gw where acc is not null limit 1)"
    )
}

/// The account a line of `item` books to when the line names none: the item's own, then its item
/// group's (or the nearest parent group's), then the company default. Selling reads income
/// accounts, buying expense accounts. Every line that picks one goes through here.
pub async fn item_account(ctx: &Ctx, tx: &mut Tx, co: &Company, item: &Doc, side: Side) -> Result<Option<Uuid>> {
    let (own, default) = match side {
        Side::Selling => ("income_account", "default_income_account"),
        Side::Buying => ("expense_account", "default_expense_account"),
    };
    if let Some(a) = id(item, own) {
        return Ok(Some(a));
    }
    if let Some(g) = id(item, "item_group") {
        let a: Option<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select {}", group_account_sql("$2", default))))
            .bind(ctx.tenant)
            .bind(g)
            .fetch_one(&mut **tx)
            .await?;
        if a.is_some() {
            return Ok(a);
        }
    }
    Ok(co.account(default))
}

/// A line without an item: named by its description, in no unit of an item (a quantity is its own,
/// and per-unit taxes count it), booked to the company's default income or expense account unless
/// it names one, with the document's or the company's cost centre.
fn free_line(row: &mut Doc, doc: &Doc, co: &Company, side: Side, has_child: &dyn Fn(&str) -> bool) {
    if has_child("item_name") {
        set(row, "item_name", row.get("description").cloned().unwrap_or(Value::Null));
    }
    for f in ["price_basis", "stock_uom"] {
        if has_child(f) {
            set(row, f, Value::Null);
        }
    }
    if has_child("conversion_factor") {
        set(row, "conversion_factor", money(Decimal::ONE));
    }
    if has_child("stock_qty") {
        set(row, "stock_qty", money(d(row, "qty")));
    }
    let (field, default) = match side {
        Side::Selling => ("income_account", "default_income_account"),
        Side::Buying => ("expense_account", "default_expense_account"),
    };
    if has_child(field)
        && id(row, field).is_none()
        && let Some(a) = co.account(default)
    {
        set(row, field, a.to_string());
    }
    if has_child("cost_center")
        && id(row, "cost_center").is_none()
        && let Some(c) = id(doc, "cost_center").or(co.account("default_cost_center"))
    {
        set(row, "cost_center", c.to_string());
    }
}

/// Whether a person's typed price on a document's lines (of entity `line`) is dropped where the
/// line has an item. Only where the line's definition makes `price_list_rate` read-only on a
/// condition (`read_only_if`, as ERP's sales order and invoice lines do for lines with an item):
/// the kernel judges such a condition on the stored row, so on a new row (or one whose item just
/// changed) it lets a person's price through, and the engine stands in for it. A price that is
/// plainly editable (a Point of Sale override) is the definition's choice and kept; a plainly
/// read-only one never reaches here (the kernel drops it). The engine's own (elevated) writes and
/// buying, whose prices are typed, are never touched.
pub(crate) fn guards_typed_price(line: Option<&EffEntity>, side: Side, is_system: bool) -> bool {
    side == Side::Selling && !is_system && line.and_then(|l| l.field("price_list_rate")).is_some_and(|f| !f.read_only && f.read_only_if.is_some())
}

/// Each stored line's item by line id (empty for a new document) when [`guards_typed_price`]
/// holds, so that a typed price on a new item line, or one whose item changed, is dropped and the
/// price list or a pricing rule fills it; a line without an item keeps its price. `None` otherwise.
async fn typed_prices(k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &Doc, side: Side) -> Result<Option<HashMap<String, Option<Uuid>>>> {
    let line = ent.field("items").and_then(|f| f.of.as_deref()).and_then(|c| ctx.schema.entity(c)).map(|e| &**e);
    if !guards_typed_price(line, side, ctx.is_system) {
        return Ok(None);
    }
    let Some(doc_id) = id(doc, "id") else { return Ok(Some(HashMap::new())) };
    let stored = match k.get_tx(&crate::util::sys(ctx), tx, &ent.def.name, doc_id, false).await {
        Ok(d) => d,
        Err(Error::NotFound(_)) => return Ok(Some(HashMap::new())),
        Err(e) => return Err(e),
    };
    Ok(Some(rows(&stored, "items").iter().filter_map(|r| Some((s(r, "id")?.to_string(), id(r, "item")))).collect()))
}

/// Fills prices, discounts, item tax rates, taxes and totals. Returns the company for reuse.
pub async fn calculate(k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<Company> {
    // The addresses the countries' rules read, then their rules (India: the GST template for the
    // place of supply), so the taxes calculated below are the ones they chose.
    crate::localization::fill_addresses(k, ctx, tx, ent, doc, side).await?;
    crate::localization::fill_delivery(k, ctx, tx, ent, doc).await?;
    crate::localization::determine(k, ctx, tx, ent, doc, side).await?;
    let company_id = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
    let co = company(k, ctx, tx, company_id).await?;
    let on = date(doc, "posting_date")?.format("%Y-%m-%d").to_string();
    let party = match id(doc, side.party_field()) {
        Some(pid) => Some(get(k, ctx, tx, side.party_entity(), pid).await?),
        None => None,
    };

    // Currency and conversion rate.
    if id(doc, "currency").is_none() {
        let cur = party.as_ref().and_then(|p| id(p, "default_currency")).unwrap_or(co.currency_id);
        set(doc, "currency", cur.to_string());
    }
    let currency_id = id(doc, "currency").unwrap_or(co.currency_id);
    let (_, precision) = crate::util::currency(k, ctx, tx, currency_id).await?;
    let rate = if currency_id == co.currency_id {
        Decimal::ONE
    } else {
        let given = d(doc, "conversion_rate");
        if given > Decimal::ZERO && given != Decimal::ONE {
            given
        } else {
            exchange_rate(k, ctx, tx, currency_id, co.currency_id, date(doc, "posting_date")?).await?
        }
    };
    set(doc, "conversion_rate", money(rate));

    // Price list.
    if ent.field("price_list").is_some() && id(doc, "price_list").is_none() {
        let pl = match party.as_ref().and_then(|p| id(p, "default_price_list")) {
            Some(p) => Some(p),
            None => default_price_list(k, ctx, tx, side).await?,
        };
        if let Some(p) = pl {
            set(doc, "price_list", p.to_string());
        }
    }
    let price_list = id(doc, "price_list");
    let rules = if side == Side::Selling { pricing_rules(k, ctx, tx).await? } else { vec![] };
    let user = ctx.user_value();
    let cel_doc = ent.cel_view(&Value::Object(doc.clone()), &ctx.schema);

    let child_ent = ent.field("items").and_then(|f| f.of.clone()).and_then(|c| ctx.schema.entity(&c).cloned());
    let has_child = |f: &str| child_ent.as_ref().is_some_and(|c| c.field(f).is_some());
    let mut item_cache: HashMap<Uuid, Doc> = HashMap::new();
    let typed = typed_prices(k, ctx, tx, ent, doc, side).await?;
    let mut items = rows(doc, "items");
    // Each line's item tax rates (by tax account), for the tax breakdown's categories.
    let mut line_rates: Vec<Vec<Doc>> = Vec::with_capacity(items.len());
    // Lines that sell stored value taxed when it is redeemed, not now.
    let mut untaxed = vec![false; items.len()];
    let mut stored_value_kinds = HashMap::new();
    for (i, row) in items.iter_mut().enumerate() {
        let Some(item_id) = id(row, "item") else {
            // A line without an item (rent, a fee, a one-off charge): its own description, price and
            // account. No price list, pricing rule, unit or warehouse; taxed as the document is.
            line_rates.push(Vec::new());
            free_line(row, doc, &co, side, &has_child);
            continue;
        };
        if let std::collections::hash_map::Entry::Vacant(e) = item_cache.entry(item_id) {
            e.insert(get(k, ctx, tx, "Item", item_id).await?);
        }
        let item = item_cache[&item_id].clone();
        if typed.as_ref().is_some_and(|stored| stored.get(s(row, "id").unwrap_or_default()) != Some(&Some(item_id))) {
            // A person's price on a new item line, or one whose item changed, is not theirs to give.
            set(row, "price_list_rate", money(Decimal::ZERO));
        }
        line_rates.push(rows(&item, "tax_rates"));
        let qty = d(row, "qty");
        // Units: quantities and prices are per the line's unit; stock moves count stock units.
        let item_changed = s(row, "price_basis").and_then(parse_basis).is_some_and(|(was, ..)| was != item_id);
        // Not priced before, or priced for another item: what the item says still applies.
        let fresh = item_changed || s(row, "price_basis").is_none();
        let (uom, factor) = if has_child("conversion_factor") { line_unit(row, &item, i, item_changed)? } else { (id(row, "uom"), Decimal::ONE) };
        if has_child("price_basis") {
            restate_price(row, item_id, uom, factor);
        }
        if d(row, "price_list_rate").is_zero() {
            let mut price = match price_list {
                Some(pl) => item_price(k, ctx, tx, &item, pl, &on, uom, factor).await?,
                None => None,
            };
            if price.is_none() {
                let fallback = match side {
                    Side::Selling => d(&item, "standard_rate"),
                    Side::Buying => d(&item, "last_purchase_rate"),
                };
                price = (!fallback.is_zero()).then_some((fallback * factor).round_dp(9));
            }
            // Prices are kept per the document currency.
            if let Some(p) = price {
                set(row, "price_list_rate", money(if rate == Decimal::ONE { p } else { (p / rate).round_dp(9) }));
            }
        }
        if side == Side::Selling
            && row.get("discount_percent").is_none_or(Value::is_null)
            && let Some(rule) = best_rule(&rules, &item, to_stock(qty, factor), party.as_ref(), &on, &cel_doc, &user)
        {
            // Rules speak of stock units: quantities and rates.
            if d(rule, "rate") > Decimal::ZERO {
                set(row, "price_list_rate", money((d(rule, "rate") * factor).round_dp(9)));
            }
            if d(rule, "discount_percent") > Decimal::ZERO {
                set(row, "discount_percent", money(d(rule, "discount_percent")));
            }
        }
        if has_child("stock_qty") {
            set(row, "stock_qty", money(to_stock(qty, factor)));
        }
        if has_child("price_basis") {
            set(row, "price_basis", price_basis(item_id, uom, factor, d(row, "price_list_rate")));
        }
        match side {
            Side::Selling if has_child("income_account") && id(row, "income_account").is_none() => {
                if let Some(a) = item_account(ctx, tx, &co, &item, side).await? {
                    set(row, "income_account", a.to_string());
                }
            }
            Side::Buying if has_child("expense_account") && id(row, "expense_account").is_none() => {
                let acc = if b(&item, "is_stock_item") { co.account("srbnb_account") } else { item_account(ctx, tx, &co, &item, side).await? };
                if let Some(a) = acc {
                    set(row, "expense_account", a.to_string());
                }
            }
            _ => {}
        }
        if side == Side::Selling && has_child("stored_value_kind") {
            untaxed[i] = crate::stored_value::price_line(k, ctx, tx, &item, row, &mut stored_value_kinds).await?;
        }
        // A line is deferred like its item when it comes with the days its service covers (without
        // them it is sold at once); once added, its own setting stands. Unset checkboxes read as
        // unchecked by now, so a new line is what tells the item's say from the line's.
        let flag = if side == Side::Selling { "enable_deferred_revenue" } else { "enable_deferred_expense" };
        if has_child(flag) && fresh && b(&item, flag) && !b(row, flag) && s(row, "service_start_date").is_some() && s(row, "service_end_date").is_some() {
            set(row, flag, true);
        }
        if has_child("warehouse")
            && id(row, "warehouse").is_none()
            && let Some(w) = id(doc, "set_warehouse").or_else(|| id(&item, "default_warehouse"))
        {
            set(row, "warehouse", w.to_string());
        }
        if has_child("cost_center")
            && id(row, "cost_center").is_none()
            && let Some(c) = id(doc, "cost_center").or(co.account("default_cost_center"))
        {
            set(row, "cost_center", c.to_string());
        }
    }
    // Promotions over the whole document, on what each line came to (a free item they add is
    // priced like any line, so the document is priced again).
    if side == Side::Selling && crate::promotion::apply(k, ctx, tx, ent, doc, &mut items, &item_cache, &co, precision, rate).await? {
        set(doc, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
        return Box::pin(calculate(k, ctx, tx, ent, doc, side)).await;
    }
    set(doc, "items", Value::Array(items.iter().cloned().map(Value::Object).collect()));

    // Taxes from the template (explicit, or the matching default).
    if ent.field("taxes").is_some() && rows(doc, "taxes").is_empty() {
        let template = match id(doc, "tax_template") {
            Some(t) => Some(get(k, ctx, tx, "TaxTemplate", t).await?),
            None => default_template(k, ctx, tx, co.id, side, &cel_doc).await?,
        };
        if let Some(t) = template {
            if ent.field("tax_template").is_some() {
                set(doc, "tax_template", s(&t, "id").unwrap_or_default().to_string());
            }
            set(doc, "taxes", Value::Array(copy_tax_rows(&t)));
        }
    }
    let mut taxes = rows(doc, "taxes");

    // Each line's own rates: its item's, as the countries' rules leave them (the EU: the rate of
    // the country whose VAT applies, for the line's category).
    let item_docs: Vec<Doc> = items.iter().map(|row| id(row, "item").and_then(|it| item_cache.get(&it)).cloned().unwrap_or_default()).collect();
    crate::localization::line_rates(k, ctx, tx, doc, &items, &item_docs, &taxes, &mut line_rates, side).await?;
    for (rates, _) in line_rates.iter_mut().zip(&untaxed).filter(|(_, u)| **u) {
        *rates = crate::stored_value::untaxed_rates(&taxes);
    }
    for (i, row) in items.iter_mut().enumerate() {
        if has_child("item_tax_rates") {
            let own: Map<String, Value> =
                line_rates[i].iter().filter_map(|r| Some((s(r, "tax_account")?.to_string(), json!(d(r, "rate").to_string())))).collect();
            set(row, "item_tax_rates", Value::Object(own));
        }
        if has_child("tax_categories") {
            // What e-invoices say of each line: its category and exemption reason per tax account.
            let own: Map<String, Value> = line_rates[i]
                .iter()
                .filter(|r| s(r, "category").is_some() || s(r, "exemption_reason").is_some())
                .filter_map(|r| Some((s(r, "tax_account")?.to_string(), json!({"category": s(r, "category"), "exemption_reason": s(r, "exemption_reason")}))))
                .collect();
            set(row, "tax_categories", Value::Object(own));
        }
    }

    // Calculation.
    let input = calc_input(&items, &taxes, d(doc, "discount_percent"), &co, precision, rate, ent.field("rounded_total").is_some());
    let out = ferrum_calc::compute(&input).map_err(|e| Error::Validation(vec![FieldError::new("items", e.to_string())]))?;
    for (row, line) in items.iter_mut().zip(&out.lines) {
        set(row, "rate", money(line.rate));
        set(row, "amount", money(line.amount));
        if has_child("net_amount") {
            set(row, "net_amount", money(line.net_amount));
        }
        if has_child("base_net_amount") {
            set(row, "base_net_amount", money(line.base_net_amount));
        }
    }
    for (t, o) in taxes.iter_mut().zip(&out.taxes) {
        set(t, "tax_amount", money(o.tax_amount));
        set(t, "total", money(o.total));
        set(t, "base_tax_amount", money(o.base_tax_amount));
        set(t, "base_total", money(o.base_total));
    }
    if ent.field("tax_breakdown").is_some() {
        set(doc, "tax_breakdown", Value::Array(tax_breakdown(&input, &taxes, &line_rates, &out).into_iter().map(Value::Object).collect()));
    }
    set(doc, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
    if ent.field("taxes").is_some() {
        set(doc, "taxes", Value::Array(taxes.into_iter().map(Value::Object).collect()));
    }
    let totals = [
        ("total", out.total),
        ("discount_amount", out.discount_amount),
        ("net_total", out.net_total),
        ("total_taxes", out.total_taxes),
        ("grand_total", out.grand_total),
        ("rounding_adjustment", out.rounding_adjustment),
        ("rounded_total", out.rounded_total),
        ("base_net_total", out.base_net_total),
        ("base_grand_total", out.base_grand_total),
        ("base_rounded_total", out.base_rounded_total),
    ];
    for (f, v) in totals {
        if ent.field(f).is_some() {
            set(doc, f, money(v));
        }
    }
    Ok(co)
}

/// The category a line falls in for a tax row: its item's for the row's account, else the row's
/// (when the item keeps the row's rate), else what the rate says.
fn category_of(row: &Doc, item_rate: Option<&Doc>, rate: Decimal) -> String {
    let rows_own = s(row, "category").filter(|_| item_rate.is_none_or(|r| d(r, "rate") == d(row, "rate")));
    match item_rate.and_then(|r| s(r, "category")).or(rows_own) {
        Some(c) => c.to_string(),
        None if b(row, "reverse_charge") => "Reverse Charge".into(),
        None if rate > Decimal::ZERO => "Standard".into(),
        None => "Zero Rated".into(),
    }
}

/// What a document's taxes come to per tax row, category, rate and exemption reason (EN 16931's
/// VAT breakdown, BG-23): the taxable amount (what the row is charged on: the lines' net amounts,
/// or the earlier row's amounts or totals it refers to) and the tax, in the document's and the
/// company's currency. Fixed (Actual) charges have no rate and are left out. `line_rates`: each
/// line's item tax rates.
pub fn tax_breakdown(input: &CalcInput, taxes: &[Doc], line_rates: &[Vec<Doc>], out: &CalcOutput) -> Vec<Doc> {
    // Of a row's earlier rows, what each adds to a running total: nothing when reverse-charged.
    let signed = |q: usize, v: Decimal| {
        if b(&taxes[q], "reverse_charge") {
            Decimal::ZERO
        } else if b(&taxes[q], "deduct") {
            -v
        } else {
            v
        }
    };
    let mut groups: BTreeMap<(usize, String, Decimal, String), [Decimal; 4]> = BTreeMap::new();
    for (k, (row, tin)) in taxes.iter().zip(&input.taxes).enumerate() {
        if tin.charge_type == ChargeType::Actual {
            continue;
        }
        let account = s(row, "account");
        for (i, (line, lin)) in out.lines.iter().zip(&input.lines).enumerate() {
            let rate = lin.tax_rates.get(&tin.key).copied().unwrap_or(tin.rate);
            let item_rate = line_rates.get(i).and_then(|rs| rs.iter().find(|r| account.is_some() && s(r, "tax_account") == account));
            let reason = item_rate.and_then(|r| s(r, "exemption_reason")).or_else(|| s(row, "exemption_reason")).unwrap_or_default().to_string();
            let (taxable, base_taxable) = match (tin.charge_type, tin.row_ref) {
                (ChargeType::OnPreviousRowAmount, Some(j)) => (line.taxes[j], line.base_taxes[j]),
                (ChargeType::OnPreviousRowTotal, Some(j)) => {
                    (0..=j).fold((line.net_amount, line.base_net_amount), |(t, bt), q| (t + signed(q, line.taxes[q]), bt + signed(q, line.base_taxes[q])))
                }
                _ => (line.net_amount, line.base_net_amount),
            };
            let g = groups.entry((k, category_of(row, item_rate, rate), rate.normalize(), reason)).or_default();
            g[0] += taxable;
            g[1] += line.taxes[k];
            g[2] += base_taxable;
            g[3] += line.base_taxes[k];
        }
    }
    groups
        .into_iter()
        .map(|((k, category, rate, reason), [taxable, tax, base_taxable, base_tax])| {
            let row = &taxes[k];
            let mut g = Doc::new();
            g.insert("id".into(), json!(Uuid::now_v7().to_string()));
            g.insert("description".into(), json!(s(row, "description")));
            g.insert("account".into(), json!(s(row, "account")));
            g.insert("category".into(), json!(category));
            g.insert("rate".into(), money(rate));
            g.insert("exemption_reason".into(), if reason.is_empty() { Value::Null } else { json!(reason) });
            g.insert("reverse_charge".into(), json!(b(row, "reverse_charge")));
            g.insert("taxable_amount".into(), money(taxable));
            g.insert("tax_amount".into(), money(tax));
            g.insert("base_taxable_amount".into(), money(base_taxable));
            g.insert("base_tax_amount".into(), money(base_tax));
            g
        })
        .collect()
}

/// Re-reads the per-line and per-tax breakup computed by `calculate` for postings.
pub struct Breakup {
    pub lines: Vec<(Doc, Decimal)>,
    pub taxes: Vec<(Doc, Decimal)>,
}

/// Base-currency amounts of an already-calculated document (line net, tax rows).
pub fn breakup(doc: &Doc) -> Breakup {
    let rate = d(doc, "conversion_rate").max(Decimal::ZERO);
    let rate = if rate.is_zero() { Decimal::ONE } else { rate };
    let lines = rows(doc, "items")
        .into_iter()
        .map(|r| {
            let base = if r.get("base_net_amount").is_some_and(|v| !v.is_null()) { d(&r, "base_net_amount") } else { d(&r, "net_amount") * rate };
            (r, base)
        })
        .collect();
    let taxes = rows(doc, "taxes")
        .into_iter()
        .map(|t| {
            let base = d(&t, "base_tax_amount");
            let deduct = b(&t, "deduct");
            (t, if deduct { -base } else { base })
        })
        .collect();
    Breakup { lines, taxes }
}
