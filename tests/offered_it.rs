//! What ERP offers other apps through the gate (src/offered.rs, docs/contracts.md "Operations through the gate"),
//! called the way an app calls them: `Kernel::call_tx` with the vault handle its registration took, in its own
//! transaction, against a real PostgreSQL (`FERRUM_ERP_OFFERED_TEST_DATABASE_URL`, reset first). A posting balances
//! to the cent or nothing is written; one key posts once, however many times and however concurrently it is asked;
//! a person cannot post; a reversal reverses once; a person's customer is found or made once; a document is priced.

use chrono::Utc;
use ferrum_kernel::capability::{Invocation, Receipt};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::vault::Vault;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Result};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// The calling app: a record of its own the postings are for, and the operations it declares it uses.
const CALLER: &str = r#"
package: caller
version: 0.1.0
label: Caller
depends: [accounting, selling]
uses:
  - {capability: accounting.company.post_entry, version: '^0.4'}
  - {capability: accounting.company.reverse_entries, version: '^0.4'}
  - {capability: selling.company.customer_for, version: '^0.2'}
  - {capability: selling.company.customers_of, version: '^0.2'}
  - {capability: selling.company.price_document, version: '^0.2'}
"#;
const VOUCHER: &str = r#"
- name: CallerVoucher
  label: Caller Voucher
  kind: master
  title_field: title
  fields:
    - {name: title, type: data, required: true}
"#;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn uid(v: &Value) -> Uuid {
    v.as_str().unwrap().parse().unwrap()
}

fn opener(url: &str) -> ferrum_kernel::vault::Opener {
    ferrum_kernel::vault::Opener {
        database_url: url.to_string(),
        file: "/nonexistent/root.key".into(),
        kdf: ferrum_kernel::vault::Kdf { m_kib: 64, t: 1, p: 1 },
        ..Default::default()
    }
}

/// The caller's call for `ctx`'s person in a transaction of its own, committed when it is done.
async fn call(k: &Kernel, vault: &Vault, ctx: &Ctx, inv: Invocation) -> Result<Receipt> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
    let done = k.call_tx(vault, ctx, &mut tx, &inv).await?;
    tx.commit().await.unwrap();
    Ok(done)
}

fn refused(e: &Error, needle: &str) -> bool {
    match e {
        Error::Invalid(m) | Error::Forbidden(m) => m.contains(needle),
        Error::Validation(fs) => fs.iter().any(|f| f.message.contains(needle)),
        other => other.to_string().contains(needle),
    }
}

#[test]
fn erp_offers_its_operations_through_the_gate() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_OFFERED_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_offered_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists vault cascade; drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    ferrum_kernel::vault::open(&db, opener(&url), false).await.expect("the vault");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages
        .extend(ferrum_meta::catalog::load_embedded(&[("caller", "package.yaml", CALLER), ("caller", "entities/voucher.yaml", VOUCHER)]).expect("the caller"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    let erp_packages: Vec<String> = ferrum_app_erp::packages().unwrap().iter().map(|p| p.name().to_string()).collect();
    hooks.app("erp", &[], ferrum_app_erp::register);
    hooks.own("erp", &erp_packages);
    let handle: Arc<Mutex<Option<Vault>>> = Arc::default();
    let kept = handle.clone();
    hooks.app("caller", &[], move |h| *kept.lock().unwrap() = Some(h.vault()));
    hooks.own("caller", &["caller".to_string()]);
    let vault = handle.lock().unwrap().clone().expect("the caller's handle");
    let k: Arc<Kernel> = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "off".into(),
            name: "Offered Co".into(),
            admin_email: "admin@off.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "caller"].iter().map(|s| s.to_string()).collect(),
            settings: None,
        })
        .await
        .unwrap();
    let session = k.login("off", "admin@off.test", "Correct-Horse-9").await.unwrap();
    let admin: Ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let find = |entity: &'static str, filters: Value| {
        let (k, ctx) = (k.clone(), admin.clone());
        async move {
            let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() };
            k.list(&ctx, entity, &q).await.unwrap().rows.remove(0)
        }
    };
    let inr = find("Currency", json!({"code": "INR"})).await;
    let company = k.insert(&admin, "Company", obj(json!({"company_name": "Offered Co", "abbr": "OC", "default_currency": inr["id"]}))).await.unwrap();
    let company = k.get(&admin, "Company", uid(&company["id"]), false).await.unwrap();
    let cid = uid(&company["id"]);
    let (receivable, bank) = (uid(&company["default_receivable_account"]), uid(&company["default_bank_account"]));
    let group: Uuid = uid(&find("Account", json!({"company": cid.to_string(), "is_group": true})).await["id"]);
    let voucher = uid(&k.insert(&admin, "CallerVoucher", obj(json!({"title": "V-1"}))).await.unwrap()["id"]);
    let today = Utc::now().date_naive().to_string();
    let post = |lines: Value, source: Uuid| {
        Invocation::new(
            "accounting.company.post_entry",
            json!({"id": cid.to_string(), "params": {"posting_date": today, "source_entity": "CallerVoucher", "source_id": source.to_string(),
                   "source_number": "V-1", "lines": lines.to_string()}}),
        )
    };
    let entries = |source: Uuid| {
        let k = k.clone();
        async move {
            let mut tx = k.db.tenant_tx(tenant).await.unwrap();
            let n: i64 = sqlx::query_scalar("select count(*) from journal_entry where tenant_id = $1 and source_id = $2")
                .bind(tenant)
                .bind(source)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            n
        }
    };
    let globex = uid(&k.insert(&admin, "Customer", obj(json!({"customer_name": "Globex"}))).await.unwrap()["id"]);
    let balanced = json!([{"account": receivable, "debit": "100.10", "customer": globex}, {"account": bank, "credit": "100.10", "remarks": "paid"}]);

    // ── a posting balances to the cent, or nothing is written ──
    let e = call(
        &k,
        &vault,
        &admin,
        post(json!([{"account": receivable, "debit": "100.10", "customer": globex}, {"account": bank, "credit": "100.00"}]), voucher).key("u-1"),
    )
    .await
    .unwrap_err();
    assert!(refused(&e, "differ: an entry balances"), "{e:?}");
    let e = call(
        &k,
        &vault,
        &admin,
        post(json!([{"account": receivable, "debit": "0.001", "customer": globex}, {"account": bank, "credit": "0.001"}]), voucher).key("u-2"),
    )
    .await
    .unwrap_err();
    assert!(refused(&e, "decimals"), "a fraction of a cent is refused, not rounded: {e:?}");
    let e = call(&k, &vault, &admin, post(json!([{"account": group, "debit": "1"}, {"account": bank, "credit": "1"}]), voucher).key("u-3")).await.unwrap_err();
    assert!(refused(&e, "not a ledger account of this company"), "{e:?}");
    let e = call(&k, &vault, &admin, post(balanced.clone(), Uuid::now_v7()).key("u-4")).await.unwrap_err();
    assert!(refused(&e, "no CallerVoucher has this id"), "{e:?}");
    // Every id a line gives names a record that is there: the ledger keeps a line for ever.
    let with = |extra: Value| {
        let mut l = json!({"account": bank, "debit": "10"});
        l.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        json!([l, {"account": receivable, "credit": "10", "customer": globex}])
    };
    // a_line_with_a_customer_of_no_one_is_refused
    let e = call(
        &k,
        &vault,
        &admin,
        post(json!([{"account": receivable, "debit": "10", "customer": Uuid::now_v7()}, {"account": bank, "credit": "10"}]), voucher).key("u-5"),
    )
    .await
    .unwrap_err();
    assert!(refused(&e, "is not a customer of this workspace"), "{e:?}");
    // a_cost_center_of_another_company_is_refused
    let other = k.insert(&admin, "Company", obj(json!({"company_name": "Other Co", "abbr": "OT", "default_currency": inr["id"]}))).await.unwrap();
    let elsewhere = find("CostCenter", json!({"company": other["id"]})).await;
    let e = call(&k, &vault, &admin, post(with(json!({"cost_center": elsewhere["id"]})), voucher).key("u-6")).await.unwrap_err();
    assert!(refused(&e, "is not a cost center of this company"), "{e:?}");
    // a_receivable_line_without_its_customer_is_refused (the journal entry's rule)
    let e = call(&k, &vault, &admin, post(json!([{"account": receivable, "debit": "10"}, {"account": bank, "credit": "10"}]), voucher).key("u-7"))
        .await
        .unwrap_err();
    assert!(refused(&e, "receivable/payable lines need a party"), "{e:?}");
    // against_must_exist
    let e =
        call(&k, &vault, &admin, post(with(json!({"against_entity": "CallerVoucher", "against_id": Uuid::now_v7()})), voucher).key("u-8")).await.unwrap_err();
    assert!(refused(&e, "no CallerVoucher has this id"), "{e:?}");
    let e = call(&k, &vault, &admin, post(with(json!({"against_entity": "NoSuchThing", "against_id": voucher})), voucher).key("u-9")).await.unwrap_err();
    assert!(refused(&e, "no NoSuchThing has this id"), "{e:?}");
    // A closed period is the database's refusal, in its words, not an internal error.
    let year = find("FiscalYear", json!({"company": cid.to_string()})).await;
    let start = year["start_date"].as_str().unwrap().to_string();
    k.run_action(&admin, "FiscalYear", uid(&year["id"]), "close_period", json!({"period_start": start})).await.expect("close the first month");
    let mut early = post(balanced.clone(), voucher).key("u-10");
    early.input["params"]["posting_date"] = json!(start);
    let e = call(&k, &vault, &admin, early).await.unwrap_err();
    assert!(matches!(e, Error::Invalid(_) | Error::Validation(_)) && refused(&e, "is closed"), "{e:?}");
    assert_eq!(entries(voucher).await, 0, "nothing refused was written");

    // ── one key posts once: repeated, and racing ──
    let first = call(&k, &vault, &admin, post(balanced.clone(), voucher).key("v1-issue")).await.expect("post");
    let again = call(&k, &vault, &admin, post(balanced.clone(), voucher).key("v1-issue")).await.expect("repeat");
    assert_eq!((first.repeated, again.repeated), (false, true));
    assert!(first.result["entry"].is_string(), "{:?}", first.result);
    assert_eq!(entries(voucher).await, 1);
    let e = call(&k, &vault, &admin, post(json!([{"account": receivable, "debit": "5"}, {"account": bank, "credit": "5"}]), voucher).key("v1-issue"))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("input") || refused(&e, "other input"), "the same key with other lines is refused: {e:?}");
    let race = (0..4)
        .map(|_| {
            let (k, vault, admin, inv) = (k.clone(), vault.clone(), admin.clone(), post(balanced.clone(), voucher).key("v1-race"));
            tokio::spawn(async move { call(&k, &vault, &admin, inv).await })
        })
        .collect::<Vec<_>>();
    let mut made = 0;
    for r in race {
        if !r.await.unwrap().expect("each racer is answered").repeated {
            made += 1;
        }
    }
    assert_eq!(made, 1, "four racers with one key make one posting");
    assert_eq!(entries(voucher).await, 2);
    let net: Decimal = {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        sqlx::query_scalar("select coalesce(sum(debit) - sum(credit), 0) from journal_line where tenant_id = $1 and entry_id in (select id from journal_entry where source_id = $2)")
            .bind(tenant)
            .bind(voucher)
            .fetch_one(&mut *tx)
            .await
            .unwrap()
    };
    assert_eq!(net, Decimal::ZERO, "every posting balances");

    // ── a person, or an agent, cannot post: only an app's call ──
    let e = k.invoke(&admin, post(balanced.clone(), voucher).key("by-hand")).await.unwrap_err();
    assert!(refused(&e, "an app posts to the ledger"), "{e:?}");
    assert_eq!(entries(voucher).await, 2);

    // ── reversing reverses once ──
    let reverse = |key: &str| {
        Invocation::new(
            "accounting.company.reverse_entries",
            json!({"id": cid.to_string(), "params": {"source_entity": "CallerVoucher", "source_id": voucher.to_string()}}),
        )
        .key(key)
    };
    let r = call(&k, &vault, &admin, reverse("v1-cancel")).await.expect("reverse");
    assert_eq!(r.result["reversed"], json!(2));
    let r = call(&k, &vault, &admin, reverse("v1-cancel-again")).await.expect("reverse again");
    assert_eq!(r.result["reversed"], json!(0), "nothing is reversed twice");
    let receivable_net: Decimal = {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        sqlx::query_scalar("select coalesce(sum(debit - credit), 0) from journal_line where tenant_id = $1 and account_id = $2")
            .bind(tenant)
            .bind(receivable)
            .fetch_one(&mut *tx)
            .await
            .unwrap()
    };
    assert_eq!(receivable_net, n("0"));

    // ── a person's customer, found or made once ──
    let customer_for = |key: &str, email: &str| {
        Invocation::new("selling.company.customer_for", json!({"id": cid.to_string(), "params": {"name": "Ada Guest", "email": email}})).key(key)
    };
    let made = call(&k, &vault, &admin, customer_for("guest-1", "Ada@Guest.test")).await.expect("customer_for");
    assert_eq!(made.result["created"], json!(true));
    let found = call(&k, &vault, &admin, customer_for("guest-2", "ada@guest.test ")).await.expect("customer_for again");
    assert_eq!((found.result["created"].clone(), found.result["customer"].clone()), (json!(false), made.result["customer"].clone()));
    let of = call(&k, &vault, &admin, Invocation::new("selling.company.customers_of", json!({"id": cid.to_string(), "params": {"email": "ada@guest.test"}})))
        .await
        .expect("customers_of");
    assert_eq!(of.result["customers"], json!([made.result["customer"]]));

    // ── a document priced as an order would be, nothing saved ──
    let group_of_items = find("ItemGroup", json!({"group_name": "Services"})).await;
    let nos = find("Uom", json!({"uom_name": "Nos"})).await;
    let item = k
        .insert(
            &admin,
            "Item",
            obj(json!({"item_code": "SVC", "item_name": "Service", "item_group": group_of_items["id"], "stock_uom": nos["id"], "is_stock_item": false})),
        )
        .await
        .unwrap();
    let document = json!({"customer": made.result["customer"], "items": [{"item": item["id"], "qty": "2", "price_list_rate": "250.50"}]}).to_string();
    let priced = call(&k, &vault, &admin, Invocation::new("selling.company.price_document", json!({"id": cid.to_string(), "params": {"document": document}})))
        .await
        .expect("price_document");
    let doc = &priced.result["document"];
    let total = ferrum_kernel::value::parse_decimal(&doc["grand_total"]).unwrap();
    assert_eq!(total, n("501.00"), "{doc}");
    let orders: i64 = {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        sqlx::query_scalar("select count(*) from e_sales_order where tenant_id = $1").bind(tenant).fetch_one(&mut *tx).await.unwrap()
    };
    assert_eq!(orders, 0, "nothing is saved");
}
