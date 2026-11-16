//! New Zealand: GST (Goods and Services Tax Act 1985; Inland Revenue) at 15% since 1 October 2010
//! (12.5% from July 1989, 10% before); zero-rated exports; exempt supplies (financial services,
//! residential rent); the reverse charge on imported services for businesses whose taxable
//! supplies are under 95% of their supplies (a company setting). The return is the GST101A's
//! boxes 5 to 15, on the tax fraction 3/23 of amounts that include GST.

use crate::engine::{Country, Rate, ReverseCharge, Row, digits};

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

/// The return is a definition the package seeds, worked out by ERP's tax returns.
fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

crate::app!("nz", crate::pint::NZ);

#[cfg(test)]
mod tests {
    #[test]
    fn numbers() {
        assert_eq!(super::number("49-091-850"), Some("049-091-850".into()));
        assert_eq!(super::number("136410132"), Some("136-410-132".into()), "the second weights");
        assert_eq!((super::number("136410133"), super::number("9125568")), (None, None));
    }
}
