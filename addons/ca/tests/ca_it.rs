//! Canada end to end against a real PostgreSQL (`FERRUM_CA_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_ca_test`; reset first): an Ontario company
//! selling into Ontario, British Columbia (PST), Quebec (QST), Alberta, Nova Scotia, Saskatchewan
//! (no PST registration) and the United States, buying at home, and its GST/HST and QST returns.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn dec(v: &Value) -> Decimal {
    match v {
        Value::String(s) => Decimal::from_str(s).unwrap(),
        Value::Null => Decimal::ZERO,
        other => Decimal::from_str(&other.to_string()).unwrap(),
    }
}

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_CA_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_ca_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_ca::packages().expect("Canada's package"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_ca::register(&mut hooks);
    for source in ferrum_ca::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let plugins = ferrum_plugins::PluginHost::new(false).expect("plugin host");
    hooks.register("*", Arc::new(ferrum_plugins::PluginHook(plugins)));
    let settings = KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([5u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    k
}

#[test]
fn canada_end_to_end() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let k = fresh_kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "ca".into(),
            name: "Maple".into(),
            admin_email: "admin@maple.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying", "ca"].iter().map(|s| s.to_string()).collect(),
            settings: Some(json!({"fiscal_year_start_month": 1})),
        })
        .await
        .expect("tenant");
    let session = k.login("ca", "admin@maple.test", "Correct-Horse-9").await.expect("login");
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let first = async |entity: &str, filters: Value| -> Doc {
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() };
        k.list(&ctx, entity, &q).await.unwrap().rows.remove(0)
    };
    let insert = async |entity: &str, v: Value| -> Doc { k.insert(&ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}")) };
    let cad = insert("Currency", json!({"code": "CAD", "currency_name": "Canadian Dollar", "symbol": "$", "precision": 2})).await;
    let co = insert(
        "Company",
        json!({"company_name": "Maple Inc", "abbr": "MI", "default_currency": cad["id"], "country": "Canada", "ca_province": "ON",
               "ca_business_number": "123456789RT0001", "ca_qst_number": "1234567890TQ0001", "ca_pst_provinces": "BC"}),
    )
    .await;
    let cid = uid(&co);
    for number in ["2161", "2162", "1158"] {
        assert!(
            !k.list(
                &ctx,
                "Account",
                &ListQuery {
                    filters: ferrum_kernel::query::parse_filters(&json!({"company": cid.to_string(), "account_number": number})).unwrap(),
                    ..Default::default()
                }
            )
            .await
            .unwrap()
            .rows
            .is_empty(),
            "account {number}"
        );
    }
    let nos = first("Uom", json!({"uom_name": "Nos"})).await;
    let group = first("ItemGroup", json!({"group_name": "Products"})).await;
    let item = async |code: &str, more: Value| -> Uuid {
        let mut v = obj(
            json!({"item_code": code, "item_name": code, "item_group": group["id"], "stock_uom": nos["id"], "is_stock_item": false, "standard_rate": "100", "ca_kind": "Goods"}),
        );
        v.extend(more.as_object().cloned().unwrap_or_default());
        uid(&insert("Item", Value::Object(v)).await)
    };
    let widget = item("WIDGET", json!({})).await;
    let bread = item("BREAD", json!({"ca_gst_category": "Zero Rated"})).await;
    let customer = async |name: &str, state: &str, country: &str| -> Uuid {
        let c = uid(&insert("Customer", json!({"customer_name": name})).await);
        let a = json!({"address_type": "Billing", "address_line1": "1 Main St", "city": "City", "state": state, "country": country, "is_primary": true, "customer": c.to_string()});
        insert("Address", a).await;
        c
    };
    let sale = async |customer: Uuid, item: Uuid| -> Doc {
        insert("SalesInvoice", json!({"company": cid.to_string(), "customer": customer.to_string(), "items": [{"item": item.to_string(), "qty": "1"}]})).await
    };
    let taxes = |doc: &Doc| -> Vec<(String, Decimal)> {
        doc["taxes"].as_array().unwrap().iter().map(|t| (t["description"].as_str().unwrap().to_string(), dec(&t["tax_amount"]))).collect()
    };

    let on = customer("Toronto Ltd", "ON", "Canada").await;
    let si = sale(on, widget).await;
    assert_eq!(taxes(&si), vec![("GST/HST".into(), n("13"))], "HST in Ontario");
    k.submit(&ctx, "SalesInvoice", uid(&si), None).await.expect("submit");
    let bc = customer("Vancouver Ltd", "British Columbia", "Canada").await;
    let si = sale(bc, widget).await;
    assert_eq!(taxes(&si), vec![("GST/HST".into(), n("5")), ("PST".into(), n("7"))], "GST and BC's PST");
    k.submit(&ctx, "SalesInvoice", uid(&si), None).await.expect("submit");
    let qc = customer("Montréal Inc", "Québec", "Canada").await;
    let si = sale(qc, widget).await;
    assert_eq!(taxes(&si), vec![("GST/HST".into(), n("5")), ("QST".into(), n("9.98"))], "GST and QST");
    k.submit(&ctx, "SalesInvoice", uid(&si), None).await.expect("submit");
    let ab = customer("Calgary Ltd", "AB", "Canada").await;
    assert_eq!(taxes(&sale(ab, widget).await), vec![("GST/HST".into(), n("5"))]);
    let ns = customer("Halifax Ltd", "Nova Scotia", "Canada").await;
    assert_eq!(taxes(&sale(ns, widget).await), vec![("GST/HST".into(), n("14"))], "Nova Scotia's 14% since April 2025");
    let sk = customer("Regina Ltd", "SK", "Canada").await;
    assert_eq!(taxes(&sale(sk, widget).await), vec![("GST/HST".into(), n("5"))], "no PST where the company is not registered");
    let us = customer("Buffalo Inc", "NY", "United States").await;
    let export = sale(us, widget).await;
    assert_eq!((dec(&export["total_taxes"]), export["tax_breakdown"][0]["category"].clone()), (n("0"), json!("Export")));
    assert_eq!(dec(&sale(on, bread).await["total_taxes"]), n("0"), "basic groceries are zero-rated");

    // Buying at home: the HST recovered.
    let supplier = uid(&insert("Supplier", json!({"supplier_name": "Ottawa Supply"})).await);
    let pi = insert(
        "PurchaseInvoice",
        json!({"company": cid.to_string(), "supplier": supplier.to_string(), "bill_no": "OS-1", "items": [{"item": widget.to_string(), "qty": "2", "price_list_rate": "100"}]}),
    )
    .await;
    assert_eq!(taxes(&pi), vec![("GST/HST".into(), n("26"))]);
    k.submit(&ctx, "PurchaseInvoice", uid(&pi), None).await.expect("purchase");

    // The returns: 300 of sales; 13 + 5 + 5 collected; 26 recovered.
    let report = async |name: &str| -> HashMap<String, Decimal> {
        let p: HashMap<String, String> = [("company", cid.to_string()), ("from_date", "2000-01-01".into()), ("to_date", "2100-12-31".into())]
            .into_iter()
            .map(|(k, v)| (k.into(), v))
            .collect();
        let r = ferrum_domain::reports::run(&k, &ctx, name, &p).await.unwrap_or_else(|e| panic!("{name}: {e:?}"));
        r["rows"].as_array().unwrap().iter().map(|l| (l["line"].as_str().unwrap().to_string(), dec(&l["amount"]))).collect()
    };
    let gst = report("ca_gst_hst_return").await;
    assert_eq!((gst["101"], gst["103"], gst["106"], gst["109"]), (n("300"), n("23"), n("26"), n("-3")));
    let qst = report("ca_qst_return").await;
    assert_eq!((qst["103"], qst["109"]), (n("9.98"), n("9.98")));
}
