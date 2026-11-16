//! The Philippines: VAT (National Internal Revenue Code, as amended) at 12% since 1 February 2006
//! (10% before); zero-rated export sales; exempt sales; VAT on services from non-residents, which
//! the company withholds and pays itself and then claims as input tax. The return summarises the
//! quarterly BIR Form 2550Q.

use crate::engine::{Country, Rate, ReverseCharge, Row, digits};

pub static COUNTRY: Country = Country {
    package: "ph",
    country: "PH",
    rows: &[Row { label: "VAT", sales: "2131", purchases: Some("1151"), self_assessed: None }],
    rates: &[Rate { category: "Standard", row: 0, from: "1988-01-01", rate: "10" }, Rate { category: "Standard", row: 0, from: "2006-02-01", rate: "12" }],
    category_field: "ph_vat_category",
    default_category: "Standard",
    kind_field: "ph_kind",
    number_field: "ph_tin",
    number,
    number_hint: "a TIN of nine digits, with its branch code (e.g. 123-456-789-000)",
    export_reason: "Zero-rated export sale",
    exempt_reason: "VAT-exempt sale",
    reverse_charge: ReverseCharge::Always,
    simplified: None,
    accounts: &[],
};

/// Nine digits and, when given, a branch code of three to five.
pub fn number(raw: &str) -> Option<String> {
    let n = digits(raw);
    let ok = n.bytes().all(|c| c.is_ascii_digit()) && (n.len() == 9 || (12..=14).contains(&n.len()));
    ok.then(|| if n.len() == 9 { format!("{}-{}-{}", &n[..3], &n[3..6], &n[6..]) } else { format!("{}-{}-{}-{}", &n[..3], &n[3..6], &n[6..9], &n[9..]) })
}

/// The return is a definition the package seeds, worked out by ERP's tax returns.
fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

crate::app!("ph");

#[cfg(test)]
mod tests {
    #[test]
    fn numbers() {
        assert_eq!(super::number("123456789000"), Some("123-456-789-000".into()));
        assert_eq!(super::number("123-456-789"), Some("123-456-789".into()));
        assert_eq!(super::number("1234567"), None);
    }
}
