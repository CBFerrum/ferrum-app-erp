//! ERP for Ferrum (docs/plan/erp-repo.md in the Ferrum repository): its manifest, the manifest of
//! its add-on for India, and its own interface (`ui/dist`, built from `ui/src` and committed), which
//! the Ferrum server compiles in and serves at `/erp/` and on the workspace's portal. Its packages
//! and their logic are still the platform's (`ferrum-domain`); its add-ons are the other crates of
//! this workspace.

/// ERP's `app.yaml`.
pub const MANIFEST: &str = include_str!("../app.yaml");

/// India's `app.yaml`: ERP's add-on for India (GST, e-invoices and e-way bills).
pub const INDIA_MANIFEST: &str = include_str!("../india/app.yaml");

include!(concat!(env!("OUT_DIR"), "/ui.rs"));

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn the_manifests_are_valid() {
        let erp = ferrum_apps::AppManifest::parse(MANIFEST).expect("ERP's app.yaml");
        assert_eq!(erp.id, "erp");
        assert_eq!(erp.ui.as_ref().map(|u| u.dist.as_str()), Some("ui/dist"));
        assert_eq!(erp.packages, ["accounting", "inventory", "buying", "selling", "manufacturing"]);
        let india = ferrum_apps::AppManifest::parse(INDIA_MANIFEST).expect("India's app.yaml");
        assert_eq!((india.id.as_str(), india.parent.as_deref()), ("india", Some("erp")));
    }

    /// The interface the server registers passes the platform's rules for serving one.
    #[test]
    fn the_interface_is_servable() {
        let files: BTreeMap<String, Vec<u8>> = UI.iter().map(|(p, b)| (p.to_string(), b.to_vec())).collect();
        assert!(files.contains_key("index.html"), "ui/dist is built and committed");
        ferrum_apps::ui::prepare("erp", files).expect("a servable interface");
    }
}
