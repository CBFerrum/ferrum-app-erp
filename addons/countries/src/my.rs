//! Malaysia: Sales and Service Tax (Royal Malaysian Customs). Sales tax on taxable goods a
//! registered manufacturer sells or imports, at 10% unless the Sales Tax (Rate of Tax) Order puts
//! them at 5% or exempts them (the order of 1 July 2025 widened both); service tax on taxable
//! services, 8% since 1 March 2024 (6% before), 6% for food and beverage, telecommunications,
//! parking and logistics and, from 1 July 2025, construction, private healthcare and education;
//! imported taxable services accounted for by the business that acquires them. Neither is
//! recovered on purchases: each goes on its own row and account. Items are outside SST unless
//! their category says otherwise. The return summarises SST-02's figures.

use crate::engine::{Country, EXEMPT, EXPORT, Figures, Line, OUT_OF_SCOPE, REVERSE_CHARGED, Rate, Return, ReverseCharge, Row, ZERO_RATED};
use ferrum_kernel::Doc;

pub static COUNTRY: Country = Country {
    package: "my",
    country: "MY",
    rows: &[
        Row { label: "Sales Tax", sales: "2131", purchases: None, self_assessed: None },
        Row { label: "Service Tax", sales: "2133", purchases: None, self_assessed: Some("5291") },
    ],
    rates: &[
        Rate { category: "Sales Tax 10%", row: 0, from: "2018-09-01", rate: "10" },
        Rate { category: "Sales Tax 5%", row: 0, from: "2018-09-01", rate: "5" },
        Rate { category: "Service Tax", row: 1, from: "2018-09-01", rate: "6" },
        Rate { category: "Service Tax", row: 1, from: "2024-03-01", rate: "8" },
        Rate { category: "Service Tax 6%", row: 1, from: "2018-09-01", rate: "6" },
    ],
    category_field: "my_sst_category",
    default_category: OUT_OF_SCOPE,
    kind_field: "my_kind",
    number_field: "my_sst_number",
    number,
    number_hint: "an SST registration number, e.g. W10-1808-32000001",
    export_reason: "Exported: no sales or service tax",
    exempt_reason: "Exempted from sales tax (Sales Tax (Goods Exempted From Tax) Order)",
    reverse_charge: ReverseCharge::Always,
    simplified: None,
    accounts: &[("2133", "Service Tax Payable", "Duties and Taxes", "Liability"), ("5291", "Service Tax on Imported Services", "Indirect Expenses", "Expense")],
};

/// A letter, two digits, four digits and eight digits (W10-1808-32000001).
pub fn number(raw: &str) -> Option<String> {
    let n: String = raw.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase();
    let ok = n.len() == 15 && n.starts_with(|c: char| c.is_ascii_alphabetic()) && n[1..].bytes().all(|c| c.is_ascii_digit());
    ok.then(|| format!("{}-{}-{}", &n[..3], &n[3..7], &n[7..]))
}

fn sst02(f: &Figures, _company: &Doc) -> Vec<Line> {
    let sales_tax = f.sales_tax(&["Sales Tax 10%", "Sales Tax 5%"]);
    let service_tax = f.sales_tax(&["Service Tax", "Service Tax 6%"]);
    let imported = f.purchases_tax(&[REVERSE_CHARGED]);
    vec![
        ("ST10", "Taxable goods sold at 10%", f.sales_net(&["Sales Tax 10%"])),
        ("ST5", "Taxable goods sold at 5%", f.sales_net(&["Sales Tax 5%"])),
        ("ST", "Sales tax", sales_tax),
        ("SV8", "Taxable services at 8%", f.sales_net(&["Service Tax"])),
        ("SV6", "Taxable services at 6%", f.sales_net(&["Service Tax 6%"])),
        ("SV", "Service tax", service_tax),
        ("ITS", "Imported taxable services", f.purchases_net(&[REVERSE_CHARGED])),
        ("ITST", "Service tax on imported taxable services", imported),
        ("EX", "Exempted, zero-rated and exported", f.sales_net(&[EXEMPT, ZERO_RATED, EXPORT])),
        ("TOTAL", "Total tax payable", sales_tax + service_tax + imported),
    ]
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![std::sync::Arc::new(Return { name: "my_sst02", label: "SST-02 Return (Summary)", country: &COUNTRY, boxes: sst02 })]
}

crate::app!("my");

#[cfg(test)]
mod tests {
    #[test]
    fn rates_and_numbers() {
        use rust_decimal::Decimal;
        assert_eq!(super::COUNTRY.rate("Service Tax", "2024-02-29").map(|r| r.1), Some(Decimal::from(6)));
        assert_eq!(super::COUNTRY.rate("Service Tax", "2026-10-31"), Some((1, Decimal::from(8))));
        assert_eq!(super::number("w10 1808 32000001"), Some("W10-1808-32000001".into()));
        assert_eq!(super::number("W10-1808-3200000"), None);
    }
}
