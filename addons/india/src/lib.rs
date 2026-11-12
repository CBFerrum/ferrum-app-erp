//! India: ERP's add-on for GST (GSTIN validation, place of supply, the intra-state, inter-state and
//! export templates, HSN/SAC codes), e-invoices and e-way bills through the IRP, and a company's
//! GST accounts and templates. Its rules reach ERP's documents, and every app's priced ones,
//! through ERP's localization seam ([`gst::India`]); its package adds the `in_` fields.

pub mod einvoice;
pub mod gst;
pub mod irp;

pub use gst::{gstin_check_char, state_name, validate_gstin};

pub(crate) use ferrum_domain::util;

use ferrum_kernel::{HookRegistry, Kernel, Result};
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// India's `app.yaml`.
pub const MANIFEST: &str = include_str!("../app.yaml");

/// India's package (`india`): what switches its rules on in a workspace.
pub const PACKAGE: &str = "india";

/// India's metadata package, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers India's rules: GSTINs on companies and parties, e-invoicing on sales invoices, the
/// IRP connector, and its localization for ERP's taxes and company set-up. Its document hooks run
/// only where its package is installed.
pub fn register(h: &mut HookRegistry) {
    ferrum_app_erp::localization::add(Arc::new(gst::India));
    h.scoped(&[PACKAGE.to_string()], |h| {
        h.register("Company", Arc::new(gst::PartyGstHook))
            .register("Customer", Arc::new(gst::PartyGstHook))
            .register("Supplier", Arc::new(gst::PartyGstHook))
            .register("SalesInvoice", Arc::new(einvoice::EInvoiceHook));
    });
    h.add_event_sink(Arc::new(irp::IrpConnector::default()));
}

/// India's reports: none of its own; its GST returns are tax return definitions it seeds (docs/india-returns.md).
pub fn reports() -> Vec<Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

/// Periodic upkeep: none (IRP requests go through the outbox).
pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_manifest_is_india() {
        assert!(super::MANIFEST.contains("id: india") && super::MANIFEST.contains("parent: erp"), "ERP's add-on");
        assert!(super::MANIFEST.contains("packages: [india]"), "packing a bundle takes the packages app.yaml lists");
        assert!(super::EMBEDDED.iter().any(|(p, f, _)| *p == super::PACKAGE && *f == "package.yaml"));
    }

    /// The seeded returns work out on ERP's return engine: an intra-state sale, an inter-state sale, an export, an
    /// exempt sale, a credit note, a purchase with input credit and one under reverse charge.
    #[test]
    fn the_gst_return_definitions_compute() {
        use ferrum_app_erp::tax_return::{TaxRow, compute};
        use rust_decimal::Decimal;
        let pkg = super::packages().expect("India's package").remove(0);
        let seed = pkg.manifest.seed.iter().find(|s| s.entity == "TaxReturnDefinition").expect("seeded");
        let row = |entity: &'static str, tax: &str, category: &str, reverse: bool, credit: bool, base: i64, amount: i64| TaxRow {
            entity,
            voucher: entity,
            id: uuid::Uuid::nil(),
            number: None,
            date: chrono::NaiveDate::from_ymd_opt(2026, 10, 5).unwrap(),
            party: None,
            tax: tax.into(),
            description: tax.into(),
            category: category.into(),
            rate: Decimal::from(9),
            reverse_charge: reverse,
            credit_note: credit,
            place: String::new(),
            base: Decimal::from(base),
            amount: Decimal::from(amount),
        };
        let ledger = [
            row("SalesInvoice", "CGST Output", "Standard", false, false, 1000, 90),
            row("SalesInvoice", "SGST Output", "Standard", false, false, 1000, 90),
            row("SalesInvoice", "IGST Output", "Standard", false, false, 2000, 360),
            row("SalesInvoice", "IGST Output", "Export", false, false, 500, 0),
            row("SalesInvoice", "IGST Output", "Exempt", false, false, 300, 0),
            row("SalesInvoice", "IGST Output", "Standard", false, true, -100, -18),
            row("PurchaseInvoice", "IGST Input", "Standard", false, false, 800, 144),
            row("PurchaseInvoice", "CGST Input", "Reverse Charge", true, false, 200, 18),
            row("PurchaseInvoice", "SGST Input", "Reverse Charge", true, false, 200, 18),
        ];
        let boxes = |name: &str| -> Vec<ferrum_kernel::Doc> {
            let rec = seed.records.iter().find(|r| r["definition_name"] == name).expect(name);
            rec["boxes"].as_array().unwrap().iter().map(|b| b.as_object().cloned().unwrap()).collect()
        };
        let out = compute(&boxes("India GSTR-3B"), &ledger, &Default::default(), 2).expect("GSTR-3B computes");
        let v = |out: &[ferrum_kernel::Doc], c: &str| {
            out.iter().find(|b| b["code"] == c).map(|b| b["value"].as_str().unwrap().parse::<Decimal>().unwrap()).unwrap()
        };
        // Taxable: 1000 intra + (2000 + 500 + 300 - 100) inter - 500 export - 300 exempt.
        assert_eq!(v(&out, "B31A_TV"), Decimal::from(2900));
        assert_eq!((v(&out, "B31A_IGST"), v(&out, "B31B_TV"), v(&out, "B31C_TV")), (Decimal::from(342), Decimal::from(500), Decimal::from(300)));
        assert_eq!((v(&out, "B31D_TV"), v(&out, "B4A3"), v(&out, "B4C")), (Decimal::from(200), Decimal::from(36), Decimal::from(180)));
        assert_eq!(v(&out, "B6_NET"), Decimal::from(342 + 90 + 90 + 36 - 180));
        let one = compute(&boxes("India GSTR-1"), &ledger, &Default::default(), 2).expect("GSTR-1 computes");
        assert_eq!((v(&one, "T6A"), v(&one, "T9B_TV"), v(&one, "TOTAL")), (Decimal::from(500), Decimal::from(-100), Decimal::from(3700)));
        let tds = pkg.manifest.seed.iter().find(|s| s.entity == "WithholdingCategory").expect("TDS categories");
        assert!(tds.records.iter().all(|r| r["account_number"] == "2132" && !r["rates"].as_array().unwrap().is_empty()));
    }
}
