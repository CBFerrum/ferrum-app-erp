//! New Zealand: GST (Goods and Services Tax Act 1985; Inland Revenue) at 15% since 1 October 2010
//! (12.5% from July 1989, 10% before); zero-rated exports; exempt supplies (financial services,
//! residential rent); the reverse charge on imported services for businesses whose taxable
//! supplies are under 95% of their supplies (a company setting). The return is the GST101A's
//! boxes 5 to 15, on the tax fraction 3/23 of amounts that include GST.

use crate::engine::{Country, EXPORT, Figures, Line, REVERSE_CHARGED, Rate, Return, ReverseCharge, Row, ZERO_RATED, digits};
use ferrum_kernel::Doc;
use rust_decimal::{Decimal, RoundingStrategy};

pub static COUNTRY: Country = Country {
    package: "nz",
    country: "NZ",
    rows: &[Row { label: "GST", sales: "2131", purchases: Some("1151"), self_assessed: None }],
    rates: &[
        Rate { category: "Standard", row: 0, from: "1986-10-01", rate: "10" },
        Rate { category: "Standard", row: 0, from: "1989-07-01", rate: "12.5" },
        Rate { category: "Standard", row: 0, from: "2010-10-01", rate: "15" },
    ],
    category_field: "nz_gst_category",
    default_category: "Standard",
    kind_field: "nz_kind",
    number_field: "nz_gst_number",
    number,
    number_hint: "a GST (IRD) number of 8 or 9 digits whose check digit holds",
    export_reason: "Zero-rated export",
    exempt_reason: "Exempt supply",
    reverse_charge: ReverseCharge::When("nz_reverse_charge"),
    simplified: None,
    accounts: &[],
};

/// Inland Revenue's check: the eight base digits weighted 3, 2, 7, 6, 5, 4, 3, 2 (again with 7, 4,
/// 3, 2, 5, 2, 7, 6 when that gives 10), modulo 11.
pub fn number(raw: &str) -> Option<String> {
    let mut n = digits(raw);
    if n.len() == 8 {
        n.insert(0, '0');
    }
    if n.len() != 9 || !n.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let v: u64 = n.parse().ok()?;
    if !(10_000_000 < v && v < 150_000_000) {
        return None;
    }
    let d: Vec<u32> = n.bytes().map(|c| u32::from(c - b'0')).collect();
    for weights in [[3, 2, 7, 6, 5, 4, 3, 2], [7, 4, 3, 2, 5, 2, 7, 6]] {
        let r = d[..8].iter().zip(weights).map(|(a, w)| a * w).sum::<u32>() % 11;
        let check = if r == 0 { 0 } else { 11 - r };
        if check != 10 {
            return (check == d[8]).then(|| format!("{}-{}-{}", &n[..3], &n[3..6], &n[6..]));
        }
    }
    None
}

fn fraction(v: Decimal) -> Decimal {
    (v * Decimal::from(3) / Decimal::from(23)).round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
}

fn gst101a(f: &Figures, _company: &Doc) -> Vec<Line> {
    let b5 = f.sales_net(&["Standard", ZERO_RATED, EXPORT]) + f.sales_tax(&["Standard"]);
    let b6 = f.sales_net(&[ZERO_RATED, EXPORT]);
    let b8 = fraction(b5 - b6);
    let b9 = f.purchases_tax(&[REVERSE_CHARGED]);
    let b11 = f.purchases_net(&["Standard"]) + f.purchases_tax(&["Standard"]);
    let b12 = fraction(b11);
    let b13 = f.purchases_tax(&[REVERSE_CHARGED]);
    vec![
        ("5", "Total sales and income for the period (including GST and any zero-rated supplies)", b5),
        ("6", "Zero-rated supplies included in Box 5", b6),
        ("7", "Box 5 less Box 6", b5 - b6),
        ("8", "Box 7 multiplied by three and divided by twenty-three", b8),
        ("9", "Adjustments from your calculation sheet", b9),
        ("10", "Total GST collected on sales and income", b8 + b9),
        ("11", "Total purchases and expenses (including GST) for which tax invoicing requirements have been met", b11),
        ("12", "Box 11 multiplied by three and divided by twenty-three", b12),
        ("13", "Credit adjustments from your calculation sheet", b13),
        ("14", "Total GST credit for purchases and expenses", b12 + b13),
        ("15", "Difference between Box 10 and Box 14 (GST to pay, or a refund when negative)", b8 + b9 - b12 - b13),
    ]
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![std::sync::Arc::new(Return { name: "nz_gst101a", label: "GST Return (GST101A)", country: &COUNTRY, boxes: gst101a })]
}

crate::app!("nz");

#[cfg(test)]
mod tests {
    #[test]
    fn numbers() {
        assert_eq!(super::number("49-091-850"), Some("049-091-850".into()));
        assert_eq!(super::number("136410132"), Some("136-410-132".into()), "the second weights");
        assert_eq!((super::number("136410133"), super::number("9125568")), (None, None));
        assert_eq!(super::fraction(rust_decimal::Decimal::from(115)), rust_decimal::Decimal::from(15));
    }
}
