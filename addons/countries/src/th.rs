//! Thailand: VAT (Revenue Code). The Code's rate is 10%, cut to 7% by royal decree since 1992 and
//! extended each time; the latest runs to 30 September 2027, so 10% applies from 1 October 2027
//! unless it is extended again. Exports at 0%; exempt supplies (basic groceries, education,
//! healthcare, interest, immovable property); VAT on services from abroad paid by the company
//! (PP.36) and claimed as input tax. The return is PP.30's lines.

use crate::engine::{Country, EXEMPT, EXPORT, Figures, Line, REVERSE_CHARGED, Rate, Return, ReverseCharge, Row, ZERO_RATED};
use ferrum_kernel::Doc;
use rust_decimal::Decimal;

pub static COUNTRY: Country = Country {
    package: "th",
    country: "TH",
    rows: &[Row { label: "VAT", sales: "2131", purchases: Some("1151"), self_assessed: None }],
    rates: &[Rate { category: "Standard", row: 0, from: "1992-01-01", rate: "7" }, Rate { category: "Standard", row: 0, from: "2027-10-01", rate: "10" }],
    category_field: "th_vat_category",
    default_category: "Standard",
    kind_field: "th_kind",
    number_field: "th_tax_id",
    number,
    number_hint: "a 13-digit tax identification number whose check digit holds",
    export_reason: "Export: VAT at 0%",
    exempt_reason: "Exempt from VAT (section 81, Revenue Code)",
    reverse_charge: ReverseCharge::Always,
    simplified: None,
    accounts: &[],
};

/// Thirteen digits, the last a check digit (weights 13 to 2, modulo 11).
pub fn number(raw: &str) -> Option<String> {
    let n = crate::engine::digits(raw);
    if n.len() != 13 || !n.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let d: Vec<u32> = n.bytes().map(|c| u32::from(c - b'0')).collect();
    let sum: u32 = (0..12).map(|i| d[i] * (13 - i as u32)).sum();
    ((11 - sum % 11) % 10 == d[12]).then_some(n)
}

fn pp30(f: &Figures, _company: &Doc) -> Vec<Line> {
    let zero = f.sales_net(&[ZERO_RATED, EXPORT]);
    let exempt = f.sales_net(&[EXEMPT]);
    let sales = f.sales_net(&["Standard"]) + zero + exempt;
    let output = f.sales_tax(&["Standard"]);
    let input = f.purchases_tax(&["Standard", REVERSE_CHARGED]);
    vec![
        ("1", "Sales amount", sales),
        ("2", "Less sales subject to 0% tax rate", zero),
        ("3", "Less exempted sales", exempt),
        ("4", "Taxable sales amount", sales - zero - exempt),
        ("5", "Output tax this month", output),
        ("6", "Purchase amount entitled to deduction of input tax", f.purchases_net(&["Standard", REVERSE_CHARGED])),
        ("7", "Input tax this month", input),
        ("8", "Tax payable", (output - input).max(Decimal::ZERO)),
        ("9", "Excess tax payment", (input - output).max(Decimal::ZERO)),
    ]
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![std::sync::Arc::new(Return { name: "th_pp30", label: "VAT Return (PP.30)", country: &COUNTRY, boxes: pp30 })]
}

crate::app!("th");

#[cfg(test)]
mod tests {
    #[test]
    fn rates_and_numbers() {
        use rust_decimal::Decimal;
        assert_eq!(super::COUNTRY.rate("Standard", "2026-10-31").map(|r| r.1), Some(Decimal::from(7)));
        assert_eq!(super::COUNTRY.rate("Standard", "2027-10-01").map(|r| r.1), Some(Decimal::from(10)), "the Code's rate once the decree lapses");
        assert_eq!(super::number("0-1055-36112-01-4"), Some("0105536112014".into()));
        assert_eq!(super::number("0105536112015"), None);
    }
}
