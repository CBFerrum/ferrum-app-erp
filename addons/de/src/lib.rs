//! Germany: ERP's add-on on the European Union's (Ferrum docs/plan/localization.md, L2). Small
//! businesses invoice without VAT (§ 19 UStG); a business customer in Germany owes the VAT on
//! construction work and the like (§ 13b UStG); and invoices go out as XRechnung, the CIUS of
//! EN 16931 that Germany's e-invoicing mandate takes, checked against KoSIT's rules first.

pub mod vat;
pub mod xrechnung;

pub(crate) use ferrum_domain::util;

use ferrum_kernel::{HookRegistry, Kernel, Result};
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// Germany's `app.yaml`.
pub const MANIFEST: &str = include_str!("../app.yaml");

/// Germany's package (`de`): what switches its rules on in a workspace.
pub const PACKAGE: &str = "de";

/// Germany's metadata package, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers Germany's rules (after the EU's, whose taxes it adjusts) and the XRechnung action.
pub fn register(h: &mut HookRegistry) {
    ferrum_app_erp::localization::add(Arc::new(vat::Germany));
    h.scoped(&[PACKAGE.to_string()], |h| {
        h.register("SalesInvoice", Arc::new(xrechnung::XRechnungHook));
    });
}

/// Germany's reports (none of its own yet: the EU's EC sales list serves the ZM).
pub fn reports() -> Vec<Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_package_fits_the_eus() {
        assert!(super::MANIFEST.contains("id: de") && super::MANIFEST.contains("parent: erp"));
        let mut all = ferrum_domain::packages().expect("the shared packages");
        all.extend(ferrum_app_erp::packages().expect("ERP's packages"));
        all.extend(ferrum_eu::packages().expect("the EU's package"));
        all.extend(super::packages().expect("Germany's package"));
        let catalog = ferrum_meta::Catalog::from_packages(all).expect("they validate together");
        let schema = ferrum_meta::effective::build(&catalog, &[super::PACKAGE.to_string()], &[], 1).unwrap_or_else(|e| panic!("{e}"));
        assert!(schema.warnings.is_empty(), "{:?}", schema.warnings);
        assert!(schema.entity("Customer").unwrap().field("de_leitweg_id").is_some());
    }
}
