//! A workspace made by an older release, run by this one (`FERRUM_FINANCE_UPGRADE_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_finance_upgrade_test`; reset first). Every other test starts from
//! a fresh install of today's packages, so none saw what a server does when it starts on a workspace whose
//! administrator has not applied the update yet: the schema is synced to today's packages at boot, the workspace still
//! records the old versions and holds the old seeds, and the upkeeps and the packages' backfills run on it at once.
//!
//! The workspace is installed from the package YAML of an older commit of this repository (`OLD`: ERP 0.22.0, Finance
//! 0.5.0), documents are made on it, then this build boots on the same database. Before the update and after it,
//! every upkeep and backfill runs, and nothing may be logged at warning level, no job may fail, and the documents
//! stay as they were. The commit is read with `git` from the repository's history (CI fetches all of it).

#[path = "../../../tests/common/mod.rs"]
mod common;
use common::*;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::json;
use std::process::Command;
use std::sync::Arc;
use uuid::Uuid;

const OLD: &str = "51cefe5";

/// The packages under `root` (`packages` or `addons/finance/packages`) as `OLD` had them.
fn packages_at(root: &str) -> Result<Vec<ferrum_meta::Package>, String> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let git = |args: &[&str]| -> Result<Vec<u8>, String> {
        let out = Command::new("git").args(["-C", dir]).args(args).output().map_err(|e| e.to_string())?;
        if out.status.success() { Ok(out.stdout) } else { Err(String::from_utf8_lossy(&out.stderr).into_owned()) }
    };
    let listed = String::from_utf8(git(&["ls-tree", "-r", "--name-only", OLD, "--", root])?).unwrap();
    let mut files: Vec<(&'static str, &'static str, &'static str)> = Vec::new();
    for path in listed.lines() {
        let (pkg, rel) = path.strip_prefix(&format!("{root}/")).and_then(|r| r.split_once('/')).ok_or("a path outside a package")?;
        let text = String::from_utf8(git(&["show", &format!("{OLD}:{path}")])?).map_err(|e| e.to_string())?;
        let leak = |s: &str| -> &'static str { Box::leak(s.to_string().into_boxed_str()) };
        files.push((leak(pkg), leak(rel), Box::leak(text.into_boxed_str())));
    }
    ferrum_meta::catalog::load_embedded(&files).map_err(|e| e.to_string())
}

fn hooks() -> HookRegistry {
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_finance::register(&mut hooks);
    hooks
}

async fn kernel(url: &str, erp: Vec<ferrum_meta::Package>, finance: Vec<ferrum_meta::Package>, reset: bool) -> Arc<Kernel> {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 10, ..Default::default() }).await.expect("connect");
    if reset {
        sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    }
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(erp);
    packages.extend(finance);
    let k = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks(), KernelSettings::default(), None).await.expect("boot");
    k.set_file_store(Arc::new(MemFiles::default()));
    k
}

#[test]
fn upgrade_from_0_22_0() {
    let (Ok(old_erp), Ok(old_finance)) = (packages_at("packages"), packages_at("addons/finance/packages")) else {
        assert!(std::env::var("CI").is_err(), "CI must fetch the whole history ({OLD} is read from it)");
        eprintln!("skipped: commit {OLD} is not in this checkout");
        return;
    };
    run(scenario(old_erp, old_finance));
}

/// Everything that runs on a timer, once, as the server's upkeep and its job runner would.
async fn upkeeps(k: &Arc<Kernel>, warnings: &Warnings, when: &str) {
    ferrum_domain::maintenance(k).await.expect("platform upkeep");
    ferrum_app_erp::maintenance(k).await.expect("ERP's upkeep");
    ferrum_finance::maintenance(k).await.expect("Finance's upkeep");
    k.maintenance().await.expect("the kernel's upkeep, which queues backfills");
    k.run_jobs_once(20).await.expect("jobs");
    // The same upkeeps once more: idempotent, and nothing left to do.
    ferrum_app_erp::maintenance(k).await.unwrap();
    ferrum_finance::maintenance(k).await.unwrap();
    k.run_jobs_once(20).await.unwrap();
    assert_eq!(warnings.take(), Vec::<String>::new(), "{when}: the upkeeps and backfills logged");
    let mut tx = k.db.system_tx().await.unwrap();
    let failed: Vec<(String, Option<String>)> =
        sqlx::query_as("select kind, last_error from job where status <> 'done' and status <> 'cancelled'").fetch_all(&mut *tx).await.unwrap();
    tx.rollback().await.unwrap();
    assert!(failed.is_empty(), "{when}: jobs not done: {failed:?}");
}

async fn scenario(old_erp: Vec<ferrum_meta::Package>, old_finance: Vec<ferrum_meta::Package>) {
    let url = std::env::var("FERRUM_FINANCE_UPGRADE_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_finance_upgrade_test".into());
    let warnings = Warnings::install();

    // The old release's workspace, with documents.
    let k = kernel(&url, old_erp, old_finance, true).await;
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    let names: Vec<String> = k
        .registry
        .catalog()
        .packages()
        .map(|p| p.name().to_string())
        .filter(|n| !["contacts", "project_records", "parties", "products", "manufacturing"].contains(&n.as_str()))
        .collect();
    assert!(names.iter().any(|n| n == "finance"));
    let tenant = k
        .create_tenant(NewTenant {
            slug: "old".into(),
            name: "Old Co".into(),
            admin_email: "admin@old.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: names.clone(),
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("old", "admin@old.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let mut t = T { k: k.clone(), ctx, company: String::new() };
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    t.company = uid(&t.insert("Company", json!({"company_name": "Old Co", "abbr": "OC", "default_currency": inr["id"]})).await).to_string();
    let c = t.company.clone();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let service = uid(&t
        .insert("Item", json!({"item_code": "Service", "item_name": "Service", "item_group": products["id"], "stock_uom": nos["id"], "is_stock_item": false}))
        .await)
    .to_string();
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme", "email": "acme@suppliers.test"})).await).to_string();
    let bill = t
        .post("PurchaseInvoice", json!({"company": c, "supplier": acme, "bill_no": "A-1", "items": [{"item": service, "qty": "1", "price_list_rate": "1000"}]}))
        .await;
    let bank = t.account("Primary Bank").await;
    let house =
        uid(&t.insert("BankAccount", json!({"account_name": "House Bank", "company": c, "account": bank, "iban": "GB82WEST12345698765432"})).await).to_string();
    let today = t.ctx.today().to_string();
    let rid = uid(&t.insert("PaymentRun", json!({"company": c, "bank_account": bank, "bank": house, "due_by": today})).await);
    // A line proposed by 0.5.0, which kept no supplier email.
    let line = Uuid::now_v7();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("insert into e_payment_run__lines (tenant_id, id, parent_id, idx, pay, supplier, supplier_name, invoice_number, outstanding, amount, state) values ($1, $2, $3, 0, true, $4, 'Acme', 'A-1', 1000, 1000, 'Proposed')")
        .bind(tenant).bind(line).bind(rid).bind(acme.parse::<Uuid>().unwrap()).execute(&mut *tx).await.expect("an old line");
    tx.commit().await.unwrap();
    let entries = |t: &T| {
        let k = t.k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let n: i64 = sqlx::query_scalar("select count(*) from journal_entry").fetch_one(&mut *tx).await.unwrap();
            tx.rollback().await.unwrap();
            n
        }
    };
    let before = entries(&t).await;
    assert!(before > 0);
    warnings.take();
    drop(t);

    // This build starts on that workspace (the schema is synced to today's packages; the workspace keeps its old versions
    // and seeds), and every upkeep and backfill runs before an administrator applies the update.
    let k = kernel(&url, ferrum_app_erp::packages().unwrap(), ferrum_finance::packages().unwrap(), false).await;
    upkeeps(&k, &warnings, "before the update").await;
    let check = |k: Arc<Kernel>, expected: Option<&'static str>| async move {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        let email: Option<String> = sqlx::query_scalar("select supplier_email from e_payment_run__lines where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(line)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(email.as_deref(), expected);
    };
    check(k.clone(), Some("acme@suppliers.test")).await;

    // The administrator applies the update; everything runs again.
    k.install_packages(tenant, &names).await.expect("the update");
    k.registry.invalidate_all();
    upkeeps(&k, &warnings, "after the update").await;
    check(k.clone(), Some("acme@suppliers.test")).await;

    // The documents are as they were.
    let session = k.login("old", "admin@old.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), ctx, company: c };
    let again = t.get("PurchaseInvoice", uid(&bill)).await;
    assert_eq!(
        (again["docstatus"].clone(), again["number"].clone(), dec(&again["grand_total"])),
        (json!(1), bill["number"].clone(), dec(&bill["grand_total"]))
    );
    assert_eq!(entries(&t).await, before, "no journal entry was made or lost");
    assert_eq!(t.get("PaymentRun", rid).await["lines"][0]["supplier_name"], json!("Acme"));
}
