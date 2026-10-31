//! China: VAT (the VAT Law of the PRC; State Taxation Administration) for general taxpayers at
//! 13% (goods, processing, repairs, leasing of movables), 9% (agricultural products, utilities,
//! transport, postal, basic telecoms, construction, immovable property) and 6% (modern services,
//! financial services, value-added telecoms, intangibles), each since 1 April 2019; exports at 0%;
//! exempt supplies; VAT withheld on services from abroad and credited. Small-scale taxpayers (a
//! company setting) levy 3%, cut to 1% from 2023 to the end of 2027, and credit no input tax. The
//! unified social credit code's check character is checked (GB 32100-2015).

use crate::engine::{Country, EXEMPT, EXPORT, Figures, Line, REVERSE_CHARGED, Rate, Return, ReverseCharge, Row, ZERO_RATED};
use ferrum_kernel::Doc;

pub static COUNTRY: Country = Country {
    package: "cn",
    country: "CN",
    rows: &[Row { label: "VAT", sales: "2131", purchases: Some("1151"), self_assessed: None }],
    rates: &[
        Rate { category: "13%", row: 0, from: "2017-07-01", rate: "17" },
        Rate { category: "13%", row: 0, from: "2018-05-01", rate: "16" },
        Rate { category: "13%", row: 0, from: "2019-04-01", rate: "13" },
        Rate { category: "9%", row: 0, from: "2017-07-01", rate: "11" },
        Rate { category: "9%", row: 0, from: "2018-05-01", rate: "10" },
        Rate { category: "9%", row: 0, from: "2019-04-01", rate: "9" },
        Rate { category: "6%", row: 0, from: "2016-05-01", rate: "6" },
        Rate { category: "Small-scale", row: 0, from: "2014-07-01", rate: "3" },
        Rate { category: "Small-scale", row: 0, from: "2023-01-01", rate: "1" },
        Rate { category: "Small-scale", row: 0, from: "2028-01-01", rate: "3" },
    ],
    category_field: "cn_vat_category",
    default_category: "13%",
    kind_field: "cn_kind",
    number_field: "cn_tax_id",
    number,
    number_hint: "a unified social credit code: 18 characters whose last checks the rest",
    export_reason: "Export: VAT at 0%",
    exempt_reason: "VAT-exempt",
    reverse_charge: ReverseCharge::Always,
    simplified: Some(("cn_small_scale", "Small-scale")),
    accounts: &[],
};

const CHARS: &str = "0123456789ABCDEFGHJKLMNPQRTUWXY";

/// GB 32100-2015: 18 characters of its 31, the last checking the first 17 (weights 3^i mod 31).
pub fn number(raw: &str) -> Option<String> {
    let n: String = raw.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_uppercase();
    if n.chars().count() != 18 {
        return None;
    }
    let idx: Vec<usize> = n.chars().map(|c| CHARS.find(c)).collect::<Option<Vec<usize>>>()?;
    let weights = [1, 3, 9, 27, 19, 26, 16, 17, 20, 29, 25, 13, 8, 24, 10, 30, 28];
    let sum: usize = idx[..17].iter().zip(weights).map(|(a, w)| a * w).sum();
    let check = (31 - sum % 31) % 31;
    (check == idx[17]).then_some(n)
}

fn vat_return(f: &Figures, _company: &Doc) -> Vec<Line> {
    let output = f.sales_tax(&["13%", "9%", "6%", "Small-scale"]);
    let input = f.purchases_tax(&["13%", "9%", "6%", REVERSE_CHARGED]);
    vec![
        ("13", "Sales at 13%", f.sales_net(&["13%"])),
        ("9", "Sales at 9%", f.sales_net(&["9%"])),
        ("6", "Sales at 6%", f.sales_net(&["6%"])),
        ("S", "Sales under the simplified method (small-scale)", f.sales_net(&["Small-scale"])),
        ("EXP", "Exports (0%)", f.sales_net(&[EXPORT, ZERO_RATED])),
        ("EX", "VAT-exempt sales", f.sales_net(&[EXEMPT])),
        ("OUT", "Output tax", output),
        ("IN", "Input tax credited (services from abroad included)", input),
        ("DUE", "VAT payable (input tax carried forward when negative)", output - input),
    ]
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![std::sync::Arc::new(Return { name: "cn_vat_return", label: "VAT Return (Summary)", country: &COUNTRY, boxes: vat_return })]
}

crate::app!("cn");

#[cfg(test)]
mod tests {
    #[test]
    fn numbers_and_rates() {
        use rust_decimal::Decimal;
        assert_eq!(super::number("91350100M000100Y43"), Some("91350100M000100Y43".into()));
        assert_eq!(super::number("911100001000013377"), None);
        assert_eq!(super::COUNTRY.rate("Small-scale", "2026-10-31").map(|r| r.1), Some(Decimal::ONE));
        assert_eq!(super::COUNTRY.rate("13%", "2019-03-31").map(|r| r.1), Some(Decimal::from(16)));
    }
}
