//! India's place of supply read from ERP's delivery seam, against a real PostgreSQL
//! (`FERRUM_INDIA_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_india_test`;
//! reset first): the same customer, one invoice delivered in the company's state (through a ship-to
//! address) and one in another (through the invoice's delivery region alone), gets CGST and SGST on
//! the first and IGST on the second.

use ferrum_india::gstin_check_char;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::sync::Arc;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn gstin(first14: &str) -> String {
    format!("{first14}{}", gstin_check_char(first14).unwrap())
}

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_INDIA_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_india_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_india::packages().expect("India's package"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_india::register(&mut hooks);
    let settings = KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([5u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    k
}

#[test]
fn place_of_supply_follows_the_delivery() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let k = fresh_kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "pos".into(),
            name: "Place of Supply".into(),
            admin_email: "admin@pos.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying", "india"].iter().map(|s| s.to_string()).collect(),
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("tenant");
    let session = k.login("pos", "admin@pos.test", "Correct-Horse-9").await.expect("login");
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let first = |entity: &'static str, filters: Value| {
        let (k, ctx) = (k.clone(), ctx.clone());
        async move {
            let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() };
            k.list(&ctx, entity, &q).await.unwrap().rows.remove(0)
        }
    };
    let insert = |entity: &'static str, v: Value| {
        let (k, ctx) = (k.clone(), ctx.clone());
        async move { k.insert(&ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}")) }
    };
    let inr = first("Currency", json!({"code": "INR"})).await;
    // A company in Maharashtra (27); a customer registered in Karnataka (29).
    let co = insert("Company", json!({"company_name": "Pune Works", "abbr": "PW", "default_currency": inr["id"], "in_gstin": gstin("27AAPFU0939F1Z")})).await;
    let customer = insert("Customer", json!({"customer_name": "Globex", "in_gstin": gstin("29AAGCG1234A1Z")})).await;
    let nos = first("Uom", json!({"uom_name": "Nos"})).await;
    let group = first("ItemGroup", json!({"group_name": "Products"})).await;
    let item = insert(
        "Item",
        json!({"item_code": "WIDGET", "item_name": "Widget", "item_group": group["id"], "stock_uom": nos["id"], "hsn_code": "8479", "standard_rate": "100", "is_stock_item": false}),
    )
    .await;
    let order = |more: Value| {
        let mut v = obj(json!({"company": co["id"], "customer": customer["id"], "items": [{"item": item["id"], "qty": "1"}]}));
        v.extend(more.as_object().cloned().unwrap_or_default());
        insert("SalesInvoice", Value::Object(v))
    };
    let accounts =
        |o: &Doc| -> Vec<String> { o["taxes"].as_array().unwrap().iter().map(|t| t["description"].as_str().unwrap_or_default().to_string()).collect() };

    // Delivered to the customer's site in Pune: the company's state.
    let site = insert(
        "Address",
        json!({"address_type": "Shipping", "address_line1": "1 MG Road", "city": "Pune", "state": "Maharashtra", "country": "India", "customer": customer["id"]}),
    )
    .await;
    let local = order(json!({"ship_to": site["id"]})).await;
    assert_eq!((local["delivery_country"].clone(), local["delivery_region"].clone()), (json!("IN"), json!("Maharashtra")), "the seam follows the ship-to");
    assert_eq!(local["in_place_of_supply"], json!("27"));
    assert_eq!(accounts(&local), ["CGST 9%", "SGST 9%"], "delivered in the company's state: CGST and SGST");

    // The same customer, delivered in Karnataka, stated on the order without an address.
    let away = order(json!({"delivery_country": "India", "delivery_region": "Karnataka"})).await;
    assert_eq!(away["delivery_country"], json!("IN"), "kept as the ISO code");
    assert_eq!(away["in_place_of_supply"], json!("29"));
    assert_eq!(accounts(&away), ["IGST 18%"], "delivered in another state: IGST");

    // Without any delivery stated, the customer's registration decides, as before.
    let plain = order(json!({})).await;
    assert_eq!(plain["in_place_of_supply"], json!("29"));
    assert_eq!(accounts(&plain), ["IGST 18%"]);
}
