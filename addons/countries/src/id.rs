//! Indonesia: PPN (value added tax). 12% since 1 January 2025, on a tax base of 11/12 of the price
//! for everything but luxury goods (PMK 131/2024: 11% in effect), the full 12% on luxury goods;
//! 11% from April 2022, 10% before. Exports at 0%; facilities that free supplies from PPN; PPN on
//! services used here from abroad, which the company pays itself. The return summarises SPT Masa
//! PPN. A 15-digit NPWP becomes the 16-digit one with a leading zero.

use crate::engine::{Country, EXEMPT, EXPORT, Figures, Line, REVERSE_CHARGED, Rate, Return, ReverseCharge, Row, ZERO_RATED, digits};
use ferrum_kernel::Doc;

pub static COUNTRY: Country = Country {
    package: "id",
    country: "ID",
    rows: &[Row { label: "PPN", sales: "2131", purchases: Some("1151"), self_assessed: None }],
    rates: &[
        Rate { category: "Standard", row: 0, from: "1985-01-01", rate: "10" },
        Rate { category: "Standard", row: 0, from: "2022-04-01", rate: "11" },
        Rate { category: "Luxury", row: 0, from: "1985-01-01", rate: "10" },
        Rate { category: "Luxury", row: 0, from: "2022-04-01", rate: "11" },
        Rate { category: "Luxury", row: 0, from: "2025-01-01", rate: "12" },
    ],
    category_field: "id_ppn_category",
    default_category: "Standard",
    kind_field: "id_kind",
    number_field: "id_npwp",
    number,
    number_hint: "an NPWP of 16 digits (or the 15 of the old form)",
    export_reason: "Ekspor: PPN 0%",
    exempt_reason: "PPN dibebaskan (fasilitas)",
    reverse_charge: ReverseCharge::Always,
    simplified: None,
    accounts: &[],
};

pub fn number(raw: &str) -> Option<String> {
    let n = digits(raw);
    if !n.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    match n.len() {
        16 => Some(n),
        15 => Some(format!("0{n}")),
        _ => None,
    }
}

fn spt_masa_ppn(f: &Figures, _company: &Doc) -> Vec<Line> {
    let output = f.sales_tax(&["Standard", "Luxury"]);
    let reverse = f.purchases_tax(&[REVERSE_CHARGED]);
    let input = f.purchases_tax(&["Standard", "Luxury", REVERSE_CHARGED]);
    vec![
        ("A1", "Ekspor (export)", f.sales_net(&[EXPORT, ZERO_RATED])),
        ("A2", "Penyerahan yang PPN-nya harus dipungut sendiri (taxable deliveries)", f.sales_net(&["Standard", "Luxury"])),
        ("A3", "Penyerahan yang dibebaskan dari pengenaan PPN (exempt deliveries)", f.sales_net(&[EXEMPT])),
        ("PK", "PPN Keluaran (output tax)", output),
        ("JLN", "PPN atas pemanfaatan JKP dari luar Daerah Pabean (services from abroad)", reverse),
        ("PM", "Pajak Masukan yang dapat dikreditkan (creditable input tax)", input),
        ("KB", "PPN kurang (lebih) bayar (payable, or overpaid when negative)", output + reverse - input),
    ]
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![std::sync::Arc::new(Return { name: "id_spt_masa_ppn", label: "SPT Masa PPN (Summary)", country: &COUNTRY, boxes: spt_masa_ppn })]
}

crate::app!("id");

#[cfg(test)]
mod tests {
    #[test]
    fn rates_and_numbers() {
        use rust_decimal::Decimal;
        assert_eq!(super::COUNTRY.rate("Standard", "2026-10-31").map(|r| r.1), Some(Decimal::from(11)), "12% on 11/12 of the price");
        assert_eq!(super::COUNTRY.rate("Luxury", "2026-10-31").map(|r| r.1), Some(Decimal::from(12)));
        assert_eq!(super::number("01.234.567.8-901.000"), Some("0012345678901000".into()));
        assert_eq!(super::number("12345"), None);
    }
}
