//! The United Arab Emirates: VAT (Federal Tax Authority) at 5% since 1 January 2018; zero-rated
//! exports and listed supplies; exempt supplies (some financial services, residential property,
//! local passenger transport); the reverse charge on imported services. The return lays out the
//! VAT 201 boxes, standard-rated supplies on the company's emirate's line.

use crate::engine::{Country, EXEMPT, EXPORT, Figures, Line, REVERSE_CHARGED, Rate, Return, ReverseCharge, Row, ZERO_RATED, digits};
use ferrum_kernel::Doc;
use rust_decimal::Decimal;

pub static COUNTRY: Country = Country {
    package: "ae",
    country: "AE",
    rows: &[Row { label: "VAT", sales: "2131", purchases: Some("1151"), self_assessed: None }],
    rates: &[Rate { category: "Standard", row: 0, from: "2018-01-01", rate: "5" }],
    category_field: "ae_vat_category",
    default_category: "Standard",
    kind_field: "ae_kind",
    number_field: "ae_trn",
    number,
    number_hint: "a Tax Registration Number of 15 digits",
    export_reason: "Zero-rated export",
    exempt_reason: "Exempt supply",
    reverse_charge: ReverseCharge::Always,
    simplified: None,
    accounts: &[],
};

pub fn number(raw: &str) -> Option<String> {
    let n = digits(raw);
    (n.len() == 15 && n.bytes().all(|c| c.is_ascii_digit())).then_some(n)
}

const EMIRATES: [(&str, &str, &str); 7] = [
    ("1a", "Standard rated supplies in Abu Dhabi", "Abu Dhabi"),
    ("1b", "Standard rated supplies in Dubai", "Dubai"),
    ("1c", "Standard rated supplies in Sharjah", "Sharjah"),
    ("1d", "Standard rated supplies in Ajman", "Ajman"),
    ("1e", "Standard rated supplies in Umm Al Quwain", "Umm Al Quwain"),
    ("1f", "Standard rated supplies in Ras Al Khaimah", "Ras Al Khaimah"),
    ("1g", "Standard rated supplies in Fujairah", "Fujairah"),
];

fn vat201(f: &Figures, company: &Doc) -> Vec<Line> {
    let emirate = crate::util::s(company, "ae_emirate").unwrap_or("Dubai");
    let standard = f.sales_net(&["Standard"]);
    let mut out: Vec<Line> = EMIRATES.iter().map(|(b, l, e)| (*b, *l, if *e == emirate { standard } else { Decimal::ZERO })).collect();
    let reverse = f.purchases_net(&[REVERSE_CHARGED]);
    let reverse_vat = f.purchases_tax(&[REVERSE_CHARGED]);
    let due = f.sales_tax(&["Standard"]) + reverse_vat;
    let recoverable = f.purchases_tax(&["Standard", REVERSE_CHARGED]);
    out.extend([
        ("3", "Supplies subject to the reverse charge provisions", reverse),
        ("3 VAT", "VAT on supplies subject to the reverse charge provisions", reverse_vat),
        ("4", "Zero rated supplies", f.sales_net(&[ZERO_RATED, EXPORT])),
        ("5", "Supplies of goods and services which are exempt from VAT", f.sales_net(&[EXEMPT])),
        ("8", "Totals", standard + reverse + f.sales_net(&[ZERO_RATED, EXPORT, EXEMPT])),
        ("8 VAT", "Total VAT on sales and all other outputs", due),
        ("9", "Standard rated expenses", f.purchases_net(&["Standard"])),
        ("9 VAT", "Recoverable VAT on standard rated expenses", f.purchases_tax(&["Standard"])),
        ("10", "Supplies subject to the reverse charge provisions", reverse),
        ("10 VAT", "Recoverable VAT on supplies subject to the reverse charge", reverse_vat),
        ("12", "Total value of due tax for the period", due),
        ("13", "Total value of recoverable tax for the period", recoverable),
        ("14", "Payable tax for the period", due - recoverable),
    ]);
    out
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![std::sync::Arc::new(Return { name: "ae_vat201", label: "VAT Return (VAT 201)", country: &COUNTRY, boxes: vat201 })]
}

crate::app!("ae", crate::pint::AE);

#[cfg(test)]
mod tests {
    #[test]
    fn numbers() {
        assert_eq!(super::number("100-1234-5678-9003"), Some("100123456789003".into()));
        assert_eq!(super::number("12345"), None);
    }
}
