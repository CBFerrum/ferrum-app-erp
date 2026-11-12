//! Promotions that look at a whole document: when one applies (dates, weekdays and hours, the
//! customer, the channel, the place, a code, a first order, a minimum spend or quantity), what it
//! gives (off the document, off the lines that qualify, buy some get some, a bundle price, free
//! delivery, a free item) and how promotions combine.
//!
//! Pure and deterministic: the same lines, promotions and moment in, the same result out; no clock
//! and no database. The server gathers the promotions that could apply and the till gets the same
//! list, so both price alike (feature `wasm` for the browser, beside `ferrum-calc`).
//!
//! Every discount is taken off line amounts as the document shows them (after each line's own
//! discount, tax-inclusive where its prices include tax), in exact decimals: a discount on several
//! lines is spread by largest remainder over what is left of them, so the parts add up to the whole.
//! What a line keeps is its [`LineOut::percent`], an exact share: priced with it (see
//! [`combined_percent`]) the line's amount is what is left, its taxes are worked out on that, and a
//! return of part of the line gives back the same share.

use ferrum_calc::{Rounding, allocate};
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};

/// A line of the document, as priced before promotions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Line {
    pub key: String,
    #[serde(default)]
    pub item: Option<String>,
    #[serde(default)]
    pub item_group: Option<String>,
    pub qty: Decimal,
    /// The line's amount before promotions, at the document's precision.
    pub amount: Decimal,
    /// The line has a discount of its own (a pricing rule's, or typed).
    #[serde(default)]
    pub line_discount: bool,
    /// The line is a delivery charge.
    #[serde(default)]
    pub delivery: bool,
    /// The promotion that added this line (a free item): it takes no other promotion.
    #[serde(default)]
    pub added_by: Option<String>,
}

/// Items, item groups, or (both empty) every line.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Set {
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(default)]
    pub item_groups: Vec<String>,
}

impl Set {
    fn holds(&self, l: &Line) -> bool {
        (self.items.is_empty() && self.item_groups.is_empty())
            || l.item.as_ref().is_some_and(|i| self.items.contains(i))
            || l.item_group.as_ref().is_some_and(|g| self.item_groups.contains(g))
    }
}

/// What the units a buy-and-get promotion gives cost.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "as", rename_all = "snake_case")]
pub enum GetAt {
    Free,
    PercentOff { percent: Decimal },
    Price { price: Decimal },
}

/// What a promotion gives. Amounts and prices are in the document's currency.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Gives {
    /// Off every line but delivery charges.
    PercentOffDocument {
        percent: Decimal,
    },
    AmountOffDocument {
        amount: Decimal,
    },
    /// Off the qualifying lines only.
    PercentOffLines {
        percent: Decimal,
    },
    AmountOffLines {
        amount: Decimal,
    },
    /// For each `buy` units of the qualifying set, `get` units of the `get_set` (the qualifying set
    /// when empty) at `at`, cheapest first unless `cheapest_first` is false.
    BuyGet {
        buy: Decimal,
        get: Decimal,
        #[serde(default)]
        get_set: Set,
        at: GetAt,
        #[serde(default = "yes")]
        cheapest_first: bool,
    },
    /// Each complete set of `parts` (item, units) for `price`.
    Bundle {
        parts: Vec<(String, Decimal)>,
        price: Decimal,
    },
    /// The delivery charge lines, whole.
    FreeDelivery,
    /// `qty` of `item` added as a line, free.
    FreeItem {
        item: String,
        qty: Decimal,
    },
}

fn yes() -> bool {
    true
}

/// A promotion: when it applies, what it gives and how it combines. Empty lists mean anyone,
/// any day, anywhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Promotion {
    pub id: String,
    #[serde(default)]
    pub title: String,
    /// Higher goes first; equal ones in the order of their ids.
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub valid_upto: Option<String>,
    /// ISO weekdays, 1 (Monday) to 7.
    #[serde(default)]
    pub weekdays: Vec<u8>,
    /// `HH:MM`; a window whose end comes before its start runs past midnight.
    #[serde(default)]
    pub from_time: Option<String>,
    #[serde(default)]
    pub to_time: Option<String>,
    #[serde(default)]
    pub customers: Vec<String>,
    #[serde(default)]
    pub customer_groups: Vec<String>,
    #[serde(default)]
    pub channels: Vec<String>,
    #[serde(default)]
    pub places: Vec<String>,
    #[serde(default)]
    pub requires_code: bool,
    #[serde(default)]
    pub first_order_only: bool,
    /// Spend over the qualifying lines it takes (0: none).
    #[serde(default)]
    pub min_amount: Decimal,
    #[serde(default)]
    pub min_qty: Decimal,
    /// The lines that count towards the minimums, that "off the lines" discounts, and that buying
    /// means.
    #[serde(default)]
    pub qualifying: Set,
    pub gives: Gives,
    /// May apply with others. One that may not applies only first, and then alone.
    #[serde(default = "yes")]
    pub combinable: bool,
    /// May discount lines that have a discount of their own.
    #[serde(default = "yes")]
    pub with_line_discounts: bool,
    /// The most it takes off a document (none when unset).
    #[serde(default)]
    pub max_discount: Option<Decimal>,
}

/// A code the customer gave, already checked by the caller, for the promotion it opens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Code {
    pub promotion: String,
    pub code: String,
}

/// Who buys, where, how and when (in the company's time zone).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct When {
    /// `YYYY-MM-DD`.
    pub date: String,
    /// ISO weekday of `date`, 1 (Monday) to 7.
    pub weekday: u8,
    /// `HH:MM` (more is ignored).
    pub time: String,
    #[serde(default)]
    pub customer: Option<String>,
    #[serde(default)]
    pub customer_group: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub place: Option<String>,
    #[serde(default)]
    pub first_order: bool,
    #[serde(default)]
    pub codes: Vec<Code>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Input {
    pub lines: Vec<Line>,
    pub promotions: Vec<Promotion>,
    pub when: When,
    /// Decimal places of the document's currency.
    pub precision: u32,
    #[serde(default)]
    pub rounding: Rounding,
}

/// What promotions took off a line, and the exact share of its amount that is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LineOut {
    pub key: String,
    pub discount: Decimal,
    pub percent: Decimal,
}

/// One promotion's part of one line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub promotion: String,
    pub title: String,
    pub line: String,
    pub amount: Decimal,
    pub code: Option<String>,
}

/// A free item a promotion adds, not on the document yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Add {
    pub promotion: String,
    pub item: String,
    pub qty: Decimal,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub lines: Vec<LineOut>,
    /// The promotions that applied, in the order they did (one adding a free item is among them
    /// even before its line is there).
    pub fired: Vec<String>,
    pub applied: Vec<Applied>,
    pub add: Vec<Add>,
}

fn round(v: Decimal, dp: u32, r: Rounding) -> Decimal {
    let strategy = match r {
        Rounding::HalfUp => RoundingStrategy::MidpointAwayFromZero,
        Rounding::HalfEven => RoundingStrategy::MidpointNearestEven,
    };
    let mut x = v.round_dp_with_strategy(dp, strategy);
    x.rescale(dp);
    x
}

fn hhmm(t: &str) -> &str {
    t.get(..5).unwrap_or(t)
}

fn admits(p: &Promotion, w: &When) -> bool {
    let within_time = match (p.from_time.as_deref().map(hhmm), p.to_time.as_deref().map(hhmm)) {
        (Some(f), Some(t)) if t < f => hhmm(&w.time) >= f || hhmm(&w.time) < t,
        (f, t) => f.is_none_or(|f| hhmm(&w.time) >= f) && t.is_none_or(|t| hhmm(&w.time) < t),
    };
    let anyone = p.customers.is_empty() && p.customer_groups.is_empty();
    let party =
        anyone || w.customer.as_ref().is_some_and(|c| p.customers.contains(c)) || w.customer_group.as_ref().is_some_and(|g| p.customer_groups.contains(g));
    p.valid_from.as_deref().is_none_or(|f| f <= w.date.as_str())
        && p.valid_upto.as_deref().is_none_or(|u| u >= w.date.as_str())
        && (p.weekdays.is_empty() || p.weekdays.contains(&w.weekday))
        && within_time
        && party
        && (p.channels.is_empty() || w.channel.as_ref().is_some_and(|c| p.channels.iter().any(|x| x.eq_ignore_ascii_case(c))))
        && (p.places.is_empty() || w.place.as_ref().is_some_and(|x| p.places.contains(x)))
        && (!p.requires_code || w.codes.iter().any(|c| c.promotion == p.id))
        && (!p.first_order_only || w.first_order)
}

/// Units of `qty` a promotion counts: whole ones.
fn units(qty: Decimal) -> Decimal {
    qty.max(Decimal::ZERO).floor()
}

/// Spreads `total` over `idx` by `weights` (largest remainder), into `out`.
fn spread(out: &mut [Decimal], idx: &[usize], weights: &[Decimal], total: Decimal, dp: u32, r: Rounding) -> Result<(), String> {
    for (i, part) in idx.iter().zip(allocate(total, weights, dp, r).map_err(|e| e.to_string())?) {
        out[*i] += part;
    }
    Ok(())
}

/// Takes `want` units from `lines` in order: (line, units taken).
fn take(lines: &[usize], qty: &[Decimal], mut want: Decimal) -> Vec<(usize, Decimal)> {
    let mut out = Vec::new();
    for &i in lines {
        if want <= Decimal::ZERO {
            break;
        }
        let u = units(qty[i]).min(want);
        if u > Decimal::ZERO {
            out.push((i, u));
            want -= u;
        }
    }
    out
}

/// What each line gets from one promotion, from what is `left` of the lines.
fn gives(p: &Promotion, lines: &[Line], left: &[Decimal], open: &[usize], dp: u32, r: Rounding) -> Result<Vec<Decimal>, String> {
    let n = lines.len();
    let mut d = vec![Decimal::ZERO; n];
    let goods: Vec<usize> = open.iter().copied().filter(|&i| !lines[i].delivery).collect();
    let qualifying: Vec<usize> = goods.iter().copied().filter(|&i| p.qualifying.holds(&lines[i])).collect();
    let sum = |idx: &[usize]| idx.iter().map(|&i| left[i]).sum::<Decimal>();
    let weights = |idx: &[usize]| idx.iter().map(|&i| left[i]).collect::<Vec<_>>();
    let qty: Vec<Decimal> = lines.iter().map(|l| l.qty).collect();
    let hundred = Decimal::ONE_HUNDRED;
    match &p.gives {
        Gives::PercentOffDocument { percent } | Gives::PercentOffLines { percent } => {
            let on = if matches!(p.gives, Gives::PercentOffDocument { .. }) { &goods } else { &qualifying };
            let total = round(sum(on) * (*percent).clamp(Decimal::ZERO, hundred) / hundred, dp, r);
            spread(&mut d, on, &weights(on), total, dp, r)?;
        }
        Gives::AmountOffDocument { amount } | Gives::AmountOffLines { amount } => {
            let on = if matches!(p.gives, Gives::AmountOffDocument { .. }) { &goods } else { &qualifying };
            let total = round(*amount, dp, r).max(Decimal::ZERO).min(sum(on));
            spread(&mut d, on, &weights(on), total, dp, r)?;
        }
        Gives::BuyGet { buy, get, get_set, at, cheapest_first } => {
            if *buy <= Decimal::ZERO || *get <= Decimal::ZERO {
                return Ok(d);
            }
            let get_set = if get_set.items.is_empty() && get_set.item_groups.is_empty() { &p.qualifying } else { get_set };
            let mut gets: Vec<usize> = goods.iter().copied().filter(|&i| get_set.holds(&lines[i])).collect();
            let count = |idx: &[usize]| idx.iter().map(|&i| units(qty[i])).sum::<Decimal>();
            let overlap = qualifying.iter().any(|i| gets.contains(i));
            let times = if overlap {
                let both: Vec<usize> = goods.iter().copied().filter(|i| qualifying.contains(i) || gets.contains(i)).collect();
                (count(&both) / (buy + get)).floor()
            } else {
                (count(&qualifying) / buy).floor()
            };
            // Unit prices, cheapest (or dearest) first; equal ones in line order.
            let unit = |i: usize| if qty[i].is_zero() { Decimal::ZERO } else { left[i] / qty[i] };
            gets.sort_by(|&a, &b| if *cheapest_first { unit(a).cmp(&unit(b)) } else { unit(b).cmp(&unit(a)) }.then(a.cmp(&b)));
            for (i, u) in take(&gets, &qty, times * get) {
                let value = left[i] * u / qty[i];
                let off = match at {
                    GetAt::Free => value,
                    GetAt::PercentOff { percent } => value * (*percent).clamp(Decimal::ZERO, hundred) / hundred,
                    GetAt::Price { price } => (value - price * u).max(Decimal::ZERO),
                };
                d[i] = round(off, dp, r).min(left[i]);
            }
        }
        Gives::Bundle { parts, price } => {
            let of = |item: &str| goods.iter().copied().filter(|&i| lines[i].item.as_deref() == Some(item)).collect::<Vec<_>>();
            let times = parts
                .iter()
                .map(|(item, per)| if *per > Decimal::ZERO { (of(item).iter().map(|&i| units(qty[i])).sum::<Decimal>() / per).floor() } else { Decimal::ZERO })
                .min()
                .unwrap_or_default();
            if times > Decimal::ZERO {
                let (mut idx, mut portions) = (Vec::new(), Vec::new());
                for (item, per) in parts {
                    for (i, u) in take(&of(item), &qty, times * per) {
                        idx.push(i);
                        portions.push(left[i] * u / qty[i]);
                    }
                }
                let value: Decimal = portions.iter().sum();
                let total = round((value - times * price).max(Decimal::ZERO), dp, r).min(sum(&idx));
                // A line can be in more than one part: spread over each portion, then add up.
                spread(&mut d, &idx, &portions, total, dp, r)?;
            }
        }
        Gives::FreeDelivery => {
            for &i in open.iter().filter(|&&i| lines[i].delivery) {
                d[i] = left[i];
            }
        }
        Gives::FreeItem { .. } => {
            for (i, l) in lines.iter().enumerate() {
                if l.added_by.as_deref() == Some(p.id.as_str()) {
                    d[i] = left[i];
                }
            }
        }
    }
    Ok(d)
}

/// Applies `promotions` to `lines`. The order, and how they combine:
/// 1. Each line's own discount (pricing rules) is already in its amount.
/// 2. Promotions that apply go by priority, highest first (then by id), each on what earlier ones
///    left of the lines.
/// 3. One that may not combine applies only if none has yet, and then none after it does.
/// 4. One that leaves lines with their own discount alone neither counts nor discounts them.
/// 5. Each one's discount is capped at its `max_discount`, spread like its discount was.
/// 6. Delivery charges count for nothing but free delivery, and lines a promotion added for
///    nothing but that promotion.
pub fn apply(input: &Input) -> Result<Outcome, String> {
    let (lines, dp, r) = (&input.lines, input.precision, input.rounding);
    let mut left: Vec<Decimal> = lines.iter().map(|l| l.amount).collect();
    let mut order: Vec<&Promotion> = input.promotions.iter().collect();
    order.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.id.cmp(&b.id)));
    let mut out = Outcome::default();
    let mut applied_any = false;
    for p in order {
        if applied_any && !p.combinable {
            continue;
        }
        if !admits(p, &input.when) {
            continue;
        }
        let open: Vec<usize> = (0..lines.len())
            .filter(|&i| lines[i].added_by.is_none() && left[i] > Decimal::ZERO && (p.with_line_discounts || !lines[i].line_discount))
            .collect();
        let qualifying: Vec<usize> = open.iter().copied().filter(|&i| !lines[i].delivery && p.qualifying.holds(&lines[i])).collect();
        let spend: Decimal = qualifying.iter().map(|&i| left[i]).sum();
        let qty: Decimal = qualifying.iter().map(|&i| lines[i].qty).sum();
        if (p.min_amount > Decimal::ZERO && spend < p.min_amount) || (p.min_qty > Decimal::ZERO && qty < p.min_qty) {
            continue;
        }
        let mut d = gives(p, lines, &left, &open, dp, r)?;
        let total: Decimal = d.iter().sum();
        if let Some(cap) = p.max_discount.filter(|c| *c >= Decimal::ZERO && total > *c) {
            let idx: Vec<usize> = (0..d.len()).filter(|&i| !d[i].is_zero()).collect();
            let weights: Vec<Decimal> = idx.iter().map(|&i| d[i]).collect();
            d = vec![Decimal::ZERO; d.len()];
            spread(&mut d, &idx, &weights, round(cap, dp, r), dp, r)?;
        }
        let adds = match &p.gives {
            Gives::FreeItem { item, qty } if !lines.iter().any(|l| l.added_by.as_deref() == Some(p.id.as_str())) => {
                vec![Add { promotion: p.id.clone(), item: item.clone(), qty: *qty }]
            }
            _ => vec![],
        };
        if d.iter().all(Decimal::is_zero) && !matches!(p.gives, Gives::FreeItem { .. }) {
            continue;
        }
        let code = input.when.codes.iter().find(|c| c.promotion == p.id).map(|c| c.code.clone()).filter(|_| p.requires_code);
        for (i, part) in d.iter().enumerate().filter(|(_, x)| !x.is_zero()) {
            left[i] -= part;
            out.applied.push(Applied { promotion: p.id.clone(), title: p.title.clone(), line: lines[i].key.clone(), amount: *part, code: code.clone() });
        }
        out.add.extend(adds);
        out.fired.push(p.id.clone());
        applied_any = true;
        if !p.combinable {
            break;
        }
    }
    out.lines = lines
        .iter()
        .zip(&left)
        .map(|(l, rest)| {
            let discount = l.amount - rest;
            let percent = if l.amount.is_zero() || discount.is_zero() { Decimal::ZERO } else { discount * Decimal::ONE_HUNDRED / l.amount };
            LineOut { key: l.key.clone(), discount, percent }
        })
        .collect();
    Ok(out)
}

/// The one discount a line is priced with: its own `line_discount` percent, then promotions'
/// `promotion_percent` of what that leaves. With no promotion it is the line's own, unchanged.
pub fn combined_percent(line_discount: Decimal, promotion_percent: Decimal) -> Decimal {
    if promotion_percent.is_zero() {
        return line_discount;
    }
    let hundred = Decimal::ONE_HUNDRED;
    hundred - (hundred - line_discount) * (hundred - promotion_percent) / hundred
}

/// JSON in ([`Input`]), JSON out ([`Outcome`], or `{"error": "..."}`).
pub fn apply_json(input: &str) -> String {
    let result = serde_json::from_str::<Input>(input).map_err(|e| e.to_string()).and_then(|i| apply(&i));
    match result {
        Ok(out) => serde_json::to_string(&out).unwrap_or_else(|e| serde_json::json!({ "error": e.to_string() }).to_string()),
        Err(e) => serde_json::json!({ "error": e }).to_string(),
    }
}

#[cfg(feature = "wasm")]
mod wasm {
    use wasm_bindgen::prelude::*;

    /// Browser entry point: the server's promotions, the same results.
    #[wasm_bindgen(js_name = applyPromotions)]
    pub fn apply_promotions(input: &str) -> String {
        super::apply_json(input)
    }
}

#[cfg(test)]
mod tests;
