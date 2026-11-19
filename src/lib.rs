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
//!
//! "Today" is the workspace's day, never the UTC day: a request's from its context (`Ctx::today`), upkeep's from
//! each workspace's system context. Only moments (a posting's time, a token's expiry, when something was sent) are
//! taken from the UTC clock.

pub mod accounting;
pub mod agreement;
pub mod bad_debt;
pub mod change;
pub mod credit;
pub mod deferred;
pub mod dimensions;
pub mod flow;
pub mod instruments;
pub mod journal_template;
mod keys;
pub mod ledger;
pub mod localization;
pub mod manufacturing;
pub mod offered;
pub mod opening;
pub mod periods;
pub mod planning;
pub mod portal;
pub mod pricing;
pub mod promotion;
pub mod providers;
pub mod reports;
pub mod reservation;
pub mod scan;
pub mod series;
pub mod setup;
pub mod shipping;
pub mod stock;
pub mod stock_entry;
pub mod stored_value;
pub mod submission;
pub mod tax_return;
pub mod trade;
pub mod withholding;

pub(crate) use ferrum_domain::util;

use chrono::Datelike;
use ferrum_kernel::{HookRegistry, Kernel, Result};
use pricing::Side;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
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
/// postings and open items. Call it inside the platform's registration of the app, `hooks.app("erp", …)`: it takes its
/// own vault handle there, which reads its secret fields (`keys.rs`).
pub fn register(h: &mut HookRegistry) {
    let only = |p: &str| vec![p.to_string()];
    // A customer's invoices on the workspace's portal ("Your invoices"); ERP's handle is kept beside it.
    let selling: Arc<dyn ferrum_kernel::Site> = Arc::new(portal::SellingPortal);
    keys::remember(&selling, h.vault());
    h.add_site("selling", selling);
    // A supplier's orders and bills on the workspace's portal ("Your orders").
    h.add_site("buying", Arc::new(portal::BuyingPortal));
    // Carriers' tracking calls, which arrive at each carrier's connection.
    h.add_inbound_handler("erp", Arc::new(shipping::CarrierInbound));
    h.scoped(&only("accounting"), |h| {
        h.register("NumberSeries", Arc::new(series::SeriesRecordHook))
            .register("Company", Arc::new(setup::CompanyHook))
            .register("ChartTemplate", Arc::new(setup::ChartTemplateHook))
            .register("FiscalYear", Arc::new(setup::FiscalYearHook))
            .register("JournalEntry", Arc::new(accounting::JournalEntryHook))
            .register("PaymentEntry", Arc::new(accounting::PaymentEntryHook))
            .register("PaymentInstrument", Arc::new(instruments::InstrumentHook))
            .register("BadDebtWriteOff", Arc::new(bad_debt::WriteOffHook))
            .register("JournalTemplate", Arc::new(journal_template::TemplateHook))
            .register("TaxReturn", Arc::new(tax_return::ReturnHook))
            .register("OpeningInvoice", Arc::new(opening::OpeningInvoiceHook))
            .register("AccountingDimension", Arc::new(dimensions::DimensionHook))
            .register("StoredValueKind", Arc::new(stored_value::KindHook))
            .register("StoredValueAccount", Arc::new(stored_value::AccountHook))
            .register("TaxTemplate", Arc::new(pricing::ConditionHook))
            .register("Company", Arc::new(offered::LedgerOps));
        for numbered in ["JournalEntry", "PaymentEntry"] {
            h.register(numbered, Arc::new(series::SeriesHook));
        }
    });
    h.scoped(&only("inventory"), |h| {
        h.register("StockEntry", Arc::new(series::SeriesHook))
            .register("StockEntry", Arc::new(stock_entry::StockEntryHook))
            .register("CarrierConnection", Arc::new(shipping::CarrierHook))
            .register("CarrierShipment", Arc::new(shipping::ShipmentHook))
            .register("Company", Arc::new(scan::ScanHook));
    });
    h.scoped(&only("manufacturing"), |h| {
        h.register("Bom", Arc::new(manufacturing::BomHook))
            .register("WorkOrder", Arc::new(manufacturing::WorkOrderHook))
            .register("MrpRun", Arc::new(manufacturing::MrpRunHook));
    });
    h.scoped(&only("selling"), |h| {
        h.register("PromotionCode", Arc::new(promotion::CodeHook))
            .register("PricingRule", Arc::new(pricing::ConditionHook))
            .register("Company", Arc::new(promotion::PriceLinesHook))
            .register("Company", Arc::new(reservation::AvailabilityHook))
            .register("Company", Arc::new(offered::PartyOps));
    });
    for (side, package, agreement, order, shipment, invoice) in [
        (Side::Selling, "selling", "SalesAgreement", "SalesOrder", "DeliveryNote", "SalesInvoice"),
        (Side::Buying, "buying", "PurchaseAgreement", "PurchaseOrder", "PurchaseReceipt", "PurchaseInvoice"),
    ] {
        h.scoped(&only(package), |h| {
            h.register(order, Arc::new(series::SeriesHook))
                .register(shipment, Arc::new(series::SeriesHook))
                .register(invoice, Arc::new(series::SeriesHook))
                .register(order, Arc::new(trade::OrderHook(side)))
                .register(agreement, Arc::new(agreement::AgreementHook(side)))
                .register(shipment, Arc::new(trade::ShipmentHook(side)))
                .register(invoice, Arc::new(trade::InvoiceHook(side)));
        });
    }
}

/// ERP's reports: the books and stock (trial balance to purchase register).
pub fn reports() -> Vec<Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![Arc::new(reports::ErpReports)]
}

/// The day this replica last made sure of the journal's yearly partitions: they are the whole installation's, so once a
/// day is enough, from whichever workspace's upkeep comes first.
static PARTITIONED: AtomicI32 = AtomicI32::new(0);

/// The journal's yearly partitions, the year before to the year after UTC's (which holds every workspace's day): once a
/// day on this replica, one replica at a time (a transaction lock; a replica that does not get it tries again at the
/// next workspace's turn).
async fn partitions(k: &Kernel) -> Result<()> {
    let now = chrono::Utc::now();
    let day = now.date_naive().num_days_from_ce();
    if PARTITIONED.load(Ordering::Relaxed) == day {
        return Ok(());
    }
    let year = now.year();
    let mut tx = k.db.system_tx().await?;
    let installed: bool = sqlx::query_scalar("select to_regproc('ferrum_ensure_journal_partitions') is not null").fetch_one(&mut *tx).await?;
    let locked: bool = sqlx::query_scalar("select pg_try_advisory_xact_lock(hashtext('ferrum:journal_partitions'))").fetch_one(&mut *tx).await?;
    if installed && locked {
        let created: i32 = sqlx::query_scalar("select ferrum_ensure_journal_partitions($1, $2)").bind(year - 1).bind(year + 1).fetch_one(&mut *tx).await?;
        if created > 0 {
            tracing::info!(created, "journal partitions created");
        }
    }
    tx.commit().await?;
    if locked {
        PARTITIONED.store(day, Ordering::Relaxed);
    }
    Ok(())
}

/// One workspace's upkeep. The platform runs it for each workspace with one of ERP's packages, about once a minute, on
/// whichever replica is free, and may run it again (docs/app-contract.md, Upkeep): every step is safe to repeat. First
/// the journal's partitions ([`partitions`]); then, where the workspace keeps the books and on its own day: each
/// company's next fiscal year, deferred lines recognised through the end of last month, recurring journals, journal
/// entries reversed on their reversal day, instruments to deposit, stored value warned of and expired, and carriers'
/// tracking. A failing step is logged and stops no other; the first failure is returned.
pub async fn maintenance(k: &Kernel, tenant: Uuid) -> Result<()> {
    partitions(k).await?;
    let schema = k.registry.schema(tenant).await?;
    if !schema.packages.iter().any(|p| p == "accounting") {
        return Ok(());
    }
    let ctx = ferrum_kernel::Ctx::system(tenant, schema);
    let today = ctx.today();
    let month_end = today.with_day(1).and_then(|first| first.pred_opt()).unwrap_or(today);
    let mut first: Option<ferrum_kernel::Error> = None;
    let mut step = |name: &str, outcome: Result<()>| {
        if let Err(e) = outcome {
            tracing::warn!(%tenant, "{name}: {e}");
            first.get_or_insert(e);
        }
    };
    step("next fiscal years", setup::upkeep(k, &ctx, today).await);
    let deferred = async {
        let mut tx = k.db.tenant_tx(tenant).await?;
        let (_, left) = deferred::recognise(k, &ctx, &mut tx, None, month_end).await?;
        tx.commit().await?;
        for invoice in left {
            tracing::warn!(%tenant, "deferred recognition waits: {invoice}");
        }
        Ok(())
    };
    step("deferred recognition", deferred.await);
    step("recurring journals", journal_template::upkeep(k, &ctx, today).await.map(|_| ()));
    step("journal entry reversals", accounting::upkeep(k, &ctx).await);
    step("instruments to deposit", instruments::upkeep(k, &ctx, today).await.map(|_| ()));
    step("stored value expiry", stored_value::upkeep(k, &ctx, today).await.map(|_| ()));
    step("carrier tracking", shipping::upkeep(k, &ctx, 100).await.map(|_| ()));
    first.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
