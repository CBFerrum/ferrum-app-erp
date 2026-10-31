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

/// India's reports (none yet; GST returns are still to build).
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
}
