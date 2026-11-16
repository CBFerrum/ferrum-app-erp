//! Singapore: GST (Goods and Services Tax Act 1993; IRAS) at 9% since 1 January 2024 (8% in
//! 2023, 7% from July 2007); zero-rated exports of goods and international services (section
//! 21(3)); exempt supplies (Fourth Schedule: financial services, residential property); the
//! reverse charge on imported services for businesses not entitled to claim all their input tax
//! (RC Businesses, a company setting); and the GST F5 return.

use crate::engine::{Country, Rate, ReverseCharge, Row, digits};

pub static COUNTRY: Country = Country {
    package: "sg",
    country: "SG",
    rows: &[Row { label: "GST", sales: "2131", purchases: Some("1151"), self_assessed: None }],
    rates: &[
        Rate { category: "Standard", row: 0, from: "1994-04-01", rate: "3" },
        Rate { category: "Standard", row: 0, from: "2003-01-01", rate: "4" },
        Rate { category: "Standard", row: 0, from: "2004-01-01", rate: "5" },
        Rate { category: "Standard", row: 0, from: "2007-07-01", rate: "7" },
        Rate { category: "Standard", row: 0, from: "2023-01-01", rate: "8" },
        Rate { category: "Standard", row: 0, from: "2024-01-01", rate: "9" },
    ],
    category_field: "sg_gst_category",
    default_category: "Standard",
    kind_field: "sg_kind",
    number_field: "sg_gst_number",
    number,
    number_hint: "a GST registration number: the UEN (e.g. 201912345K) or the M-number (e.g. M90312345A)",
    export_reason: "Zero-rated: export of goods or international services (section 21(3), GST Act)",
    exempt_reason: "Exempt supply (Fourth Schedule, GST Act)",
    reverse_charge: ReverseCharge::When("sg_reverse_charge"),
    simplified: None,
    accounts: &[],
};

/// A UEN (nine or ten characters ending in a letter) or a GST M-number.
pub fn number(raw: &str) -> Option<String> {
    let n = digits(raw).to_uppercase();
    let ok = (n.len() == 9 || n.len() == 10) && n.chars().all(|c| c.is_ascii_alphanumeric()) && n.chars().last().is_some_and(|c| c.is_ascii_alphabetic());
    ok.then_some(n)
}

/// The return is a definition the package seeds, worked out by ERP's tax returns.
fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

crate::app!("sg", crate::pint::SG);

#[cfg(test)]
mod tests {
    #[test]
    fn rates_and_numbers() {
        use rust_decimal::Decimal;
        assert_eq!(super::COUNTRY.rate("Standard", "2023-12-31").map(|r| r.1), Some(Decimal::from(8)));
        assert_eq!(super::COUNTRY.rate("Standard", "2026-10-31").map(|r| r.1), Some(Decimal::from(9)));
        assert_eq!(
            (super::number("2019 12345k"), super::number("M9-0312345-A"), super::number("12345")),
            (Some("201912345K".into()), Some("M90312345A".into()), None)
        );
    }
}
