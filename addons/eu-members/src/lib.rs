//! EU member states on the European Union's add-on (Ferrum docs/plan/localization.md, L2), each its
//! own app: Belgium (Peppol between businesses), Italy (FatturaPA through SDI), Poland (FA(3)
//! through KSeF) and Romania (CIUS-RO through ANAF's e-Factura), with what member states add to the EU's VAT at home ([`member`]: small
//! businesses' exemptions, the domestic reverse charge). Germany has its own crate (`ferrum-de`).
//!
//! Each module is an app as Ferrum compiles them in (`MANIFEST`, `packages`, `register`,
//! `reports`, `maintenance`): its manifest is `apps/<code>.yaml` and its package `packages/<code>`.

pub mod member;

pub mod be;
pub mod it;
pub mod pl;
pub mod ro;

pub(crate) use ferrum_domain::util;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// One member state's package.
pub(crate) fn packages_of(name: &str) -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    let mine: Vec<(&str, &str, &str)> = EMBEDDED.iter().filter(|(p, _, _)| *p == name).copied().collect();
    ferrum_meta::catalog::load_embedded(&mine)
}

/// A member state's app around its `rules()`, `hooks()` and `reports_list()`.
macro_rules! app {
    ($code:literal) => {
        /// The app's `app.yaml`.
        pub const MANIFEST: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/apps/", $code, ".yaml"));
        /// The app's package.
        pub const PACKAGE: &str = $code;

        pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
            crate::packages_of(PACKAGE)
        }

        /// Registers the member state's rules after the EU's, which they adjust, and its hooks.
        pub fn register(h: &mut ferrum_kernel::HookRegistry) {
            ferrum_app_erp::localization::add(std::sync::Arc::new(rules()));
            h.scoped(&[PACKAGE.to_string()], hooks);
        }

        pub fn reports() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
            reports_list()
        }

        pub async fn maintenance(_k: &ferrum_kernel::Kernel) -> ferrum_kernel::Result<()> {
            Ok(())
        }
    };
}
pub(crate) use app;

#[cfg(test)]
mod tests {
    #[test]
    fn every_package_fits_the_eu_and_erp() {
        for code in ["be", "it", "pl", "ro"] {
            let mine = super::packages_of(code).unwrap_or_else(|e| panic!("{code}: {e}"));
            assert_eq!(mine.len(), 1, "{code}");
            let mut all = ferrum_domain::packages().expect("the shared packages");
            all.extend(ferrum_app_erp::packages().expect("ERP's packages"));
            all.extend(ferrum_eu::packages().expect("the EU's package"));
            all.extend(mine);
            let catalog = ferrum_meta::Catalog::from_packages(all).unwrap_or_else(|e| panic!("{code}: {e}"));
            let schema = ferrum_meta::effective::build(&catalog, &["eu".to_string(), code.to_string()], &[], 1).unwrap_or_else(|e| panic!("{code}: {e}"));
            assert!(schema.warnings.is_empty(), "{code}: {:?}", schema.warnings);
        }
        assert!(super::it::MANIFEST.contains("id: it") && super::pl::MANIFEST.contains("eu:"));
    }
}
