//! Saudi Arabia: VAT (ZATCA) at 15% since 1 July 2020 (5% from 1 January 2018); zero-rated
//! exports and listed supplies; exempt supplies (financial services, residential rent); the
//! reverse charge on services from abroad. The return lays out ZATCA's VAT return boxes (amounts,
//! then their VAT).

use crate::engine::{Country, EXEMPT, EXPORT, Figures, Line, REVERSE_CHARGED, Rate, Return, ReverseCharge, Row, ZERO_RATED, digits};
use ferrum_kernel::Doc;

pub static COUNTRY: Country = Country {
    package: "sa",
    country: "SA",
    rows: &[Row { label: "VAT", sales: "2131", purchases: Some("1151"), self_assessed: None }],
    rates: &[Rate { category: "Standard", row: 0, from: "2018-01-01", rate: "5" }, Rate { category: "Standard", row: 0, from: "2020-07-01", rate: "15" }],
    category_field: "sa_vat_category",
    default_category: "Standard",
    kind_field: "sa_kind",
    number_field: "sa_vat_number",
    number,
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

fn vat_return(f: &Figures, _company: &Doc) -> Vec<Line> {
    let sales_vat = f.sales_tax(&["Standard"]);
    let purchases_vat = f.purchases_tax(&["Standard", REVERSE_CHARGED]);
    vec![
        ("1", "Standard rated sales", f.sales_net(&["Standard"])),
        ("1 VAT", "VAT on standard rated sales", sales_vat),
        ("3", "Zero rated domestic sales", f.sales_net(&[ZERO_RATED])),
        ("4", "Exports", f.sales_net(&[EXPORT])),
        ("5", "Exempt sales", f.sales_net(&[EXEMPT])),
        ("6", "Total sales", f.sales_net(&["Standard", ZERO_RATED, EXPORT, EXEMPT])),
        ("6 VAT", "VAT on total sales", sales_vat),
        ("7", "Standard rated domestic purchases", f.purchases_net(&["Standard"])),
        ("7 VAT", "VAT on standard rated domestic purchases", f.purchases_tax(&["Standard"])),
        ("9", "Imports subject to VAT accounted for through reverse charge mechanism", f.purchases_net(&[REVERSE_CHARGED])),
        ("9 VAT", "VAT on imports through reverse charge", f.purchases_tax(&[REVERSE_CHARGED])),
        ("10", "Zero rated purchases", f.purchases_net(&[ZERO_RATED])),
        ("11", "Exempt purchases", f.purchases_net(&[EXEMPT])),
        ("12", "Total purchases", f.purchases_net(&["Standard", REVERSE_CHARGED, ZERO_RATED, EXEMPT])),
        ("12 VAT", "VAT on total purchases", purchases_vat),
        ("13", "Total VAT due for current period", sales_vat + f.purchases_tax(&[REVERSE_CHARGED]) - purchases_vat),
    ]
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![std::sync::Arc::new(Return { name: "sa_vat_return", label: "VAT Return (ZATCA)", country: &COUNTRY, boxes: vat_return })]
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
