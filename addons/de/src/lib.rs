//! Germany: ERP's add-on on the European Union's (Ferrum docs/plan/localization.md, L2). Small
//! businesses invoice without VAT (§ 19 UStG); a business customer in Germany owes the VAT on
//! construction work and the like (§ 13b UStG, Article 226(11a) of Directive 2006/112/EC), as the
//! item says; both are the EU member states' rules ([`MEMBER`]), and each invoice says why. Invoices
//! go out as XRechnung, the CIUS of EN 16931 that Germany's e-invoicing mandate takes, checked
//! against KoSIT's rules first.

pub mod xrechnung;

pub(crate) use ferrum_domain::util;

use ferrum_eu_members::member::{Member, Rules};
use ferrum_kernel::{HookRegistry, Kernel, Result};
use std::sync::Arc;

/// The exemption reason a small business's lines carry (EN 16931 category E).
pub const SMALL_BUSINESS: &str = "Kleinunternehmerregelung (§ 19 UStG)";
pub const SMALL_BUSINESS_NOTE: &str = "Gemäß § 19 UStG wird keine Umsatzsteuer berechnet.";
pub const REVERSE_CHARGE_NOTE: &str = "Steuerschuldnerschaft des Leistungsempfängers (§ 13b UStG).";

/// Germany's rules on the EU's: § 19 UStG on the company, § 13b UStG on the item.
pub static MEMBER: Member = Member {
    package: PACKAGE,
    country: "DE",
    small: Some(("de_small_business", SMALL_BUSINESS, SMALL_BUSINESS_NOTE)),
    reverse: Some(("de_reverse_charge", REVERSE_CHARGE_NOTE)),
};

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
    ferrum_app_erp::localization::add(Arc::new(Rules(&MEMBER)));
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
    use super::*;
    use ferrum_eu::vat::Plan;
    use ferrum_eu_members::member::domestic_business;
    use ferrum_kernel::Doc;
    use serde_json::json;

    #[test]
    fn small_businesses_and_the_domestic_reverse_charge() {
        let flagged = json!({"de_reverse_charge": true}).as_object().cloned().unwrap();
        let plain = Doc::new();
        assert_eq!(MEMBER.treat(true, true, &flagged), Some(("Exempt", SMALL_BUSINESS)), "a small business charges no VAT at all");
        assert_eq!(MEMBER.treat(false, true, &flagged), Some(("Reverse Charge", "VATEX-EU-AE")));
        assert_eq!(MEMBER.treat(false, false, &flagged), None, "consumers pay the VAT");
        assert_eq!(MEMBER.treat(false, true, &plain), None);
        let home = Plan { home: "DE".into(), side: ferrum_app_erp::pricing::Side::Selling, other: Some("DE".into()), business: true, oss: false };
        let firm = json!({"eu_vat_number": "DE987654321"}).as_object().cloned().unwrap();
        assert!(domestic_business("DE", &home, Some(&firm)));
        assert!(!domestic_business("DE", &Plan { other: Some("FR".into()), ..home.clone() }, Some(&firm)), "abroad the EU's rules hold");
        assert!(!domestic_business("DE", &home, Some(&Doc::new())));
    }

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
