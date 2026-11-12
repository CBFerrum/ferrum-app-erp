use super::*;
use ferrum_calc::{CalcInput, ChargeType, LineIn, TaxIn, compute};
use std::str::FromStr;

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn line(key: &str, item: &str, qty: &str, amount: &str) -> Line {
    Line { key: key.into(), item: Some(item.into()), item_group: Some("goods".into()), qty: n(qty), amount: n(amount), ..Default::default() }
}

fn promo(id: &str, gives: Gives) -> Promotion {
    serde_json::from_value(serde_json::json!({"id": id, "title": id, "gives": gives})).unwrap()
}

fn when() -> When {
    When { date: "2026-11-12".into(), weekday: 4, time: "10:30".into(), ..Default::default() }
}

fn run(lines: Vec<Line>, promotions: Vec<Promotion>, w: When) -> Outcome {
    apply(&Input { lines, promotions, when: w, precision: 2, rounding: Rounding::HalfUp }).unwrap()
}

fn discounts(o: &Outcome) -> Vec<Decimal> {
    o.lines.iter().map(|l| l.discount).collect()
}

/// The engine's input for lines priced at `price` each with the promotions' share, and a tax.
fn priced(lines: &[(&str, &str, Decimal)], tax: &str, inclusive: bool) -> CalcInput {
    CalcInput {
        lines: lines
            .iter()
            .map(|(qty, price, pp)| LineIn {
                key: format!("{qty}@{price}"),
                qty: n(qty),
                price: n(price),
                discount_percent: combined_percent(Decimal::ZERO, *pp),
                ..Default::default()
            })
            .collect(),
        taxes: vec![TaxIn { key: "vat".into(), charge_type: ChargeType::OnNetTotal, rate: n(tax), included_in_price: inclusive, ..Default::default() }],
        ..Default::default()
    }
}

/// An amount off the document is spread over its lines by largest remainder; priced with each
/// line's share, the lines come to exactly what is left, tax is on those amounts, and a return of
/// half of each line gives back half.
#[test]
fn an_amount_off_the_document_is_spread_and_taxed_per_line_exclusive() {
    let lines = vec![line("a", "A", "2", "30.00"), line("b", "B", "2", "45.00"), line("c", "C", "2", "25.00")];
    let o = run(lines, vec![promo("p", Gives::AmountOffDocument { amount: n("10") })], when());
    assert_eq!(discounts(&o), [n("3.00"), n("4.50"), n("2.50")]);
    assert_eq!(o.applied.iter().map(|a| a.amount).sum::<Decimal>(), n("10"));
    let pp: Vec<Decimal> = o.lines.iter().map(|l| l.percent).collect();
    let sale = compute(&priced(&[("2", "15", pp[0]), ("2", "22.5", pp[1]), ("2", "12.5", pp[2])], "10", false)).unwrap();
    assert_eq!(sale.lines.iter().map(|l| l.amount).collect::<Vec<_>>(), [n("27.00"), n("40.50"), n("22.50")]);
    assert_eq!((sale.net_total, sale.total_taxes, sale.grand_total), (n("90.00"), n("9.00"), n("99.00")));
    // Half of every line back: half the net, half the tax, half the total.
    let back = compute(&priced(&[("-1", "15", pp[0]), ("-1", "22.5", pp[1]), ("-1", "12.5", pp[2])], "10", false)).unwrap();
    assert_eq!((back.net_total, back.total_taxes, back.grand_total), (n("-45.00"), n("-4.50"), n("-49.50")));
}

/// With prices that include tax, the discount comes off what the customer sees, the tax is
/// backed out of what is left of each line, and the total is what is left.
#[test]
fn an_amount_off_the_document_with_tax_included() {
    let lines = vec![line("a", "A", "1", "110.00"), line("b", "B", "1", "55.00")];
    let o = run(lines, vec![promo("p", Gives::AmountOffDocument { amount: n("16.50") })], when());
    assert_eq!(discounts(&o), [n("11.00"), n("5.50")]);
    let pp: Vec<Decimal> = o.lines.iter().map(|l| l.percent).collect();
    let sale = compute(&priced(&[("1", "110", pp[0]), ("1", "55", pp[1])], "10", true)).unwrap();
    assert_eq!(sale.grand_total, n("148.50"));
    assert_eq!((sale.net_total, sale.total_taxes), (n("135.00"), n("13.50")));
    assert_eq!(sale.lines.iter().map(|l| l.net_amount).collect::<Vec<_>>(), [n("90.00"), n("45.00")]);
}

/// Parts that do not divide evenly still add up to the whole, and each line priced with its share
/// comes to exactly what is left of it, whatever the price and quantity.
#[test]
fn shares_price_back_to_the_cent() {
    let lines = vec![line("a", "A", "3", "10.00"), line("b", "B", "7", "10.00"), line("c", "C", "1", "10.00")];
    let o = run(lines, vec![promo("p", Gives::AmountOffDocument { amount: n("10") })], when());
    assert_eq!(discounts(&o), [n("3.34"), n("3.33"), n("3.33")], "largest remainder, ties to the earlier line");
    for qty in ["1", "3", "7", "13", "999"] {
        for price in ["0.01", "0.99", "3.33", "17.5", "1234.567", "99999.99"] {
            for own in ["0", "12.5", "33.333"] {
                let raw = n(qty) * n(price) * (Decimal::ONE_HUNDRED - n(own)) / Decimal::ONE_HUNDRED;
                let amount = round(raw, 2, Rounding::HalfUp);
                for off in ["0.01", "1", "7.77"] {
                    let off = n(off).min(amount);
                    let pp = off * Decimal::ONE_HUNDRED / amount;
                    let li = LineIn { key: "x".into(), qty: n(qty), price: n(price), discount_percent: combined_percent(n(own), pp), ..Default::default() };
                    let out = compute(&CalcInput { lines: vec![li], ..Default::default() }).unwrap();
                    assert_eq!(out.lines[0].amount, amount - off, "{qty} x {price} less {own}% less {off}");
                }
            }
        }
    }
    assert_eq!(combined_percent(n("12.5"), Decimal::ZERO), n("12.5"), "no promotion: the line's own discount, unchanged");
}

#[test]
fn percent_off_qualifying_lines_only_and_a_minimum_spend() {
    let mut p = promo("p", Gives::PercentOffLines { percent: n("10") });
    p.qualifying = Set { items: vec!["A".into()], item_groups: vec![] };
    p.min_amount = n("50");
    let lines = || vec![line("a", "A", "2", "40.00"), line("b", "B", "1", "100.00")];
    assert!(run(lines(), vec![p.clone()], when()).applied.is_empty(), "40 of A is under the 50 to spend on it");
    p.min_amount = n("40");
    assert_eq!(discounts(&run(lines(), vec![p], when())), [n("4.00"), n("0")]);
}

#[test]
fn buy_two_get_one_cheapest_free_and_a_choice_of_dearest() {
    let mut p = promo("p", Gives::BuyGet { buy: n("2"), get: n("1"), get_set: Set::default(), at: GetAt::Free, cheapest_first: true });
    let lines = || vec![line("a", "A", "2", "20.00"), line("b", "B", "1", "5.00"), line("c", "C", "3", "30.00")];
    // Six units in one set: two free, the cheapest (B at 5, then one of A at 10).
    assert_eq!(discounts(&run(lines(), vec![p.clone()], when())), [n("10.00"), n("5.00"), n("0")]);
    p.gives = Gives::BuyGet { buy: n("2"), get: n("1"), get_set: Set::default(), at: GetAt::PercentOff { percent: n("50") }, cheapest_first: false };
    // The dearest two units at half price: A and C cost 10 each, so A's two, in line order.
    assert_eq!(discounts(&run(lines(), vec![p.clone()], when())), [n("10.00"), n("0"), n("0")]);
    // Buy from one set, get from another.
    p.qualifying = Set { items: vec!["C".into()], item_groups: vec![] };
    p.gives = Gives::BuyGet {
        buy: n("3"),
        get: n("2"),
        get_set: Set { items: vec!["A".into()], item_groups: vec![] },
        at: GetAt::Price { price: n("1") },
        cheapest_first: true,
    };
    assert_eq!(discounts(&run(lines(), vec![p], when())), [n("18.00"), n("0"), n("0")]);
}

#[test]
fn a_bundle_price_free_delivery_and_a_free_item() {
    let bundle = promo("bundle", Gives::Bundle { parts: vec![("A".into(), n("1")), ("B".into(), n("2"))], price: n("20") });
    let mut ship = line("s", "Ship", "1", "7.00");
    ship.delivery = true;
    let lines = vec![line("a", "A", "1", "15.00"), line("b", "B", "3", "15.00"), ship];
    // One A and two B (15 + 10) for 20: 5 off, spread 3 to A and 2 to B.
    let o = run(lines.clone(), vec![bundle, promo("ship", Gives::FreeDelivery)], when());
    assert_eq!(discounts(&o), [n("3.00"), n("2.00"), n("7.00")]);
    assert_eq!(o.fired, ["bundle", "ship"]);
    // A free item is added, then taken whole once it is a line.
    let gift = promo("gift", Gives::FreeItem { item: "G".into(), qty: n("1") });
    let o = run(lines.clone(), vec![gift.clone()], when());
    assert_eq!(o.add, [Add { promotion: "gift".into(), item: "G".into(), qty: n("1") }]);
    let mut with = lines;
    with.push(Line { added_by: Some("gift".into()), ..line("g", "G", "1", "4.00") });
    let o = run(with, vec![gift, promo("doc", Gives::PercentOffDocument { percent: n("10") })], when());
    assert!(o.add.is_empty());
    assert_eq!(discounts(&o), [n("1.50"), n("1.50"), n("0"), n("4.00")], "the free line takes no other promotion, delivery none but free delivery");
}

#[test]
fn priority_combining_caps_and_line_discounts() {
    let lines = || {
        let mut b = line("b", "B", "1", "100.00");
        b.line_discount = true;
        vec![line("a", "A", "1", "100.00"), b]
    };
    let mut first = promo("first", Gives::PercentOffDocument { percent: n("10") });
    first.priority = 5;
    let second = promo("second", Gives::AmountOffDocument { amount: n("18") });
    // The higher priority first, the next on what it left.
    assert_eq!(discounts(&run(lines(), vec![second.clone(), first.clone()], when())), [n("19.00"), n("19.00")]);
    // One that may not combine, first: alone.
    first.combinable = false;
    let o = run(lines(), vec![second.clone(), first.clone()], when());
    assert_eq!((discounts(&o), o.fired), (vec![n("10.00"), n("10.00")], vec!["first".to_string()]));
    // Second, it is skipped once another applied.
    first.priority = 0;
    let mut second = second;
    second.priority = 9;
    assert_eq!(run(lines(), vec![second.clone(), first.clone()], when()).fired, ["second"]);
    // A cap, and lines with their own discount left alone.
    let mut capped = promo("capped", Gives::PercentOffDocument { percent: n("50") });
    capped.max_discount = Some(n("30"));
    capped.with_line_discounts = false;
    assert_eq!(discounts(&run(lines(), vec![capped], when())), [n("30.00"), n("0")]);
}

#[test]
fn when_it_applies() {
    let lines = || vec![line("a", "A", "1", "10.00")];
    let fired = |p: &Promotion, w: When| !run(lines(), vec![p.clone()], w).fired.is_empty();
    let mut p = promo("p", Gives::AmountOffDocument { amount: n("1") });
    p.valid_from = Some("2026-11-13".into());
    assert!(!fired(&p, when()), "not yet");
    p.valid_from = None;
    p.weekdays = vec![6, 7];
    assert!(!fired(&p, when()), "a Thursday");
    p.weekdays = vec![];
    // A happy hour past midnight.
    (p.from_time, p.to_time) = (Some("22:00".into()), Some("02:00".into()));
    assert!(!fired(&p, when()));
    assert!(fired(&p, When { time: "23:15:00".into(), ..when() }) && fired(&p, When { time: "01:59".into(), ..when() }));
    (p.from_time, p.to_time) = (None, None);
    p.channels = vec!["Point of Sale".into()];
    assert!(!fired(&p, when()) && fired(&p, When { channel: Some("point of sale".into()), ..when() }));
    p.channels = vec![];
    p.places = vec!["store-1".into()];
    assert!(!fired(&p, When { place: Some("store-2".into()), ..when() }) && fired(&p, When { place: Some("store-1".into()), ..when() }));
    p.places = vec![];
    p.customer_groups = vec!["retail".into()];
    assert!(!fired(&p, When { customer: Some("c1".into()), ..when() }) && fired(&p, When { customer_group: Some("retail".into()), ..when() }));
    p.customer_groups = vec![];
    p.first_order_only = true;
    assert!(!fired(&p, when()) && fired(&p, When { first_order: true, ..when() }));
    p.first_order_only = false;
    p.requires_code = true;
    assert!(!fired(&p, When { codes: vec![Code { promotion: "other".into(), code: "X".into() }], ..when() }));
    let o = run(lines(), vec![p], When { codes: vec![Code { promotion: "p".into(), code: "SPRING-7".into() }], ..when() });
    assert_eq!(o.applied[0].code.as_deref(), Some("SPRING-7"), "the code is kept with what it gave");
}

/// The same input gives the same result, through JSON as the browser calls it.
#[test]
fn json_in_json_out() {
    let input = Input {
        lines: vec![line("a", "A", "1", "10.00")],
        promotions: vec![promo("p", Gives::PercentOffDocument { percent: n("15") })],
        when: when(),
        precision: 2,
        ..Default::default()
    };
    let text = serde_json::to_string(&input).unwrap();
    let out: Outcome = serde_json::from_str(&apply_json(&text)).unwrap();
    assert_eq!(out, apply(&input).unwrap());
    assert_eq!(out.lines[0].discount, n("1.50"));
    assert!(apply_json("{").contains("error"));
}
