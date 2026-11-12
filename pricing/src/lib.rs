//! A whole basket priced from data alone: each line's price from a price list (by date and unit) or
//! the item's standard rate, its pricing rule, the promotions over the whole basket (with the free
//! items they add), the taxes per line by rate rows (inclusive or exclusive), the document's
//! discount, rounding by the currency's rules, the totals and the tax summary per rate.
//!
//! Pure and deterministic: no database, no clock (the moment is an input), no floats, no country;
//! the same snapshot and basket give the same bytes out. ERP's server prices documents with the
//! pieces here ([`list_price`], [`unit_factor`], [`best_rule`], [`line_tax_rates`],
//! [`before_promotions`], [`tax_summary`]) and the arithmetic of `ferrum-calc` and
//! `ferrum-erp-promotions`; [`price_basket`] puts them together in the server's order for a
//! [`Snapshot`], which is what a till keeps to price offline (feature `wasm`: `priceBasket`). The
//! schema of both is versioned ([`SCHEMA`]) and written down in `docs/pricing.md`.

use ferrum_calc::{CalcInput, CalcOutput, ChargeType, DocDiscount, LineIn, compute};
use ferrum_erp_promotions as promotions;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The version of [`Snapshot`], [`Basket`] and [`Priced`]. A reader refuses another.
pub const SCHEMA: u32 = 1;

/// A price list's price of an item: for one unit (none: the stock unit), from a day, until a day.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Price {
    pub item: String,
    #[serde(default)]
    pub uom: Option<String>,
    pub rate: Decimal,
    /// `YYYY-MM-DD`; none: always.
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub valid_upto: Option<String>,
}

/// The price of one `uom` of `item` on `on` (`YYYY-MM-DD`): the latest price in force kept for that
/// unit, else the latest for the stock unit (or for no unit in particular) times `factor`, at nine
/// places. Prices starting on the same day keep the order given.
pub fn list_price(prices: &[Price], item: &str, on: &str, uom: Option<&str>, stock_uom: Option<&str>, factor: Decimal) -> Option<Decimal> {
    let mut valid: Vec<&Price> = prices
        .iter()
        .filter(|p| p.item == item && p.valid_from.as_deref().is_none_or(|f| f <= on) && p.valid_upto.as_deref().is_none_or(|u| u >= on))
        .collect();
    valid.sort_by(|a, b| b.valid_from.cmp(&a.valid_from));
    if let Some(p) = valid.iter().find(|p| uom.is_some() && p.uom.as_deref() == uom) {
        return Some(p.rate);
    }
    valid.iter().find(|p| p.uom.is_none() || p.uom.as_deref() == stock_uom).map(|p| (p.rate * factor).round_dp(9))
}

/// One of an item's units and how many stock units it holds.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    pub uom: String,
    pub factor: Decimal,
}

/// How many stock units one `uom` holds: one for the stock unit, else the item's conversion (none
/// when it has none above zero).
pub fn unit_factor(stock_uom: Option<&str>, units: &[Unit], uom: Option<&str>) -> Option<Decimal> {
    if uom == stock_uom {
        return Some(Decimal::ONE);
    }
    units.iter().find(|u| uom.is_some() && Some(u.uom.as_str()) == uom).map(|u| u.factor).filter(|f| *f > Decimal::ZERO)
}

/// `qty` of a unit of `factor` stock units, in stock units, at six places.
pub fn to_stock(qty: Decimal, factor: Decimal) -> Decimal {
    if factor == Decimal::ONE { qty } else { (qty * factor).round_dp(6) }
}

/// A pricing rule: on an item, an item group or anything (`apply_on`), for a customer or a group,
/// within quantities (in stock units; `max_qty` zero: no most) and days; it sets a rate (per stock
/// unit) and a discount when above zero. A rule with a condition is the server's alone.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    /// `Item`, `Item Group`, else every item.
    #[serde(default)]
    pub apply_on: Option<String>,
    #[serde(default)]
    pub item: Option<String>,
    #[serde(default)]
    pub item_group: Option<String>,
    #[serde(default)]
    pub customer: Option<String>,
    #[serde(default)]
    pub customer_group: Option<String>,
    #[serde(default)]
    pub min_qty: Decimal,
    #[serde(default)]
    pub max_qty: Decimal,
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub valid_upto: Option<String>,
    /// None: 1.
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub discount_percent: Decimal,
    #[serde(default)]
    pub rate: Decimal,
}

/// Who buys what, how much and when, as a pricing rule asks.
#[derive(Debug, Clone, Copy, Default)]
pub struct RuleAsk<'a> {
    pub item: Option<&'a str>,
    pub item_group: Option<&'a str>,
    /// In stock units.
    pub qty: Decimal,
    pub customer: Option<&'a str>,
    pub customer_group: Option<&'a str>,
    /// `YYYY-MM-DD`.
    pub on: &'a str,
}

/// The rule that applies (`condition`: whether the rule at that place holds otherwise): the highest
/// priority, then the largest discount, then the last of equals.
pub fn best_rule<'a>(rules: &'a [Rule], ask: RuleAsk<'_>, condition: impl Fn(usize) -> bool) -> Option<&'a Rule> {
    rules
        .iter()
        .enumerate()
        .filter(|(i, r)| {
            let applies = match r.apply_on.as_deref() {
                Some("Item") => r.item.as_deref() == ask.item,
                Some("Item Group") => r.item_group.is_some() && r.item_group.as_deref() == ask.item_group,
                _ => true,
            };
            let party_ok =
                r.customer.as_deref().is_none_or(|c| ask.customer == Some(c)) && r.customer_group.as_deref().is_none_or(|g| ask.customer_group == Some(g));
            let qty_ok = ask.qty.abs() >= r.min_qty && (r.max_qty.is_zero() || ask.qty.abs() <= r.max_qty);
            let date_ok = r.valid_from.as_deref().is_none_or(|f| f <= ask.on) && r.valid_upto.as_deref().is_none_or(|u| u >= ask.on);
            applies && party_ok && qty_ok && date_ok && condition(*i)
        })
        .map(|(_, r)| r)
        .max_by(|a, b| a.priority.unwrap_or(1).cmp(&b.priority.unwrap_or(1)).then(a.discount_percent.cmp(&b.discount_percent)))
}

/// A line's rates for the document's tax rows (`(key, account)` each), from its item's rates by tax
/// account, keyed like the rows.
pub fn line_tax_rates(by_account: &BTreeMap<String, Decimal>, rows: &[(String, Option<String>)]) -> BTreeMap<String, Decimal> {
    rows.iter().filter_map(|(key, account)| Some((key.clone(), *by_account.get(account.as_deref()?)?))).collect()
}

/// What each line comes to before promotions: priced alone, with its own discount, without taxes,
/// the document's discount or cash rounding. What promotions are worked out on.
pub fn before_promotions(input: &CalcInput) -> Result<Vec<Decimal>, String> {
    let lines = input.lines.iter().map(|l| LineIn { tax_rates: BTreeMap::new(), ..l.clone() }).collect();
    let bare = CalcInput { lines, taxes: vec![], discount: None, round_total_to: None, ..input.clone() };
    Ok(compute(&bare).map_err(|e| e.to_string())?.lines.into_iter().map(|l| l.amount).collect())
}

/// What a tax row says of itself beyond its arithmetic, for the summary.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaxLabel {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub exemption_reason: Option<String>,
}

/// An item's own rate for a tax account, with its category and exemption reason.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LineRate {
    pub tax_account: String,
    pub rate: Decimal,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub exemption_reason: Option<String>,
}

/// What a document's taxes come to per tax row, category, rate and exemption reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    /// The tax row's place.
    pub tax: usize,
    pub description: Option<String>,
    pub account: Option<String>,
    pub category: String,
    pub rate: Decimal,
    pub exemption_reason: Option<String>,
    pub reverse_charge: bool,
    pub taxable_amount: Decimal,
    pub tax_amount: Decimal,
    pub base_taxable_amount: Decimal,
    pub base_tax_amount: Decimal,
}

/// The category a line falls in for a tax row: its item's for the row's account, else the row's
/// (when the item keeps the row's rate), else what the rate says.
fn category_of(label: &TaxLabel, row_rate: Decimal, reverse_charge: bool, item_rate: Option<&LineRate>, rate: Decimal) -> String {
    let rows_own = label.category.as_deref().filter(|_| item_rate.is_none_or(|r| r.rate == row_rate));
    match item_rate.and_then(|r| r.category.as_deref()).or(rows_own) {
        Some(c) => c.to_string(),
        None if reverse_charge => "Reverse Charge".into(),
        None if rate > Decimal::ZERO => "Standard".into(),
        None => "Zero Rated".into(),
    }
}

/// The tax summary (EN 16931's VAT breakdown, BG-23): per tax row, category, rate and exemption
/// reason, the taxable amount (the lines' net amounts, or the earlier row's amounts or totals the
/// row is charged on) and the tax, in the document's and the base currency. Fixed (Actual) charges
/// have no rate and are left out. `labels`: one per tax row; `line_rates`: each line's item rates.
pub fn tax_summary(input: &CalcInput, labels: &[TaxLabel], line_rates: &[Vec<LineRate>], out: &CalcOutput) -> Vec<Summary> {
    let signed = |q: usize, v: Decimal| {
        if input.taxes[q].reverse_charge {
            Decimal::ZERO
        } else if input.taxes[q].deduct {
            -v
        } else {
            v
        }
    };
    let mut groups: BTreeMap<(usize, String, Decimal, String), [Decimal; 4]> = BTreeMap::new();
    for (k, (label, tin)) in labels.iter().zip(&input.taxes).enumerate() {
        if tin.charge_type == ChargeType::Actual {
            continue;
        }
        for (i, (line, lin)) in out.lines.iter().zip(&input.lines).enumerate() {
            let rate = lin.tax_rates.get(&tin.key).copied().unwrap_or(tin.rate);
            let item_rate = line_rates.get(i).and_then(|rs| rs.iter().find(|r| label.account.as_deref() == Some(r.tax_account.as_str())));
            let reason = item_rate.and_then(|r| r.exemption_reason.as_deref()).or(label.exemption_reason.as_deref()).unwrap_or_default().to_string();
            let (taxable, base_taxable) = match (tin.charge_type, tin.row_ref) {
                (ChargeType::OnPreviousRowAmount, Some(j)) => (line.taxes[j], line.base_taxes[j]),
                (ChargeType::OnPreviousRowTotal, Some(j)) => {
                    (0..=j).fold((line.net_amount, line.base_net_amount), |(t, bt), q| (t + signed(q, line.taxes[q]), bt + signed(q, line.base_taxes[q])))
                }
                _ => (line.net_amount, line.base_net_amount),
            };
            let g = groups.entry((k, category_of(label, tin.rate, tin.reverse_charge, item_rate, rate), rate.normalize(), reason)).or_default();
            g[0] += taxable;
            g[1] += line.taxes[k];
            g[2] += base_taxable;
            g[3] += line.base_taxes[k];
        }
    }
    groups
        .into_iter()
        .map(|((k, category, rate, reason), [taxable, tax, base_taxable, base_tax])| Summary {
            tax: k,
            description: labels[k].description.clone(),
            account: labels[k].account.clone(),
            category,
            rate,
            exemption_reason: (!reason.is_empty()).then_some(reason),
            reverse_charge: input.taxes[k].reverse_charge,
            taxable_amount: taxable,
            tax_amount: tax,
            base_taxable_amount: base_taxable,
            base_tax_amount: base_tax,
        })
        .collect()
}

/// An item as a basket prices it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    #[serde(default)]
    pub item_group: Option<String>,
    #[serde(default)]
    pub stock_uom: Option<String>,
    #[serde(default)]
    pub units: Vec<Unit>,
    /// Its price per stock unit where the price list has none (zero: none).
    #[serde(default)]
    pub standard_rate: Decimal,
    /// A delivery charge (only free delivery takes it off).
    #[serde(default)]
    pub delivery: bool,
    /// Its own rates by tax account (as the document's countries leave them).
    #[serde(default)]
    pub tax_rates: Vec<LineRate>,
}

/// A tax row: its arithmetic (`ferrum-calc`'s) and what it says of itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tax {
    #[serde(flatten)]
    pub calc: ferrum_calc::TaxIn,
    #[serde(flatten)]
    pub label: TaxLabel,
}

/// Everything a basket is priced from, as a till keeps it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema: u32,
    /// The days (`YYYY-MM-DD`, in `time_zone`) it prices for, both included.
    pub valid_from: String,
    pub valid_until: String,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub currency: Option<String>,
    /// Decimal places of the currency and of the company's, how they round, the cash rounding of a
    /// total, how taxes round, the rate to the company's currency.
    pub precision: u32,
    #[serde(default)]
    pub base_precision: Option<u32>,
    #[serde(default = "one")]
    pub conversion_rate: Decimal,
    #[serde(default)]
    pub rounding: ferrum_calc::Rounding,
    #[serde(default)]
    pub round_total_to: Option<Decimal>,
    #[serde(default)]
    pub tax_rounding: ferrum_calc::TaxRounding,
    #[serde(default)]
    pub taxes: Vec<Tax>,
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(default)]
    pub prices: Vec<Price>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// In `ferrum-erp-promotions`' form, amounts in the snapshot's currency.
    #[serde(default)]
    pub promotions: Vec<promotions::Promotion>,
    /// Who buys when the basket names nobody, and their group.
    #[serde(default)]
    pub customer: Option<String>,
    #[serde(default)]
    pub customer_group: Option<String>,
    /// What promotions call where and how the sale is made.
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub place: Option<String>,
}

fn one() -> Decimal {
    Decimal::ONE
}

/// A line as the till rings it up: what a person typed (`price`, `discount_percent`) stands in for
/// the price list and the pricing rule as on the server.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BasketLine {
    pub item: String,
    pub qty: Decimal,
    #[serde(default)]
    pub uom: Option<String>,
    #[serde(default)]
    pub price: Option<Decimal>,
    #[serde(default)]
    pub discount_percent: Option<Decimal>,
}

/// A basket at a moment.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Basket {
    pub lines: Vec<BasketLine>,
    /// `YYYY-MM-DD HH:MM` in the snapshot's time zone.
    pub at: String,
    /// None: the snapshot's.
    #[serde(default)]
    pub customer: Option<String>,
    #[serde(default)]
    pub customer_group: Option<String>,
    /// The document's own discount.
    #[serde(default)]
    pub discount_percent: Decimal,
}

/// A line as priced: what it was priced from, then (in [`Priced::totals`]) what it came to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricedLine {
    /// Its place in [`Priced::lines`]: the basket's lines, then the free items promotions added.
    pub key: String,
    pub item: String,
    pub qty: Decimal,
    pub uom: Option<String>,
    pub conversion_factor: Decimal,
    pub stock_qty: Decimal,
    pub price_list_rate: Decimal,
    /// Its own (typed or a pricing rule's).
    pub discount_percent: Decimal,
    /// The share promotions took of what its own discount left.
    pub promotion_percent: Decimal,
    pub added_by_promotion: Option<String>,
    pub tax_rates: BTreeMap<String, Decimal>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Priced {
    pub schema: u32,
    pub lines: Vec<PricedLine>,
    /// Each promotion's part of each line (`line`: the line's key).
    pub applied: Vec<promotions::Applied>,
    /// Rates, amounts, taxes and totals, as `ferrum-calc` gives them.
    pub totals: CalcOutput,
    pub tax_summary: Vec<Summary>,
}

/// The ISO weekday (1 Monday to 7) of a `YYYY-MM-DD` date.
pub fn iso_weekday(date: &str) -> Option<u8> {
    let mut p = date.get(..10)?.split('-');
    let (y, m, d): (i64, i64, i64) = (p.next()?.parse().ok()?, p.next()?.parse().ok()?, p.next()?.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Days from 1970-01-01 (a Thursday), by Howard Hinnant's civil calendar.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(((days + 3).rem_euclid(7) + 1) as u8)
}

struct Ring {
    item: String,
    qty: Decimal,
    uom: Option<String>,
    price: Option<Decimal>,
    discount: Option<Decimal>,
    added_by: Option<String>,
}

/// One line priced as the server prices a line of a draft sale: its unit, its price (typed, else the
/// price list's, else the item's standard rate, in the snapshot's currency), its pricing rule when
/// no discount was typed (a rule's rate stands in for any price).
fn price_line(s: &Snapshot, r: &Ring, key: usize, on: &str, customer: Option<&str>, group: Option<&str>) -> Result<PricedLine, String> {
    let item = s.items.iter().find(|i| i.id == r.item).ok_or_else(|| format!("item {} is not in the snapshot", r.item))?;
    let uom = r.uom.clone().or(item.stock_uom.clone());
    let factor = unit_factor(item.stock_uom.as_deref(), &item.units, uom.as_deref())
        .ok_or_else(|| format!("item {} has no conversion from this unit to its stock unit", item.id))?;
    let mut price = r.price.filter(|p| !p.is_zero());
    if price.is_none() {
        let found = list_price(&s.prices, &item.id, on, uom.as_deref(), item.stock_uom.as_deref(), factor)
            .or_else(|| (!item.standard_rate.is_zero()).then(|| (item.standard_rate * factor).round_dp(9)));
        price = found.map(|p| if s.conversion_rate == Decimal::ONE { p } else { (p / s.conversion_rate).round_dp(9) });
    }
    let mut price = price.unwrap_or_default();
    let mut discount = r.discount.unwrap_or_default();
    let stock_qty = to_stock(r.qty, factor);
    if r.discount.is_none() {
        let ask = RuleAsk { item: Some(&item.id), item_group: item.item_group.as_deref(), qty: stock_qty, customer, customer_group: group, on };
        if let Some(rule) = best_rule(&s.rules, ask, |_| true) {
            if rule.rate > Decimal::ZERO {
                price = (rule.rate * factor).round_dp(9);
            }
            if rule.discount_percent > Decimal::ZERO {
                discount = rule.discount_percent;
            }
        }
    }
    let by_account: BTreeMap<String, Decimal> = item.tax_rates.iter().map(|t| (t.tax_account.clone(), t.rate)).collect();
    let rows: Vec<(String, Option<String>)> = s.taxes.iter().map(|t| (t.calc.key.clone(), t.label.account.clone())).collect();
    Ok(PricedLine {
        key: key.to_string(),
        item: item.id.clone(),
        qty: r.qty,
        uom,
        conversion_factor: factor,
        stock_qty,
        price_list_rate: price,
        discount_percent: discount,
        promotion_percent: Decimal::ZERO,
        added_by_promotion: r.added_by.clone(),
        tax_rates: line_tax_rates(&by_account, &rows),
    })
}

/// How often free items may change the basket before it is priced: each round adds a promotion's
/// item or drops one, and a promotion adds its item once.
const ROUNDS: usize = 8;

/// `basket` priced with `snapshot`, as ERP prices a draft sale of its own with the same data: each
/// line's price and rule, the promotions over the lines (a free item they add is a line priced like
/// any, and the basket is priced again), each line's own discount then the promotions' share of what
/// it leaves, the taxes, the document's discount, the rounding and the summary. Promotions needing a
/// code or a first order are not in a snapshot: they are the server's.
pub fn price_basket(s: &Snapshot, basket: &Basket) -> Result<Priced, String> {
    if s.schema != SCHEMA {
        return Err(format!("a snapshot of schema {} cannot be read by schema {SCHEMA}", s.schema));
    }
    let (on, time) = basket.at.split_once([' ', 'T']).unwrap_or((basket.at.as_str(), ""));
    let weekday = iso_weekday(on).ok_or("at is a moment as YYYY-MM-DD HH:MM")?;
    if on < s.valid_from.as_str() || on > s.valid_until.as_str() {
        return Err(format!("the snapshot prices from {} to {}, not on {on}", s.valid_from, s.valid_until));
    }
    let (customer, group) = match &basket.customer {
        Some(c) => (Some(c.as_str()), basket.customer_group.as_deref()),
        None => (s.customer.as_deref(), s.customer_group.as_deref()),
    };
    let when = promotions::When {
        date: on.to_string(),
        weekday,
        time: time.get(..5).unwrap_or(time).to_string(),
        customer: customer.map(str::to_string),
        customer_group: group.map(str::to_string),
        channel: s.channel.clone(),
        place: s.place.clone(),
        first_order: false,
        codes: vec![],
    };
    let mut ring: Vec<Ring> = basket
        .lines
        .iter()
        .map(|l| Ring { item: l.item.clone(), qty: l.qty, uom: l.uom.clone(), price: l.price, discount: l.discount_percent, added_by: None })
        .collect();
    let document = CalcInput {
        precision: s.precision,
        base_precision: s.base_precision,
        conversion_rate: s.conversion_rate,
        rounding: s.rounding,
        lines: vec![],
        taxes: s.taxes.iter().map(|t| t.calc.clone()).collect(),
        discount: (basket.discount_percent > Decimal::ZERO).then_some(DocDiscount::Percent { value: basket.discount_percent }),
        round_total_to: s.round_total_to,
        tax_rounding: s.tax_rounding,
    };
    let input_of = |lines: &[PricedLine]| CalcInput {
        lines: lines
            .iter()
            .map(|l| LineIn {
                key: l.key.clone(),
                qty: l.qty,
                price: l.price_list_rate,
                discount_percent: promotions::combined_percent(l.discount_percent, l.promotion_percent),
                tax_rates: l.tax_rates.clone(),
                tax_qty: Some(l.stock_qty),
            })
            .collect(),
        ..document.clone()
    };
    let mut applied = Vec::new();
    let mut rounds = 0;
    let lines = loop {
        let mut lines = ring.iter().enumerate().map(|(i, r)| price_line(s, r, i, on, customer, group)).collect::<Result<Vec<_>, _>>()?;
        if s.promotions.is_empty() {
            break lines;
        }
        let amounts = before_promotions(&input_of(&lines))?;
        let engine_lines = lines
            .iter()
            .zip(&amounts)
            .map(|(l, amount)| {
                let item = s.items.iter().find(|i| i.id == l.item);
                promotions::Line {
                    key: l.key.clone(),
                    item: Some(l.item.clone()),
                    item_group: item.and_then(|i| i.item_group.clone()),
                    qty: l.qty,
                    amount: *amount,
                    line_discount: l.discount_percent > Decimal::ZERO,
                    delivery: item.is_some_and(|i| i.delivery),
                    added_by: l.added_by_promotion.clone(),
                }
            })
            .collect();
        let input =
            promotions::Input { lines: engine_lines, promotions: s.promotions.clone(), when: when.clone(), precision: s.precision, rounding: s.rounding };
        let out = promotions::apply(&input)?;
        let count = ring.len();
        ring.retain(|r| r.added_by.as_ref().is_none_or(|p| out.fired.contains(p)));
        let changed = ring.len() != count || !out.add.is_empty();
        for add in &out.add {
            ring.push(Ring { item: add.item.clone(), qty: add.qty, uom: None, price: None, discount: None, added_by: Some(add.promotion.clone()) });
        }
        rounds += 1;
        if changed && rounds < ROUNDS {
            continue;
        }
        for (l, o) in lines.iter_mut().zip(&out.lines) {
            l.promotion_percent = o.percent.normalize();
        }
        applied = out.applied;
        break lines;
    };
    let input = input_of(&lines);
    let totals = compute(&input).map_err(|e| e.to_string())?;
    let line_rates: Vec<Vec<LineRate>> =
        lines.iter().map(|l| s.items.iter().find(|i| i.id == l.item).map(|i| i.tax_rates.clone()).unwrap_or_default()).collect();
    let labels: Vec<TaxLabel> = s.taxes.iter().map(|t| t.label.clone()).collect();
    let tax_summary = tax_summary(&input, &labels, &line_rates, &totals);
    Ok(Priced { schema: SCHEMA, lines, applied, totals, tax_summary })
}

/// What [`price_json`] reads: a snapshot and a basket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub snapshot: Snapshot,
    pub basket: Basket,
}

/// JSON in ([`Request`]), JSON out ([`Priced`], or `{"error": "..."}`).
pub fn price_json(input: &str) -> String {
    let result = serde_json::from_str::<Request>(input).map_err(|e| e.to_string()).and_then(|r| price_basket(&r.snapshot, &r.basket));
    match result {
        Ok(out) => serde_json::to_string(&out).unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }).to_string()),
        Err(e) => serde_json::json!({ "error": e }).to_string(),
    }
}

#[cfg(feature = "wasm")]
mod wasm {
    use wasm_bindgen::prelude::*;

    /// Browser entry point: the server's basket pricing, the same bytes out.
    #[wasm_bindgen(js_name = priceBasket)]
    pub fn price_basket(input: &str) -> String {
        super::price_json(input)
    }
}

#[cfg(test)]
mod tests;
