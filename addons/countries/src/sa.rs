//! Saudi Arabia: VAT (ZATCA) at 15% since 1 July 2020 (5% from 1 January 2018); zero-rated
//! exports and listed supplies; exempt supplies (financial services, residential rent); the
//! reverse charge on services from abroad. The return lays out ZATCA's VAT return boxes (amounts,
//! then their VAT).

use crate::engine::{Country, Rate, ReverseCharge, Row, digits};

pub static COUNTRY: Country = Country {
    package: "sa",
    country: "SA",
    rows: &[Row { label: "VAT", sales: "2131", purchases: Some("1151"), self_assessed: None }],
    rates: &[Rate { category: "Standard", row: 0, from: "2018-01-01", rate: "5" }, Rate { category: "Standard", row: 0, from: "2020-07-01", rate: "15" }],
    category_field: "sa_vat_category",
    default_category: "Standard",
    kind_field: "sa_kind",
    number_field: "sa_vat_number",
    number: crate::engine::Number::Code(number),
    number_hint: "a VAT registration number: 15 digits, beginning and ending with 3",
    export_reason: "Zero-rated export",
    exempt_reason: "Exempt supply",
    reverse_charge: ReverseCharge::Always,
    simplified: None,
    accounts: &[],
};

pub fn number(raw: &str) -> Option<String> {
    let n = digits(raw);
    (n.len() == 15 && n.bytes().all(|c| c.is_ascii_digit()) && n.starts_with('3') && n.ends_with('3')).then_some(n)
}

/// The return is a definition the package seeds, worked out by ERP's tax returns.
fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

crate::app!("sa");

#[cfg(test)]
mod tests {
    #[test]
    fn numbers() {
        assert_eq!(super::number("310 1234567 00003"), Some("310123456700003".into()));
        assert_eq!(super::number("410123456700003"), None);
    }
}
