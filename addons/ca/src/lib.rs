//! Canada: ERP's add-on for GST/HST, PST and QST (Ferrum docs/plan/localization.md, L5). Which
//! taxes each line of a Canadian company's sale or purchase takes, by its province of supply
//! ([`tax::Canada`]), and the GST/HST and QST returns ([`tax::CaReports`]). Rates are the CRA's
//! (canada.ca, "Charge and collect the tax — Which rate to charge", 2026): GST 5%; HST 13% in
//! Ontario, 15% in New Brunswick, Newfoundland and Labrador and Prince Edward Island, 14% in Nova
//! Scotia from 1 April 2025 (15% before); PST 7% in British Columbia, RST 7% in Manitoba, PST 6%
//! in Saskatchewan; QST 9.975%.

pub mod tax;

pub(crate) use ferrum_domain::util;

use ferrum_kernel::{HookRegistry, Kernel, Result};
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

pub const MANIFEST: &str = include_str!("../app.yaml");
pub const PACKAGE: &str = "ca";

pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

pub fn register(_h: &mut HookRegistry) {
    ferrum_app_erp::localization::add(Arc::new(tax::Canada));
}

/// The GST/HST return and the QST return.
pub fn reports() -> Vec<Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![Arc::new(tax::CaReports)]
}

pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_package_fits_erp() {
        assert!(super::MANIFEST.contains("id: ca") && super::MANIFEST.contains("parent: erp"));
        let mut all = ferrum_domain::packages().expect("the shared packages");
        all.extend(ferrum_app_erp::packages().expect("ERP's packages"));
        all.extend(super::packages().expect("Canada's package"));
        let catalog = ferrum_meta::Catalog::from_packages(all).expect("they validate together");
        let schema = ferrum_meta::effective::build(&catalog, &[super::PACKAGE.to_string()], &[], 1).unwrap_or_else(|e| panic!("{e}"));
        assert!(schema.warnings.is_empty(), "{:?}", schema.warnings);
    }
}
