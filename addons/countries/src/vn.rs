//! Vietnam: VAT (Law 48/2024/QH15 from 1 July 2025) at 10%, 5% for essential goods and services,
//! 0% for exports; the 2% cut to 8% for goods and services at 10% that the National Assembly's
//! resolutions list (February to December 2022, then without a break from July 2023 to 31
//! December 2026, last by Resolution 204/2025/QH15); VAT on services bought from abroad declared by
//! the buyer (foreign contractor tax) and claimed as input tax. The return lays out declaration
//! 01/GTGT's indicators.

use crate::engine::{Country, EXEMPT, EXPORT, Figures, Line, OUT_OF_SCOPE, REVERSE_CHARGED, Rate, Return, ReverseCharge, Row, ZERO_RATED, digits};
use ferrum_kernel::Doc;

pub static COUNTRY: Country = Country {
    package: "vn",
    country: "VN",
    rows: &[Row { label: "VAT", sales: "2131", purchases: Some("1151"), self_assessed: None }],
    rates: &[
        Rate { category: "Standard", row: 0, from: "2009-01-01", rate: "10" },
        Rate { category: "Standard (Reduced to 8%)", row: 0, from: "2009-01-01", rate: "10" },
        Rate { category: "Standard (Reduced to 8%)", row: 0, from: "2022-02-01", rate: "8" },
        Rate { category: "Standard (Reduced to 8%)", row: 0, from: "2023-01-01", rate: "10" },
        Rate { category: "Standard (Reduced to 8%)", row: 0, from: "2023-07-01", rate: "8" },
        Rate { category: "Standard (Reduced to 8%)", row: 0, from: "2027-01-01", rate: "10" },
        Rate { category: "Reduced 5%", row: 0, from: "2009-01-01", rate: "5" },
    ],
    category_field: "vn_vat_category",
    default_category: "Standard",
    kind_field: "vn_kind",
    number_field: "vn_tax_code",
    number,
    number_hint: "a tax code of 10 digits, or 10 and a 3-digit branch (0101234567-001)",
    export_reason: "Exported: VAT at 0%",
    exempt_reason: "Not subject to VAT (Article 5, Law on VAT)",
    reverse_charge: ReverseCharge::Always,
    simplified: None,
    accounts: &[],
};

pub fn number(raw: &str) -> Option<String> {
    let n: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits(raw).chars().any(|c| !c.is_ascii_digit()) {
        return None;
    }
    match n.len() {
        10 => Some(n),
        13 => Some(format!("{}-{}", &n[..10], &n[10..])),
        _ => None,
    }
}

fn gtgt01(f: &Figures, _company: &Doc) -> Vec<Line> {
    let input = f.purchases_tax(&["Standard", "Standard (Reduced to 8%)", "Reduced 5%", REVERSE_CHARGED]);
    let not_subject = f.sales_net(&[EXEMPT, OUT_OF_SCOPE]);
    let zero = f.sales_net(&[ZERO_RATED, EXPORT]);
    let five = f.sales_net(&["Reduced 5%"]);
    let ten = f.sales_net(&["Standard", "Standard (Reduced to 8%)"]);
    let output = f.sales_tax(&["Reduced 5%", "Standard", "Standard (Reduced to 8%)"]);
    vec![
        (
            "[23]",
            "Value of goods and services purchased",
            f.purchases_net(&["Standard", "Standard (Reduced to 8%)", "Reduced 5%", REVERSE_CHARGED, ZERO_RATED]),
        ),
        ("[24]", "VAT on goods and services purchased", input),
        ("[25]", "Deductible input VAT this period", input),
        ("[26]", "Goods and services sold not subject to VAT", not_subject),
        ("[29]", "Goods and services sold at 0%", zero),
        ("[30]", "Goods and services sold at 5%", five),
        ("[31]", "VAT on sales at 5%", f.sales_tax(&["Reduced 5%"])),
        ("[32]", "Goods and services sold at 10% (those reduced to 8% included)", ten),
        ("[32a]", "Of which reduced to 8%", f.sales_net(&["Standard (Reduced to 8%)"])),
        ("[33]", "VAT on sales at 10% and 8%", f.sales_tax(&["Standard", "Standard (Reduced to 8%)"])),
        ("[34]", "Total sales", not_subject + zero + five + ten),
        ("[35]", "Total output VAT", output),
        ("[36]", "VAT arising in the period (output less deductible input)", output - input),
    ]
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![std::sync::Arc::new(Return { name: "vn_01_gtgt", label: "VAT Declaration (01/GTGT)", country: &COUNTRY, boxes: gtgt01 })]
}

crate::app!("vn");

#[cfg(test)]
mod tests {
    #[test]
    fn rates_and_numbers() {
        use rust_decimal::Decimal;
        let r = |on| super::COUNTRY.rate("Standard (Reduced to 8%)", on).map(|r| r.1);
        assert_eq!((r("2023-03-01"), r("2026-12-31"), r("2027-01-01")), (Some(Decimal::from(10)), Some(Decimal::from(8)), Some(Decimal::from(10))));
        assert_eq!(
            (super::number("0101234567"), super::number("0101234567-001"), super::number("12345")),
            (Some("0101234567".into()), Some("0101234567-001".into()), None)
        );
    }
}
