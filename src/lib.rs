//! ERP for Ferrum (docs/plan/erp-repo.md in the Ferrum repository): its manifest, its own interface
//! (`ui/dist`, built from `ui/src` and committed, which the Ferrum server serves at `/erp/` and on
//! the workspace's portal), its packages (accounting, inventory, buying, selling, manufacturing) and
//! its engine: the double-entry ledger with receivables and payables, inventory valuation, selling
//! and buying flows, stock entries, manufacturing and MRP, company set-up and the books' reports.
//!
//! Everything plugs into the kernel through `DocHook`s registered by entity name, each for the
//! workspaces with its package, so a workspace without accounting gets no chart of accounts. Other
//! apps (Point of Sale, Commerce, Assets, Field Service, HR's payroll, CRM's quotations) build on
//! the engine's modules; countries plug in through [`localization`] (India is the add-on
//! `ferrum-india`), and ERP's other add-ons are the other crates of this workspace.

pub mod accounting;
pub mod change;
pub mod credit;
pub mod deferred;
pub mod flow;
pub mod ledger;
pub mod localization;
pub mod manufacturing;
pub mod opening;
pub mod periods;
pub mod portal;
pub mod pricing;
pub mod promotion;
pub mod reports;
pub mod reservation;
pub mod setup;
pub mod stock;
pub mod stock_entry;
pub mod stored_value;
pub mod trade;
pub mod withholding;

pub(crate) use ferrum_domain::util;

use chrono::Datelike;
use ferrum_kernel::{HookRegistry, Kernel, Result};
use pricing::Side;
use std::sync::Arc;
use uuid::Uuid;

include!(concat!(env!("OUT_DIR"), "/ui.rs"));
include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// ERP's `app.yaml`.
pub const MANIFEST: &str = include_str!("../app.yaml");

/// ERP's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers ERP's document hooks and the portal's invoices, each for the workspaces with its
/// package. Call it before the hooks of apps that build on ERP's documents, so theirs see totals,
/// postings and open items.
pub fn register(h: &mut HookRegistry) {
    let only = |p: &str| vec![p.to_string()];
    // A customer's invoices on the workspace's portal ("Your invoices").
    h.add_site("selling", Arc::new(portal::SellingPortal));
    // A supplier's orders and bills on the workspace's portal ("Your orders").
    h.add_site("buying", Arc::new(portal::BuyingPortal));
    h.scoped(&only("accounting"), |h| {
        h.register("Company", Arc::new(setup::CompanyHook))
            .register("FiscalYear", Arc::new(setup::FiscalYearHook))
            .register("JournalEntry", Arc::new(accounting::JournalEntryHook))
            .register("PaymentEntry", Arc::new(accounting::PaymentEntryHook))
            .register("OpeningInvoice", Arc::new(opening::OpeningInvoiceHook))
            .register("StoredValueKind", Arc::new(stored_value::KindHook))
            .register("StoredValueAccount", Arc::new(stored_value::AccountHook));
    });
    h.scoped(&only("inventory"), |h| {
        h.register("StockEntry", Arc::new(stock_entry::StockEntryHook));
    });
    h.scoped(&only("manufacturing"), |h| {
        h.register("Bom", Arc::new(manufacturing::BomHook))
            .register("WorkOrder", Arc::new(manufacturing::WorkOrderHook))
            .register("MrpRun", Arc::new(manufacturing::MrpRunHook));
    });
    h.scoped(&only("selling"), |h| {
        h.register("PromotionCode", Arc::new(promotion::CodeHook)).register("Company", Arc::new(promotion::PriceLinesHook));
    });
    for (side, package, order, shipment, invoice) in [
        (Side::Selling, "selling", "SalesOrder", "DeliveryNote", "SalesInvoice"),
        (Side::Buying, "buying", "PurchaseOrder", "PurchaseReceipt", "PurchaseInvoice"),
    ] {
        h.scoped(&only(package), |h| {
            h.register(order, Arc::new(trade::OrderHook(side)))
                .register(shipment, Arc::new(trade::ShipmentHook(side)))
                .register(invoice, Arc::new(trade::InvoiceHook(side)));
        });
    }
}

/// ERP's reports: the books and stock (trial balance to purchase register).
pub fn reports() -> Vec<Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![Arc::new(reports::ErpReports)]
}

/// Periodic upkeep, safe to run on every replica at once: yearly journal partitions, deferred
/// lines recognised, journal entries reversed on their reversal day, and stored value warned of and
/// expired.
pub async fn maintenance(k: &Kernel) -> Result<()> {
    let year = chrono::Utc::now().year();
    let mut tx = k.db.system_tx().await?;
    let installed: bool = sqlx::query_scalar("select to_regproc('ferrum_ensure_journal_partitions') is not null").fetch_one(&mut *tx).await?;
    // One replica at a time; the others skip this round.
    let locked: bool = sqlx::query_scalar("select pg_try_advisory_xact_lock(hashtext('ferrum:journal_partitions'))").fetch_one(&mut *tx).await?;
    if installed && locked {
        let created: i32 = sqlx::query_scalar("select ferrum_ensure_journal_partitions($1, $2)").bind(year - 1).bind(year + 1).fetch_one(&mut *tx).await?;
        if created > 0 {
            tracing::info!(created, "journal partitions created");
        }
    }
    tx.commit().await?;
    // What deferred lines earned or used through the end of last month, in every workspace with
    // the books (a company's accountant may recognise later days themselves).
    let mut tx = k.db.system_tx().await?;
    let tenants: Vec<Uuid> = sqlx::query_scalar("select id from tenant where status = 'active' order by id").fetch_all(&mut *tx).await?;
    tx.commit().await?;
    let today = chrono::Utc::now().date_naive();
    let month_end = today.with_day(1).and_then(|first| first.pred_opt()).unwrap_or(today);
    for tenant in tenants {
        let schema = k.registry.schema(tenant).await?;
        if !schema.packages.iter().any(|p| p == "accounting") {
            continue;
        }
        let ctx = ferrum_kernel::Ctx::system(tenant, schema);
        let mut tx = k.db.tenant_tx(tenant).await?;
        match deferred::recognise(k, &ctx, &mut tx, None, month_end).await {
            Ok(_) => tx.commit().await?,
            Err(e) => tracing::warn!(%tenant, "deferred recognition: {e}"),
        }
        if let Err(e) = accounting::upkeep(k, &ctx).await {
            tracing::warn!(%tenant, "journal entry reversals: {e}");
        }
        if let Err(e) = stored_value::upkeep(k, &ctx, today).await {
            tracing::warn!(%tenant, "stored value expiry: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;

    #[test]
    fn the_manifest_is_valid() {
        let erp = ferrum_apps::AppManifest::parse(MANIFEST).expect("ERP's app.yaml");
        assert_eq!(erp.id, "erp");
        assert_eq!(erp.ui.as_ref().map(|u| u.dist.as_str()), Some("ui/dist"));
        // Packing a bundle takes the packages app.yaml lists, and they are this crate's.
        let embedded: std::collections::BTreeSet<&str> = EMBEDDED.iter().map(|(p, ..)| *p).collect();
        assert_eq!(erp.packages, ["accounting", "inventory", "buying", "selling", "manufacturing"]);
        assert!(erp.packages.iter().all(|p| embedded.contains(p.as_str())) && embedded.len() == erp.packages.len());
    }

    /// The interface the server registers passes the platform's rules for serving one.
    #[test]
    fn the_interface_is_servable() {
        let files: BTreeMap<String, Vec<u8>> = UI.iter().map(|(p, b)| (p.to_string(), b.to_vec())).collect();
        assert!(files.contains_key("index.html"), "ui/dist is built and committed");
        ferrum_apps::ui::prepare("erp", files).expect("a servable interface");
    }

    /// ERP's packages load with the shared ones, all together and each alone with what it needs.
    #[test]
    fn the_packages_fit_the_shared_core() {
        let ours = ferrum_meta::catalog::load_embedded(EMBEDDED).expect("ERP's packages parse");
        let mut all = ferrum_domain::packages().expect("the shared packages");
        assert_eq!(all.iter().map(|p| p.name()).collect::<Vec<_>>(), ["contacts", "core", "parties", "products", "project_records"]);
        all.extend(ours.clone());
        let catalog = ferrum_meta::Catalog::from_packages(all).expect("they validate together");
        assert_eq!(catalog.unknown_form_fields(), Vec::<String>::new(), "forms place fields nothing defines");
        let names: Vec<String> = ours.iter().map(|p| p.name().to_string()).collect();
        let schema = ferrum_meta::effective::build(&catalog, &names, &[], 1).unwrap_or_else(|e| panic!("{e}"));
        assert!(schema.warnings.is_empty(), "{:?}", schema.warnings);
        assert!(schema.entity("SalesInvoice").is_some() && schema.entity("JournalEntry").is_some());
        for p in &names {
            ferrum_meta::effective::build(&catalog, std::slice::from_ref(p), &[], 1).unwrap_or_else(|e| panic!("package {p} alone: {e}"));
        }
    }

    /// A person's typed price on an item line is dropped only where the line's definition makes the
    /// price read-only on a condition the kernel cannot judge on a new row; a plainly editable price
    /// (another app's own lines, buying's) is kept, a plainly read-only one never reaches the engine,
    /// and the engine's own writes are never touched.
    #[test]
    fn typed_prices_are_dropped_only_where_the_definition_asks() {
        use pricing::{Side, guards_typed_price};
        let mut all = ferrum_domain::packages().expect("the shared packages");
        all.extend(ferrum_meta::catalog::load_embedded(EMBEDDED).expect("ERP's packages"));
        let catalog = ferrum_meta::Catalog::from_packages(all).expect("they validate together");
        let names: Vec<String> = ferrum_meta::catalog::load_embedded(EMBEDDED).unwrap().iter().map(|p| p.name().to_string()).collect();
        let schema = ferrum_meta::effective::build(&catalog, &names, &[], 1).unwrap();
        let line = |e: &str| schema.entity(e).cloned();
        let (conditional, editable, read_only) = (line("SalesInvoiceItem"), line("PurchaseInvoiceItem"), line("DeliveryNoteItem"));
        assert!(guards_typed_price(conditional.as_deref(), Side::Selling, false), "read-only on a condition: dropped on a new item line");
        assert!(guards_typed_price(line("SalesOrderItem").as_deref(), Side::Selling, false));
        assert!(!guards_typed_price(editable.as_deref(), Side::Selling, false), "plainly editable: kept");
        assert!(!guards_typed_price(read_only.as_deref(), Side::Selling, false), "plainly read-only: the kernel drops it");
        assert!(!guards_typed_price(conditional.as_deref(), Side::Selling, true), "the engine's own writes: kept");
        assert!(!guards_typed_price(conditional.as_deref(), Side::Buying, false));
    }

    /// Clusters record each package SQL file they applied with its checksum, and refuse to start
    /// when an applied file changes: these are the files from before ERP had its own repository.
    #[test]
    fn applied_sql_files_are_unchanged() {
        let applied = [
            ("accounting", "sql/001_ledger.sql", "8161471627f74d25ce6db3096f0cf84cbcaa505e44fcb8376bbe28e5a52a3bfd"),
            ("accounting", "sql/002_project_index.sql", "5e3263c4bb778c3e6d6a5c4d0af3d9fc5115e798bf0a5e1a420f582528b18426"),
            ("inventory", "sql/001_stock.sql", "5a773facc96d9b825b0d9b5b53733ebe0b0e1d8ce1a8cbfb5978a15ebe1545c8"),
        ];
        for (package, file, checksum) in applied {
            let (.., sql) = EMBEDDED.iter().find(|(p, f, _)| *p == package && *f == file).unwrap_or_else(|| panic!("{package}/{file} is gone"));
            assert_eq!(hex::encode(Sha256::digest(sql.as_bytes())), checksum, "{package}/{file} changed: add a new file instead");
        }
    }
}
