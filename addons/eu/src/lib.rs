//! The European Union: ERP's add-on for the VAT rules member states share (Ferrum
//! docs/plan/localization.md, L1). Which country's VAT each sale and purchase takes and how
//! ([`vat::Eu`], on ERP's localization seam), VAT numbers checked in VIES ([`vies`]), the member
//! states' rates from the Commission's TEDB ([`tedb`]), EN 16931 e-invoices in Peppol BIS Billing
//! 3.0's UBL ([`ubl`]), and the EC sales list and the OSS return ([`reports`]). Its package adds
//! the `eu_` fields; it acts only for companies in a member state.

pub mod reports;
pub mod tedb;
pub mod ubl;
pub mod vat;
pub mod vies;

pub(crate) use ferrum_domain::util;

use ferrum_kernel::{HookRegistry, Kernel, Result};
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// The EU's `app.yaml`.
pub const MANIFEST: &str = include_str!("../app.yaml");

/// The EU's package (`eu`): what switches its rules on in a workspace.
pub const PACKAGE: &str = "eu";

/// The EU's metadata package, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers the EU's rules: VAT numbers on companies, customers and suppliers, rate suggestions
/// on items, e-invoices on sales invoices, the VIES and TEDB connectors, and its localization for
/// ERP's taxes and company set-up. Its document hooks run only where its package is installed.
pub fn register(h: &mut HookRegistry) {
    ferrum_app_erp::localization::add(Arc::new(vat::Eu));
    h.scoped(&[PACKAGE.to_string()], |h| {
        h.register("Company", Arc::new(vat::VatNumberHook))
            .register("Customer", Arc::new(vat::VatNumberHook))
            .register("Supplier", Arc::new(vat::VatNumberHook))
            .register("Item", Arc::new(vat::VatNumberHook))
            .register("SalesInvoice", Arc::new(ubl::EInvoiceHook));
    });
    h.add_event_sink(Arc::new(vies::Vies::default()));
    h.add_event_sink(Arc::new(tedb::Tedb::default()));
}

/// The EU's reports: the EC sales list and the OSS return.
pub fn reports() -> Vec<Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![Arc::new(reports::EuReports)]
}

/// Periodic upkeep: none (VIES and TEDB requests go through the outbox).
pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_manifest_is_the_eu() {
        assert!(super::MANIFEST.contains("id: eu") && super::MANIFEST.contains("parent: erp"), "ERP's add-on");
        assert!(super::MANIFEST.contains("packages: [eu]"), "packing a bundle takes the packages app.yaml lists");
        let pkgs = super::packages().expect("the package parses");
        assert_eq!(pkgs.iter().map(|p| p.name()).collect::<Vec<_>>(), ["eu"]);
    }

    /// Its package loads with ERP's and the shared ones, and on its own with what it needs.
    #[test]
    fn the_package_fits_erp() {
        let mut all = ferrum_domain::packages().expect("the shared packages");
        all.extend(ferrum_app_erp::packages().expect("ERP's packages"));
        all.extend(super::packages().expect("the EU's package"));
        let catalog = ferrum_meta::Catalog::from_packages(all).expect("they validate together");
        assert_eq!(catalog.unknown_form_fields(), Vec::<String>::new(), "forms place fields nothing defines");
        let schema = ferrum_meta::effective::build(&catalog, &[super::PACKAGE.to_string()], &[], 1).unwrap_or_else(|e| panic!("{e}"));
        assert!(schema.warnings.is_empty(), "{:?}", schema.warnings);
        for e in ["EuVatRate", "SalesInvoice", "Item"] {
            assert!(schema.entity(e).is_some(), "{e}");
        }
        assert!(schema.entity("SalesInvoice").unwrap().field("eu_customer_vat").is_some());
    }

    /// The seeded VAT3 works out on ERP's return engine: a sale, an intra-Community acquisition
    /// under reverse charge and a domestic purchase.
    #[test]
    fn the_vat3_definition_computes() {
        use ferrum_app_erp::tax_return::{TaxRow, compute};
        use rust_decimal::Decimal;
        let pkg = super::packages().expect("the EU's package").remove(0);
        let seed = pkg.manifest.seed.iter().find(|s| s.entity == "TaxReturnDefinition").expect("seeded");
        let boxes: Vec<ferrum_kernel::Doc> = seed.records[0]["boxes"].as_array().unwrap().iter().map(|b| b.as_object().cloned().unwrap()).collect();
        let row = |entity: &'static str, category: &str, reverse: bool, base: i64, tax: i64| TaxRow {
            entity,
            voucher: entity,
            id: uuid::Uuid::nil(),
            number: None,
            date: chrono::NaiveDate::from_ymd_opt(2026, 1, 5).unwrap(),
            party: None,
            tax: "VAT".into(),
            description: "VAT".into(),
            category: category.into(),
            rate: Decimal::from(23),
            reverse_charge: reverse,
            credit_note: false,
            place: String::new(),
            base: Decimal::from(base),
            amount: Decimal::from(tax),
        };
        let ledger = [
            row("SalesInvoice", "Standard", false, 1000, 230),
            row("PurchaseInvoice", "Intra-Community", true, 500, 115),
            row("PurchaseInvoice", "Standard", false, 200, 46),
        ];
        let out = compute(&boxes, &ledger, &Default::default(), 2).expect("computes");
        let v = |c: &str| out.iter().find(|b| b["code"] == c).map(|b| b["value"].as_str().unwrap().parse::<Decimal>().unwrap()).unwrap();
        assert_eq!((v("T1"), v("T2"), v("T3"), v("T4"), v("E1"), v("E2")), (345.into(), 161.into(), 184.into(), 0.into(), 0.into(), 500.into()));
    }
}
