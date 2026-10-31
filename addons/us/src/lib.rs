//! The United States: ERP's add-on for sales and use tax (Ferrum docs/plan/localization.md, L4).
//! Which tax a US company's sales and purchases take ([`tax::Us`], on ERP's localization seam):
//! by destination (or origin, within origin-based states) at the ZIP code's rate from the states'
//! rate tables, where the company is registered, with exemption certificates, marketplace
//! facilitators and products' taxability by state; use tax on purchases; sales reported to Avalara
//! AvaTax ([`avatax`]); and the sales tax, economic nexus and Form 1099 reports ([`reports`]).

pub mod avatax;
pub mod reports;
pub mod tax;

pub(crate) use ferrum_domain::util;

use ferrum_kernel::{HookRegistry, Kernel, Result};
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// The US's `app.yaml`.
pub const MANIFEST: &str = include_str!("../app.yaml");

/// The US's package (`us`).
pub const PACKAGE: &str = "us";

pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers the US's rules, its rate tables' upkeep and the AvaTax connector.
pub fn register(h: &mut HookRegistry) {
    ferrum_app_erp::localization::add(Arc::new(tax::Us));
    h.scoped(&[PACKAGE.to_string()], |h| {
        for entity in ["UsTaxRate", "UsRateTable", "UsTaxability", "UsExemptionCertificate"] {
            h.register(entity, Arc::new(tax::RateHook));
        }
        h.register("SalesInvoice", Arc::new(avatax::InvoiceHook));
    });
    h.add_event_sink(Arc::new(avatax::AvaTax::default()));
}

/// Sales tax by jurisdiction, economic nexus and Forms 1099.
pub fn reports() -> Vec<Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![Arc::new(reports::UsReports)]
}

pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_package_fits_erp() {
        assert!(super::MANIFEST.contains("id: us") && super::MANIFEST.contains("parent: erp"));
        let mut all = ferrum_domain::packages().expect("the shared packages");
        all.extend(ferrum_app_erp::packages().expect("ERP's packages"));
        all.extend(super::packages().expect("the US's package"));
        let catalog = ferrum_meta::Catalog::from_packages(all).expect("they validate together");
        let schema = ferrum_meta::effective::build(&catalog, &[super::PACKAGE.to_string()], &[], 1).unwrap_or_else(|e| panic!("{e}"));
        assert!(schema.warnings.is_empty(), "{:?}", schema.warnings);
        for e in ["UsTaxRate", "UsRateTable", "UsTaxability", "UsExemptionCertificate"] {
            assert!(schema.entity(e).is_some(), "{e}");
        }
    }
}
