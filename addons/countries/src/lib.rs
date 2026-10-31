//! Countries whose VAT or GST works the common way, each its own app on ERP's localization seam
//! (Ferrum docs/plan/localization.md, L6 and L7), sharing one engine ([`engine`]): South East Asia
//! (Singapore, Malaysia, Indonesia, Thailand, the Philippines, Vietnam), the Gulf (Saudi Arabia,
//! the United Arab Emirates), Australia, New Zealand, Japan, Mexico and China.
//!
//! Each module is an app as Ferrum compiles them in (`MANIFEST`, `packages`, `register`,
//! `reports`, `maintenance`): its manifest is `apps/<code>.yaml` and its package `packages/<code>`.

pub mod engine;
pub mod pint;

pub mod ae;
pub mod au;
pub mod cn;
pub mod id;
pub mod jp;
pub mod mx;
pub mod my;
pub mod myinvois;
pub mod nz;
pub mod ph;
pub mod sa;
pub mod sg;
pub mod th;
pub mod vn;

pub(crate) use ferrum_domain::util;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// One country's package.
pub(crate) fn packages_of(name: &str) -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    let mine: Vec<(&str, &str, &str)> = EMBEDDED.iter().filter(|(p, _, _)| *p == name).copied().collect();
    ferrum_meta::catalog::load_embedded(&mine)
}

/// A country's app around its `COUNTRY` and its `reports_list()`: its manifest and package, its
/// rules on ERP's seam, its registration numbers' check, its returns, and, with its PINT
/// specialization, its Peppol e-invoices.
macro_rules! app {
    ($code:literal) => {
        crate::app!(@base $code, |_h: &mut ferrum_kernel::HookRegistry| {});
    };
    ($code:literal, hooks = $f:path) => {
        crate::app!(@base $code, |h: &mut ferrum_kernel::HookRegistry| $f(h));
    };
    ($code:literal, $pint:expr) => {
        crate::app!(@base $code, |h: &mut ferrum_kernel::HookRegistry| {
            h.register("SalesInvoice", std::sync::Arc::new(crate::pint::PintHook(&COUNTRY, &$pint)));
        });
    };
    (@base $code:literal, $extra:expr) => {
        /// The app's `app.yaml`.
        pub const MANIFEST: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/apps/", $code, ".yaml"));
        /// The app's package.
        pub const PACKAGE: &str = $code;

        pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
            crate::packages_of(PACKAGE)
        }

        pub fn register(h: &mut ferrum_kernel::HookRegistry) {
            ferrum_app_erp::localization::add(std::sync::Arc::new(crate::engine::National(&COUNTRY)));
            h.scoped(&[PACKAGE.to_string()], |h| {
                for entity in ["Company", "Customer", "Supplier"] {
                    h.register(entity, std::sync::Arc::new(crate::engine::NumberHook(&COUNTRY)));
                }
                ($extra)(h);
            });
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

/// The countries' rules and returns, for tests and tools.
pub fn all() -> Vec<(&'static str, &'static engine::Country)> {
    vec![
        ("ae", &ae::COUNTRY),
        ("au", &au::COUNTRY),
        ("cn", &cn::COUNTRY),
        ("id", &id::COUNTRY),
        ("jp", &jp::COUNTRY),
        ("mx", &mx::COUNTRY),
        ("my", &my::COUNTRY),
        ("nz", &nz::COUNTRY),
        ("ph", &ph::COUNTRY),
        ("sa", &sa::COUNTRY),
        ("sg", &sg::COUNTRY),
        ("th", &th::COUNTRY),
        ("vn", &vn::COUNTRY),
    ]
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_package_fits_erp_and_offers_its_categories() {
        for (code, country) in super::all() {
            let mine = super::packages_of(code).unwrap_or_else(|e| panic!("{code}: {e}"));
            assert_eq!(mine.len(), 1, "{code}");
            let mut all = ferrum_domain::packages().expect("the shared packages");
            all.extend(ferrum_app_erp::packages().expect("ERP's packages"));
            all.extend(mine);
            let catalog = ferrum_meta::Catalog::from_packages(all).unwrap_or_else(|e| panic!("{code}: {e}"));
            let schema = ferrum_meta::effective::build(&catalog, &[code.to_string()], &[], 1).unwrap_or_else(|e| panic!("{code}: {e}"));
            assert!(schema.warnings.is_empty(), "{code}: {:?}", schema.warnings);
            let item = schema.entity("Item").expect("Item");
            let field = item.field(country.category_field).unwrap_or_else(|| panic!("{code}: no {}", country.category_field));
            let options: Vec<String> = field.options.clone();
            assert_eq!(options, country.categories(), "{code}: the item's categories are the country's");
            assert!(options.iter().any(|o| o == country.default_category), "{code}: the default is offered");
            assert!(item.field(country.kind_field).is_some(), "{code}");
            for entity in ["Company", "Customer", "Supplier"] {
                assert!(schema.entity(entity).and_then(|e| e.field(country.number_field)).is_some(), "{code}: {entity}.{}", country.number_field);
            }
            if let super::engine::ReverseCharge::When(field) = country.reverse_charge {
                assert!(schema.entity("Company").and_then(|e| e.field(field)).is_some(), "{code}: Company.{field}");
            }
            if let Some((field, simple)) = country.simplified {
                assert!(schema.entity("Company").and_then(|e| e.field(field)).is_some(), "{code}: Company.{field}");
                assert!(country.rate(simple, "2026-10-31").is_some(), "{code}: {simple} has a rate");
            }
            assert!(country.rate(country.default_category, "2026-10-31").is_some() || country.default_category == super::engine::OUT_OF_SCOPE, "{code}");
        }
    }
}
