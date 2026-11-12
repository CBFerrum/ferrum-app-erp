use super::*;
use ferrum_calc::{Rounding, TaxIn};
use promotions::{GetAt, Gives, Promotion, Set};
use std::str::FromStr;

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

/// A small deterministic generator (no dependency): the same seed, the same baskets.
struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, in_ten: u64) -> bool {
        self.below(10) < in_ten
    }
    fn money(&mut self, most: u64) -> Decimal {
        Decimal::new((1 + self.below(most * 100)) as i64, 2)
    }
}

fn promo(id: &str, gives: Gives) -> Promotion {
    serde_json::from_value(serde_json::json!({"id": id, "title": id, "gives": gives})).unwrap()
}

/// A snapshot and a basket within it.
fn generate(g: &mut Gen) -> (Snapshot, Basket) {
    let items: Vec<Item> = (0..2 + g.below(4))
        .map(|i| Item {
            id: format!("i{i}"),
            item_group: Some(format!("g{}", g.below(2))),
            stock_uom: Some("nos".into()),
            tax_rates: if g.chance(3) {
                vec![LineRate { tax_account: "acc0".into(), rate: Decimal::from(g.below(3) * 5), ..Default::default() }]
            } else {
                vec![]
            },
            ..Default::default()
        })
        .collect();
    let prices = items.iter().map(|i| Price { item: i.id.clone(), rate: g.money(200), valid_from: Some("2026-01-01".into()), ..Default::default() }).collect();
    let mut rules = Vec::new();
    for i in &items {
        if g.chance(3) {
            rules.push(Rule {
                id: format!("r{}", i.id),
                apply_on: Some("Item".into()),
                item: Some(i.id.clone()),
                discount_percent: Decimal::from(5 + g.below(20)),
                ..Default::default()
            });
        }
    }
    let inclusive = g.chance(4);
    let mut taxes = vec![Tax {
        calc: TaxIn {
            key: "t0".into(),
            charge_type: ChargeType::OnNetTotal,
            rate: Decimal::from(5 + g.below(16)),
            included_in_price: inclusive,
            ..Default::default()
        },
        label: TaxLabel { account: Some("acc0".into()), description: Some("Tax".into()), ..Default::default() },
    }];
    if !inclusive && g.chance(3) {
        taxes.push(Tax {
            calc: TaxIn { key: "t1".into(), charge_type: ChargeType::OnPreviousRowTotal, rate: n("2"), row_ref: Some(0), ..Default::default() },
            label: TaxLabel { account: Some("acc1".into()), ..Default::default() },
        });
    }
    let ids: Vec<String> = items.iter().map(|i| i.id.clone()).collect();
    let pick = |g: &mut Gen| ids[g.below(ids.len() as u64) as usize].clone();
    let mut promos = Vec::new();
    for p in 0..g.below(4) {
        let mut x = match g.below(5) {
            0 => promo(&format!("p{p}"), Gives::PercentOffDocument { percent: Decimal::from(5 + g.below(30)) }),
            1 => promo(&format!("p{p}"), Gives::AmountOffLines { amount: g.money(40) }),
            2 => promo(&format!("p{p}"), Gives::BuyGet { buy: n("2"), get: n("1"), get_set: Set::default(), at: GetAt::Free, cheapest_first: true }),
            // Two different items: a bundle naming one item in two parts can take more than its line (an engine fault).
            3 => promo(
                &format!("p{p}"),
                Gives::Bundle { parts: vec![(ids[0].clone(), n("1")), (ids[1].clone(), Decimal::from(1 + g.below(2)))], price: g.money(150) },
            ),
            _ => promo(&format!("p{p}"), Gives::FreeItem { item: pick(g), qty: n("1") }),
        };
        if matches!(x.gives, Gives::AmountOffLines { .. }) {
            x.qualifying = Set { items: vec![pick(g)], item_groups: vec![] };
        }
        x.priority = g.below(3) as i64;
        x.combinable = g.chance(8);
        x.with_line_discounts = g.chance(7);
        if g.chance(3) {
            x.min_amount = g.money(300);
        }
        if g.chance(2) {
            x.weekdays = vec![4];
        }
        // Not on a free item: the old replay priced a free item without its pricing rule (the server applies it), which
        // shows only when a cap leaves part of the item to pay.
        if g.chance(2) && !matches!(x.gives, Gives::FreeItem { .. }) {
            x.max_discount = Some(g.money(30));
        }
        promos.push(x);
    }
    // At most one free item: the till's old replay added one round of them.
    let mut free = false;
    promos.retain(|p| !matches!(p.gives, Gives::FreeItem { .. }) || !std::mem::replace(&mut free, true));
    let snapshot = Snapshot {
        schema: SCHEMA,
        valid_from: "2026-11-12".into(),
        valid_until: "2026-11-26".into(),
        time_zone: None,
        currency: Some("XXX".into()),
        precision: 2,
        base_precision: Some(2),
        conversion_rate: Decimal::ONE,
        rounding: if g.chance(5) { Rounding::HalfUp } else { Rounding::HalfEven },
        round_total_to: g.chance(4).then(|| n("0.05")),
        tax_rounding: Default::default(),
        taxes,
        items,
        prices,
        rules,
        promotions: promos,
        customer: Some("walk-in".into()),
        customer_group: Some("retail".into()),
        channel: Some("Point of Sale".into()),
        place: Some("store".into()),
    };
    let lines = (0..1 + g.below(4))
        .map(|_| BasketLine {
            item: pick(g),
            qty: Decimal::from(1 + g.below(5)),
            discount_percent: g.chance(1).then(|| Decimal::from(g.below(15))),
            ..Default::default()
        })
        .collect();
    let at = if g.chance(5) { "2026-11-12 10:30" } else { "2026-11-14 18:05" };
    (snapshot, Basket { lines, at: at.into(), discount_percent: if g.chance(2) { n("5") } else { Decimal::ZERO }, ..Default::default() })
}

/// Point of Sale 0.7.1's `snapshot::replay`, kept here only to hold the crate to it: each item's
/// line as a sale of one priced it (its price list price, its rule's discount), the promotions in
/// two rounds, then the engine with the promotions' share.
fn old_replay(s: &Snapshot, b: &Basket) -> Result<CalcOutput, String> {
    let item = |id: &str| s.items.iter().find(|i| i.id == id).unwrap();
    let rows: Vec<(String, Option<String>)> = s.taxes.iter().map(|t| (t.calc.key.clone(), t.label.account.clone())).collect();
    // The book's line for an item: what a sale of one gives it.
    let book = |id: &str| {
        let price = list_price(&s.prices, id, "2026-11-12", Some("nos"), Some("nos"), Decimal::ONE).unwrap();
        let ask = RuleAsk {
            item: Some(id),
            item_group: item(id).item_group.as_deref(),
            qty: Decimal::ONE,
            customer: Some("walk-in"),
            customer_group: Some("retail"),
            on: "2026-11-12",
        };
        let discount = best_rule(&s.rules, ask, |_| true).map(|r| r.discount_percent).unwrap_or_default();
        let by_account = item(id).tax_rates.iter().map(|t| (t.tax_account.clone(), t.rate)).collect();
        (price, discount, line_tax_rates(&by_account, &rows))
    };
    let mut input = CalcInput {
        precision: s.precision,
        base_precision: s.base_precision,
        conversion_rate: s.conversion_rate,
        rounding: s.rounding,
        lines: vec![],
        taxes: s.taxes.iter().map(|t| t.calc.clone()).collect(),
        discount: None,
        round_total_to: s.round_total_to,
        tax_rounding: s.tax_rounding,
    };
    let mut groups: Vec<Option<String>> = Vec::new();
    let mut typed: Vec<bool> = Vec::new();
    for (i, l) in b.lines.iter().enumerate() {
        let (price, discount, tax_rates) = book(&l.item);
        let discount_percent = l.discount_percent.unwrap_or(discount);
        typed.push(discount_percent > Decimal::ZERO);
        groups.push(item(&l.item).item_group.clone());
        input.lines.push(LineIn { key: format!("l{i}"), qty: l.qty, price, discount_percent, tax_rates, tax_qty: Some(l.qty) });
    }
    input.discount = (b.discount_percent > Decimal::ZERO).then_some(DocDiscount::Percent { value: b.discount_percent });
    let (date, time) = b.at.split_once(' ').unwrap();
    let when = promotions::When {
        date: date.into(),
        weekday: iso_weekday(date).unwrap(),
        time: time.into(),
        customer: Some("walk-in".into()),
        customer_group: Some("retail".into()),
        channel: Some("Point of Sale".into()),
        place: Some("store".into()),
        ..Default::default()
    };
    fn bare(input: &CalcInput) -> CalcInput {
        CalcInput { taxes: vec![], discount: None, round_total_to: None, ..input.clone() }
    }
    let mut added: Vec<(String, String)> = Vec::new();
    let mut out = promotions::Outcome::default();
    for _ in 0..2 {
        let before = compute(&bare(&input)).map_err(|e| e.to_string())?;
        let lines: Vec<promotions::Line> = input
            .lines
            .iter()
            .zip(&before.lines)
            .enumerate()
            .map(|(i, (l, o))| promotions::Line {
                key: l.key.clone(),
                item: b.lines.get(i).map(|x| x.item.clone()).or_else(|| added.get(i - b.lines.len()).map(|a| a.1.clone())),
                item_group: groups.get(i).cloned().flatten(),
                qty: l.qty,
                amount: o.amount,
                line_discount: typed.get(i).copied().unwrap_or(false),
                delivery: false,
                added_by: added.get(i.wrapping_sub(b.lines.len())).map(|a| a.0.clone()),
            })
            .collect();
        out = promotions::apply(&promotions::Input {
            lines,
            promotions: s.promotions.clone(),
            when: when.clone(),
            precision: input.precision,
            rounding: input.rounding,
        })?;
        let mut grew = false;
        for add in &out.add {
            if added.iter().any(|a| a.0 == add.promotion && a.1 == add.item) {
                continue;
            }
            input.lines.push(LineIn { key: format!("f{}", added.len()), qty: add.qty, price: book(&add.item).0, ..Default::default() });
            groups.push(item(&add.item).item_group.clone());
            typed.push(false);
            added.push((add.promotion.clone(), add.item.clone()));
            grew = true;
        }
        if !grew {
            break;
        }
    }
    let shares: BTreeMap<&str, Decimal> = out.lines.iter().map(|l| (l.key.as_str(), l.percent)).collect();
    for l in input.lines.iter_mut() {
        if let Some(p) = shares.get(l.key.as_str()) {
            l.discount_percent = promotions::combined_percent(l.discount_percent, *p);
        }
    }
    compute(&input).map_err(|e| e.to_string())
}

/// On generated baskets the crate prices exactly as the till's old replay did: what moved out of
/// Point of Sale into here changed no total.
#[test]
fn equals_the_old_replay_on_generated_baskets() {
    let mut g = Gen(7);
    let mut promoted = 0;
    for case in 0..400 {
        let (s, b) = generate(&mut g);
        let new = price_basket(&s, &b)
            .unwrap_or_else(|e| panic!("case {case}: {e} {}", serde_json::to_string(&Request { snapshot: s.clone(), basket: b.clone() }).unwrap()));
        let old = old_replay(&s, &b).unwrap();
        let t = &new.totals;
        assert_eq!(
            (t.net_total, t.total_taxes, t.grand_total, t.rounded_total),
            (old.net_total, old.total_taxes, old.grand_total, old.rounded_total),
            "case {case}: {}",
            serde_json::to_string(&Request { snapshot: s.clone(), basket: b.clone() }).unwrap()
        );
        promoted += (!new.applied.is_empty()) as usize;
        // The summary adds up to the taxes.
        let summed: Decimal = new.tax_summary.iter().filter(|x| !x.reverse_charge).map(|x| x.tax_amount).sum();
        assert_eq!(summed, new.totals.taxes.iter().map(|x| x.tax_amount).sum::<Decimal>(), "case {case}");
    }
    assert!(promoted > 100, "promotions applied in {promoted} of 400");
}

/// The same request is the same bytes, and the browser's build is held to these bytes: the
/// requests and what the native crate answers are kept in `tests/` (`UPDATE_PRICING_FIXTURES=1`
/// writes them again), and `scripts/pricing-wasm.sh --check` runs them through the WebAssembly.
#[test]
fn fixtures_are_priced_byte_for_byte() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut g = Gen(2026);
    let requests: Vec<String> = (0..60)
        .map(|_| {
            let (snapshot, basket) = generate(&mut g);
            serde_json::to_string(&Request { snapshot, basket }).unwrap()
        })
        .collect();
    let answers: Vec<String> = requests.iter().map(|r| price_json(r)).collect();
    assert_eq!(answers, requests.iter().map(|r| price_json(r)).collect::<Vec<_>>(), "deterministic");
    if std::env::var("UPDATE_PRICING_FIXTURES").is_ok() {
        std::fs::write(dir.join("baskets.jsonl"), requests.join("\n") + "\n").unwrap();
        std::fs::write(dir.join("priced.jsonl"), answers.join("\n") + "\n").unwrap();
    }
    let kept = std::fs::read_to_string(dir.join("priced.jsonl")).expect("tests/priced.jsonl");
    let asked = std::fs::read_to_string(dir.join("baskets.jsonl")).expect("tests/baskets.jsonl");
    assert_eq!(asked.lines().collect::<Vec<_>>(), requests, "the generator changed: write the fixtures again");
    assert_eq!(kept.lines().collect::<Vec<_>>(), answers);
}

/// A rule's rate, units, a price list by date, a free item priced and taken whole, inclusive taxes
/// and the summary; the snapshot's days; another schema refused.
#[test]
fn a_basket_by_hand() {
    let snapshot: Snapshot = serde_json::from_value(serde_json::json!({
        "schema": 1, "valid_from": "2026-11-12", "valid_until": "2026-11-26", "precision": 2, "round_total_to": "0.05",
        "taxes": [{"key": "t0", "charge_type": "on_net_total", "rate": "10", "included_in_price": true, "account": "vat", "description": "VAT"}],
        "items": [
            {"id": "lamp", "item_group": "goods", "stock_uom": "nos", "units": [{"uom": "box", "factor": "6"}]},
            {"id": "bulb", "item_group": "goods", "stock_uom": "nos", "standard_rate": "4", "tax_rates": [{"tax_account": "vat", "rate": "0", "category": "Zero Rated"}]}
        ],
        "prices": [
            {"item": "lamp", "rate": "11", "valid_from": "2026-01-01"},
            {"item": "lamp", "rate": "12", "valid_from": "2026-11-13"},
            {"item": "lamp", "uom": "box", "rate": "60", "valid_from": "2026-01-01"}
        ],
        "rules": [{"id": "r", "apply_on": "Item Group", "item_group": "goods", "min_qty": "10", "discount_percent": "10"}],
        "promotions": [{"id": "p", "title": "A bulb over 100", "gives": {"kind": "free_item", "item": "bulb", "qty": "1"}, "min_amount": "100"}],
        "customer": "walk-in", "channel": "Point of Sale"
    }))
    .unwrap();
    let basket = |at: &str, lines: serde_json::Value| Basket { lines: serde_json::from_value(lines).unwrap(), at: at.into(), ..Default::default() };
    let p = price_basket(&snapshot, &basket("2026-11-12 09:00", serde_json::json!([{"item": "lamp", "qty": "2", "uom": "box"}]))).unwrap();
    // A box has its own price; twelve lamps meet the rule; 108 is over 100, so a bulb comes free.
    assert_eq!((p.lines[0].price_list_rate, p.lines[0].discount_percent, p.lines[0].stock_qty), (n("60"), n("10"), n("12")));
    assert_eq!((p.lines[1].item.as_str(), p.lines[1].added_by_promotion.as_deref(), p.lines[1].price_list_rate), ("bulb", Some("p"), n("4")));
    assert_eq!((p.lines[1].promotion_percent, p.totals.lines[1].amount), (n("100"), n("0.00")));
    assert_eq!((p.totals.grand_total, p.totals.total_taxes, p.totals.rounded_total), (n("108.00"), n("9.82"), n("108.00")));
    assert_eq!(p.applied.len(), 1);
    assert_eq!(p.tax_summary.iter().map(|x| (x.category.as_str(), x.tax_amount)).collect::<Vec<_>>(), [("Standard", n("9.82")), ("Zero Rated", n("0.00"))]);
    // The next day the lamp's price changes; one lamp, no rule, no bulb.
    let p = price_basket(&snapshot, &basket("2026-11-13 09:00", serde_json::json!([{"item": "lamp", "qty": "1"}]))).unwrap();
    assert_eq!((p.lines.len(), p.totals.rounded_total), (1, n("12.00")));
    assert!(price_basket(&snapshot, &basket("2026-11-27 09:00", serde_json::json!([{"item": "lamp", "qty": "1"}]))).unwrap_err().contains("not on 2026-11-27"));
    assert!(price_basket(&snapshot, &basket("2026-11-12 09:00", serde_json::json!([{"item": "lamp", "qty": "1", "uom": "crate"}]))).is_err());
    let other = Snapshot { schema: 2, ..snapshot };
    assert!(price_basket(&other, &basket("2026-11-12 09:00", serde_json::json!([]))).unwrap_err().contains("schema 2"));
    assert!(price_json("{").contains("error"));
}

#[test]
fn weekdays() {
    assert_eq!(["1970-01-01", "2026-11-12", "2026-11-16", "2000-02-29", "2026-11-15"].map(iso_weekday), [Some(4), Some(4), Some(1), Some(2), Some(7)]);
    assert_eq!(iso_weekday("2026-13-01"), None);
}
