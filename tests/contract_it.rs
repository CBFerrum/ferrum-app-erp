//! ERP's contract with the apps that call its engine (docs/contracts.md), against a real PostgreSQL
//! (`FERRUM_ERP_CONTRACT_TEST_DATABASE_URL`, reset first): each guarantee it states, held. A posting
//! balances or nothing is written; every call appends, and reversing reverses once; an open item
//! opens once, takes no more than it is owed and cannot be cancelled while paid; stock is never
//! issued beyond what is there, and its reversal is exact and once.

use chrono::Utc;
use ferrum_app_erp::ledger::{self, Entry, Line, OpenItem};
use ferrum_app_erp::stock::{self, MoveIn};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Tx};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn id(v: &Value) -> Uuid {
    v.as_str().unwrap().parse().unwrap()
}

async fn find(k: &Kernel, ctx: &Ctx, entity: &str, filters: Value) -> Doc {
    let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() };
    k.list(ctx, entity, &q).await.unwrap().rows.remove(0)
}

async fn count(tx: &mut Tx, sql: &'static str, source: Uuid) -> i64 {
    sqlx::query_scalar(sql).bind(source).fetch_one(&mut **tx).await.unwrap()
}

#[test]
fn the_engines_contract_holds() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_CONTRACT_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_contract_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    let k: Arc<Kernel> = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "co".into(),
            name: "Contract Co".into(),
            admin_email: "admin@co.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory"].iter().map(|s| s.to_string()).collect(),
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("co", "admin@co.test", "Correct-Horse-9").await.unwrap();
    let ctx: Ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let inr = find(&k, &ctx, "Currency", json!({"code": "INR"})).await;
    let company = k.insert(&ctx, "Company", obj(json!({"company_name": "Contract Co", "abbr": "CC", "default_currency": inr["id"]}))).await.unwrap();
    let company = k.get(&ctx, "Company", id(&company["id"]), false).await.unwrap();
    let cid = id(&company["id"]);
    let (receivable, bank) = (id(&company["default_receivable_account"]), id(&company["default_bank_account"]));
    let customer = k.insert(&ctx, "Customer", obj(json!({"customer_name": "Globex"}))).await.unwrap();
    let customer = id(&customer["id"]);
    let today = Utc::now().date_naive();
    let entry = |source: Uuid, lines: Vec<Line>| Entry { lines, ..Entry::new(cid, today, "ContractTest", source, None) };
    let entries = "select count(*) from journal_entry where source_entity = 'ContractTest' and source_id = $1";

    // ── ledger::post: balanced, or nothing is written ──
    let source = Uuid::now_v7();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let unbalanced = ledger::post(&mut tx, &ctx, entry(source, vec![Line::new(receivable, n("100"), "INR"), Line::new(bank, n("-90"), "INR")]), 2).await;
    assert!(matches!(unbalanced, Err(Error::Internal(ref m)) if m.contains("unbalanced")), "{unbalanced:?}");
    drop(tx);
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert_eq!(count(&mut tx, entries, source).await, 0, "the caller's transaction ends, and with it the posting");
    let nothing = ledger::post(&mut tx, &ctx, entry(source, vec![Line::new(receivable, n("0.001"), "INR"), Line::new(bank, n("-0.001"), "INR")]), 2).await;
    assert_eq!(nothing.unwrap(), None, "lines that net to nothing post nothing");
    // Every call appends: the caller calls once for each event of its document.
    for _ in 0..2 {
        let posted = ledger::post(&mut tx, &ctx, entry(source, vec![Line::new(receivable, n("100"), "INR"), Line::new(bank, n("-100"), "INR")]), 2).await;
        assert!(posted.unwrap().is_some());
    }
    assert_eq!(count(&mut tx, entries, source).await, 2);

    // ── ledger::reverse: what is not reversed yet, once ──
    assert_eq!(ledger::reverse(&mut tx, &ctx, "ContractTest", source, today, 2).await.unwrap(), 2);
    assert_eq!(ledger::reverse(&mut tx, &ctx, "ContractTest", source, today, 2).await.unwrap(), 0, "a second reversal finds nothing left");
    let net: Decimal = sqlx::query_scalar(
        "select coalesce(sum(l.debit - l.credit), 0) from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
         where e.source_id = $1 and l.account_id = $2",
    )
    .bind(source)
    .bind(receivable)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(net, Decimal::ZERO);
    tx.commit().await.unwrap();

    // ── open items: opened once, never paid beyond what is owed, not cancelled while paid ──
    let invoice = Uuid::now_v7();
    let item = OpenItem {
        id: invoice,
        company: cid,
        entity: "SalesInvoice".into(),
        number: Some("CT-1".into()),
        customer: Some(customer),
        supplier: None,
        account: receivable,
        posting_date: today,
        due_date: None,
        currency: "INR".into(),
        exchange_rate: Decimal::ONE,
        amount: n("100"),
        base_amount: n("100"),
    };
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    ledger::open_item(&mut tx, &ctx, &item).await.unwrap();
    tx.commit().await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert!(ledger::open_item(&mut tx, &ctx, &item).await.is_err(), "an invoice's item opens once");
    drop(tx);
    let payment = Uuid::now_v7();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let over = ledger::allocate(&mut tx, &ctx, cid, invoice, "PaymentEntry", payment, today, n("150"), n("150")).await;
    assert!(matches!(over, Err(Error::Invalid(ref m)) if m.contains("exceeds")), "{over:?}");
    ledger::allocate(&mut tx, &ctx, cid, invoice, "PaymentEntry", payment, today, n("60"), n("60")).await.unwrap();
    assert_eq!(ledger::load_open_item(&mut tx, tenant, invoice, false).await.unwrap().unwrap().outstanding, n("40"));
    let paid = ledger::cancel_open_item(&mut tx, &ctx, invoice).await;
    assert!(matches!(paid, Err(Error::Invalid(ref m)) if m.contains("payments are allocated")), "{paid:?}");
    assert_eq!(ledger::unallocate(&mut tx, &ctx, "PaymentEntry", payment, today).await.unwrap(), [invoice]);
    assert!(ledger::unallocate(&mut tx, &ctx, "PaymentEntry", payment, today).await.unwrap().is_empty(), "undone once");
    assert_eq!(ledger::load_open_item(&mut tx, tenant, invoice, false).await.unwrap().unwrap().outstanding, n("100"));
    ledger::cancel_open_item(&mut tx, &ctx, invoice).await.unwrap();
    let cancelled = ledger::allocate(&mut tx, &ctx, cid, invoice, "PaymentEntry", payment, today, n("10"), n("10")).await;
    assert!(cancelled.is_err(), "a cancelled item takes no payment");
    // A credit (a credit note, an advance) is a negative item: it is given back, as a refund does,
    // by negative allocations, never beyond it and never the other way.
    let credit = OpenItem { id: Uuid::now_v7(), number: Some("CT-CN".into()), amount: n("-50"), base_amount: n("-50"), ..item.clone() };
    ledger::open_item(&mut tx, &ctx, &credit).await.unwrap();
    let wrong_way = ledger::allocate(&mut tx, &ctx, cid, credit.id, "PaymentEntry", payment, today, n("20"), n("20")).await;
    assert!(matches!(wrong_way, Err(Error::Invalid(_))), "{wrong_way:?}");
    let beyond = ledger::allocate(&mut tx, &ctx, cid, credit.id, "PaymentEntry", payment, today, n("-60"), n("-60")).await;
    assert!(matches!(beyond, Err(Error::Invalid(_))), "{beyond:?}");
    ledger::allocate(&mut tx, &ctx, cid, credit.id, "PaymentEntry", payment, today, n("-20"), n("-20")).await.unwrap();
    assert_eq!(ledger::load_open_item(&mut tx, tenant, credit.id, false).await.unwrap().unwrap().outstanding, n("-30"));
    tx.commit().await.unwrap();

    // ── stock: never issued beyond what is there; reversed exactly, once ──
    let group = find(&k, &ctx, "ItemGroup", json!({"group_name": "Products"})).await;
    let uom = find(&k, &ctx, "Uom", json!({"uom_name": "Nos"})).await;
    let bolt = k.insert(&ctx, "Item", obj(json!({"item_code": "BOLT", "item_name": "Bolt", "item_group": group["id"], "stock_uom": uom["id"]}))).await.unwrap();
    let bolt = id(&bolt["id"]);
    let stores = find(&k, &ctx, "Warehouse", json!({"warehouse_name": "Stores", "company": cid.to_string()})).await;
    let stores = id(&stores["id"]);
    let receipt = Uuid::now_v7();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let co = ferrum_domain::util::company(&k, &ctx, &mut tx, cid).await.unwrap();
    let day = today;
    let moved = |qty: &str, rate: Option<&str>| MoveIn { item: bolt, warehouse: stores, qty: n(qty), rate: rate.map(n), ..Default::default() };
    let posted = stock::post_moves(&mut tx, &ctx, &co, "ContractTest", receipt, day, Utc::now(), vec![moved("10", Some("5"))], "Delivered").await.unwrap();
    assert_eq!(posted[0].value, n("50"));
    tx.commit().await.unwrap();
    let issue = Uuid::now_v7();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let short = stock::post_moves(&mut tx, &ctx, &co, "ContractTest", issue, day, Utc::now(), vec![moved("-11", None)], "Delivered").await;
    assert!(matches!(short, Err(Error::Invalid(ref m)) if m.contains("not enough stock")), "{short:?}");
    drop(tx);
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let moves = "select count(*) from stock_move where source_entity = 'ContractTest' and source_id = $1";
    assert_eq!(count(&mut tx, moves, issue).await, 0, "nothing was issued");
    let back = stock::reverse_moves(&mut tx, &ctx, &co, "ContractTest", receipt, day, Utc::now()).await.unwrap();
    assert_eq!((back.len(), back[0].value), (1, n("-50")), "the receipt's exact value back out");
    assert!(stock::reverse_moves(&mut tx, &ctx, &co, "ContractTest", receipt, day, Utc::now()).await.unwrap().is_empty(), "and once");
    tx.commit().await.unwrap();
    assert_eq!(stock::parse_serials(Some(" A1 \n\nB2,C3 ")), ["A1", "B2", "C3"]);
}
