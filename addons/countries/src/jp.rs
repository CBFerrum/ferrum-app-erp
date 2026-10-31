//! Japan: consumption tax (National Tax Agency) at 10% (national 7.8%, local 2.2%) and the
//! reduced 8% (national 6.24%, local 1.76%) for food and drink other than alcohol and dining out
//! and for subscribed newspapers, since 1 October 2019 (8% from April 2014, 5% from April 1997,
//! 3% from April 1989). Each rate is its own row, so an invoice works out its tax once per rate,
//! as the qualified invoice system asks; the registration number (T and 13 digits) goes on the
//! company. Export sales are exempt (zero-rated); non-taxable sales (land, interest, rent of
//! housing) are the Exempt category; the reverse charge on specified services bought from abroad,
//! for businesses whose taxable sales ratio is under 95% (a company setting). The return
//! summarises the consumption tax return by rate, with the national and local parts.
//!
//! The cut to the food rate from 1 April 2027 to 31 March 2029 is enacted but its rate is not yet
//! in this table: until it is, set the items' own tax rate.

use crate::engine::{Country, EXEMPT, EXPORT, Figures, Line, REVERSE_CHARGED, Rate, Return, ReverseCharge, Row, ZERO_RATED};
use ferrum_kernel::Doc;
use rust_decimal::Decimal;

pub static COUNTRY: Country = Country {
    package: "jp",
    country: "JP",
    rows: &[
        Row { label: "Consumption Tax 10%", sales: "2131", purchases: Some("1151"), self_assessed: None },
        Row { label: "Consumption Tax 8% (Reduced)", sales: "2133", purchases: Some("1153"), self_assessed: None },
    ],
    rates: &[
        Rate { category: "Standard", row: 0, from: "1989-04-01", rate: "3" },
        Rate { category: "Standard", row: 0, from: "1997-04-01", rate: "5" },
        Rate { category: "Standard", row: 0, from: "2014-04-01", rate: "8" },
        Rate { category: "Standard", row: 0, from: "2019-10-01", rate: "10" },
        Rate { category: "Reduced", row: 1, from: "2019-10-01", rate: "8" },
    ],
    category_field: "jp_tax_category",
    default_category: "Standard",
    kind_field: "jp_kind",
    number_field: "jp_registration_number",
    number,
    number_hint: "a qualified invoice issuer's registration number: T and 13 digits",
    export_reason: "Export sale: exempt from consumption tax (zero-rated)",
    exempt_reason: "Non-taxable sale",
    reverse_charge: ReverseCharge::When("jp_reverse_charge"),
    simplified: None,
    accounts: &[("2133", "Output Tax (Reduced Rate)", "Duties and Taxes", "Liability"), ("1153", "Input Tax (Reduced Rate)", "Tax Assets", "Asset")],
};

/// "T" and 13 digits; a corporation's are its corporate number, whose first digit checks the rest.
pub fn number(raw: &str) -> Option<String> {
    let n: String = raw.chars().filter(|c| !matches!(c, ' ' | '-')).collect::<String>().to_uppercase();
    let d = n.strip_prefix('T')?;
    (d.len() == 13 && d.bytes().all(|c| c.is_ascii_digit())).then(|| format!("T{d}"))
}

/// Whether a corporate number's check digit (its first) holds.
pub fn corporate(d: &str) -> bool {
    let v: Vec<u32> = d.bytes().map(|c| u32::from(c - b'0')).collect();
    v.len() == 13 && v[0] == 9 - v[1..].iter().rev().enumerate().map(|(i, x)| x * if i % 2 == 0 { 1 } else { 2 }).sum::<u32>() % 9
}

fn ct_return(f: &Figures, _company: &Doc) -> Vec<Line> {
    let tax10 = f.sales_tax(&["Standard"]) + f.purchases_tax(&[REVERSE_CHARGED]);
    let tax8 = f.sales_tax(&["Reduced"]);
    let input10 = f.purchases_tax(&["Standard", REVERSE_CHARGED]);
    let input8 = f.purchases_tax(&["Reduced"]);
    let national = |t10: Decimal, t8: Decimal| (t10 * Decimal::new(78, 2) + t8 * Decimal::new(78, 2)).round_dp(0);
    vec![
        ("S10", "Taxable sales at 10%", f.sales_net(&["Standard"])),
        ("S8", "Taxable sales at 8% (reduced)", f.sales_net(&["Reduced"])),
        ("EXP", "Export (exempt) sales", f.sales_net(&[EXPORT, ZERO_RATED])),
        ("NT", "Non-taxable sales", f.sales_net(&[EXEMPT])),
        ("RC", "Specified services under the reverse charge", f.purchases_net(&[REVERSE_CHARGED])),
        ("T10", "Consumption tax on sales at 10%", tax10),
        ("T8", "Consumption tax on sales at 8%", tax8),
        ("I10", "Consumption tax on purchases at 10%", input10),
        ("I8", "Consumption tax on purchases at 8%", input8),
        ("N", "National consumption tax due (7.8/10 and 6.24/8 of the net)", national(tax10 - input10, tax8 - input8)),
        ("L", "Local consumption tax due (2.2/10 and 1.76/8 of the net)", (tax10 - input10 + tax8 - input8) - national(tax10 - input10, tax8 - input8)),
        ("NET", "Consumption tax due", tax10 + tax8 - input10 - input8),
    ]
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![std::sync::Arc::new(Return { name: "jp_consumption_tax", label: "Consumption Tax Return (Summary)", country: &COUNTRY, boxes: ct_return })]
}

crate::app!("jp");

#[cfg(test)]
mod tests {
    #[test]
    fn numbers_and_rates() {
        use rust_decimal::Decimal;
        assert_eq!(super::number("t7000-0120-50002"), Some("T7000012050002".into()));
        assert_eq!((super::number("7000012050002"), super::number("T700001205000")), (None, None));
        assert!(super::corporate("7000012050002") && super::corporate("1180301018771") && !super::corporate("1180301018772"));
        assert_eq!(super::COUNTRY.rate("Reduced", "2026-10-31"), Some((1, Decimal::from(8))));
        assert_eq!(super::COUNTRY.rate("Standard", "2019-09-30").map(|r| r.1), Some(Decimal::from(8)));
    }
}
