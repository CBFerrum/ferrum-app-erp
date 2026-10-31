//! The United Kingdom: ERP's add-on for UK VAT (Ferrum docs/plan/localization.md, L3). Which VAT
//! a UK company's sales and purchases take ([`vat::Uk`], on ERP's localization seam), the
//! construction industry's reverse charge, VAT registration numbers, and VAT returns worked out
//! from the books and filed through HMRC's Making Tax Digital API ([`mtd`]).

pub mod mtd;
pub mod vat;

pub(crate) use ferrum_domain::util;

use ferrum_kernel::{HookRegistry, Kernel, Result};
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// The UK's `app.yaml`.
pub const MANIFEST: &str = include_str!("../app.yaml");

/// The UK's package (`gb`).
pub const PACKAGE: &str = "gb";

pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers the UK's rules, VAT returns and the MTD connector with its HTTP endpoints
/// (`/api/app/gb/hmrc/callback`, `/api/app/gb/returns/{id}/file`).
pub fn register(h: &mut HookRegistry) {
    ferrum_app_erp::localization::add(Arc::new(vat::Uk));
    h.scoped(&[PACKAGE.to_string()], |h| {
        h.register("Company", Arc::new(vat::VrnHook))
            .register("Customer", Arc::new(vat::VrnHook))
            .register("Supplier", Arc::new(vat::VrnHook))
            .register("GbVatReturn", Arc::new(mtd::VatReturnHook));
    });
    h.add_event_sink(Arc::new(mtd::Mtd::default()));
    h.add_site(PACKAGE, Arc::new(mtd::HmrcSite));
}

/// The UK's reports: none apart from its VAT returns, which are documents.
pub fn reports() -> Vec<Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_package_fits_erp() {
        assert!(super::MANIFEST.contains("id: gb") && super::MANIFEST.contains("parent: erp"));
        let mut all = ferrum_domain::packages().expect("the shared packages");
        all.extend(ferrum_app_erp::packages().expect("ERP's packages"));
        all.extend(super::packages().expect("the UK's package"));
        let catalog = ferrum_meta::Catalog::from_packages(all).expect("they validate together");
        let schema = ferrum_meta::effective::build(&catalog, &[super::PACKAGE.to_string()], &[], 1).unwrap_or_else(|e| panic!("{e}"));
        assert!(schema.warnings.is_empty(), "{:?}", schema.warnings);
        assert!(schema.entity("GbVatReturn").is_some());
    }
}
