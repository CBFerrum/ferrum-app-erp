//! Australia: GST (A New Tax System (Goods and Services Tax) Act 1999; ATO) at 10% since 1 July
//! 2000; GST-free supplies (the Zero Rated category: basic food, health, education, exports);
//! input-taxed supplies (the Exempt category: financial supplies, residential rent); the reverse
//! charge on imported services for businesses that cannot claim all of their GST credits (a
//! company setting). The return lays out the BAS's GST labels (G1 includes GST; every purchase
//! goes to G11, non-capital).

use crate::engine::{Country, EXEMPT, EXPORT, Figures, Line, REVERSE_CHARGED, Rate, Return, ReverseCharge, Row, ZERO_RATED, digits};
use ferrum_kernel::Doc;

pub static COUNTRY: Country = Country {
    package: "au",
    country: "AU",
    rows: &[Row { label: "GST", sales: "2131", purchases: Some("1151"), self_assessed: None }],
    rates: &[Rate { category: "Taxable", row: 0, from: "2000-07-01", rate: "10" }],
    category_field: "au_gst_category",
    default_category: "Taxable",
    kind_field: "au_kind",
    number_field: "au_abn",
    number,
    number_hint: "an ABN: 11 digits whose check holds",
    export_reason: "GST-free export",
    exempt_reason: "Input-taxed supply",
    reverse_charge: ReverseCharge::When("au_reverse_charge"),
    simplified: None,
    accounts: &[],
};

/// Eleven digits: one off the first, weighted 10, 1, 3, 5... 19, a multiple of 89.
pub fn number(raw: &str) -> Option<String> {
    let n = digits(raw);
    if n.len() != 11 || !n.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut d: Vec<i64> = n.bytes().map(|c| i64::from(c - b'0')).collect();
    d[0] -= 1;
    let sum: i64 = d.iter().zip([10, 1, 3, 5, 7, 9, 11, 13, 15, 17, 19]).map(|(a, w)| a * w).sum();
    (sum % 89 == 0).then(|| format!("{} {} {} {}", &n[..2], &n[2..5], &n[5..8], &n[8..]))
}

fn bas(f: &Figures, _company: &Doc) -> Vec<Line> {
    let reverse_gst = f.purchases_tax(&[REVERSE_CHARGED]);
    vec![
        ("G1", "Total sales (including any GST)", f.sales_gross),
        ("G2", "Export sales", f.sales_net(&[EXPORT])),
        ("G3", "Other GST-free sales", f.sales_net(&[ZERO_RATED])),
        ("G4", "Input taxed sales", f.sales_net(&[EXEMPT])),
        ("G11", "Non-capital purchases (including any GST)", f.purchases_gross),
        ("1A", "GST on sales", f.sales_tax(&["Taxable"]) + reverse_gst),
        ("1B", "GST on purchases", f.purchases_tax(&["Taxable", REVERSE_CHARGED])),
    ]
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![std::sync::Arc::new(Return { name: "au_bas_gst", label: "BAS: GST", country: &COUNTRY, boxes: bas })]
}

crate::app!("au");

#[cfg(test)]
mod tests {
    #[test]
    fn numbers() {
        assert_eq!(super::number("51824753556"), Some("51 824 753 556".into()), "the ATO's own ABN");
        assert_eq!(super::number("51824753557"), None);
    }
}
