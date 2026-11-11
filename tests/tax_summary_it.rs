//! The tax summary (FN-41) against a real PostgreSQL (`FERRUM_ERP_TAXSUM_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_taxsum_test`; reset first):
//! sales at two rates, a credit note netting one of them, a cancelled invoice left out and a
//! purchase on the input side, each group drilling to its documents.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings, ListQuery};
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

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

fn dec(v: &Value) -> Decimal {
    Decimal::from_str(v.as_str().unwrap_or("0")).unwrap()
}

struct T {
    k: Arc<Kernel>,
    ctx: Ctx,
}

impl T {
    async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.k.insert(&self.ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let page =
            self.k.list(&self.ctx, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() }).await.unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
}

#[test]
fn tax_summary() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_TAXSUM_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_taxsum_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    for source in ferrum_app_erp::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let k: Arc<Kernel> = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "ts".into(),
            name: "Tax Summary Co".into(),
            admin_email: "admin@ts.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying"].iter().map(|s| s.to_string()).collect(),
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("ts", "admin@ts.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), ctx };

    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let cid = uid(&t.insert("Company", json!({"company_name": "Tax Summary Co", "abbr": "TS", "default_currency": inr["id"]})).await);
    let c = cid.to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let advice = uid(&t
        .insert("Item", json!({"item_code": "Advice", "item_name": "Advice", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "100", "is_stock_item": false}))
        .await)
    .to_string();
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await).to_string();
    let output = t.find_one("Account", json!({"account_name": "Output Tax", "company": c})).await["id"].clone();
    let input = t.find_one("Account", json!({"account_name": "Input Tax", "company": c})).await["id"].clone();
    let template = |title: &str, kind: &str, account: &Value, rate: &str| json!({"title": title, "company": c, "kind": kind, "rows": [{"charge_type": "On Net Total", "account": account, "description": title, "rate": rate}]});
    let full = uid(&t.insert("TaxTemplate", template("VAT 10%", "Sales", &output, "10")).await).to_string();
    let reduced = uid(&t.insert("TaxTemplate", template("VAT 5%", "Sales", &output, "5")).await).to_string();
    let bought = uid(&t.insert("TaxTemplate", template("Input VAT 10%", "Purchase", &input, "10")).await).to_string();
    let sale = |template: &str, qty: &str| json!({"company": c, "customer": globex, "tax_template": template, "items": [{"item": advice, "qty": qty}]});

    let si1 = uid(&t.insert("SalesInvoice", sale(&full, "2")).await);
    let si1_doc = t.submit("SalesInvoice", si1).await;
    let si2 = uid(&t.insert("SalesInvoice", sale(&reduced, "1")).await);
    t.submit("SalesInvoice", si2).await;
    // One unit of the first credited back: it nets the 10% group.
    let mut credit = sale(&full, "-1");
    credit.as_object_mut().unwrap().extend(obj(json!({"is_return": true, "return_against": si1.to_string()})));
    let cn = uid(&t.insert("SalesInvoice", credit).await);
    t.submit("SalesInvoice", cn).await;
    // Cancelled: left out.
    let gone = uid(&t.insert("SalesInvoice", sale(&full, "3")).await);
    t.submit("SalesInvoice", gone).await;
    t.k.cancel(&t.ctx, "SalesInvoice", gone, None).await.expect("cancel");
    let pi = uid(&t
        .insert(
            "PurchaseInvoice",
            json!({"company": c, "supplier": acme, "bill_no": "A-1", "tax_template": bought, "items": [{"item": advice, "qty": "1", "price_list_rate": "50"}]}),
        )
        .await);
    t.submit("PurchaseInvoice", pi).await;

    let day = si1_doc["posting_date"].as_str().unwrap().to_string();
    let params: HashMap<String, String> =
        [("company", c.clone()), ("from_date", day.clone()), ("to_date", day)].into_iter().map(|(a, b)| (a.to_string(), b)).collect();
    let report = ferrum_domain::reports::run(&t.k, &t.ctx, "tax_summary", &params).await.expect("tax summary");
    let rows = report["rows"].as_array().unwrap();
    // The groups: direction, rate, taxable base and tax, in company currency.
    let groups: Vec<(String, String, Decimal, Decimal, Decimal)> = rows
        .iter()
        .filter(|r| r["_bold"] == json!(true) && r["voucher_id"].is_null())
        .map(|r| {
            (
                r["direction"].as_str().unwrap().to_string(),
                r["category"].as_str().unwrap_or("").to_string(),
                dec(&r["rate"]),
                dec(&r["taxable"]),
                dec(&r["tax"]),
            )
        })
        .collect();
    assert_eq!(
        groups,
        vec![
            ("Output".into(), "Standard".into(), Decimal::from(5), Decimal::from(100), Decimal::from(5)),
            ("Output".into(), "Standard".into(), Decimal::from(10), Decimal::from(100), Decimal::from(10)),
            ("Input".into(), "Standard".into(), Decimal::from(10), Decimal::from(50), Decimal::from(5)),
        ],
        "200 at 10% less the 100 credited, 100 at 5%, the purchase on the input side; the cancelled 300 nowhere"
    );
    // Each group's documents, to drill into.
    let docs: Vec<(String, String, Decimal)> = rows
        .iter()
        .filter(|r| !r["voucher_id"].is_null())
        .map(|r| (r["voucher_type"].as_str().unwrap().to_string(), r["voucher_id"].as_str().unwrap().to_string(), dec(&r["tax"])))
        .collect();
    assert_eq!(
        docs,
        vec![
            ("SalesInvoice".into(), si2.to_string(), Decimal::from(5)),
            ("SalesInvoice".into(), si1.to_string(), Decimal::from(20)),
            ("SalesInvoice".into(), cn.to_string(), Decimal::from(-10)),
            ("PurchaseInvoice".into(), pi.to_string(), Decimal::from(5)),
        ]
    );
    assert_eq!(
        (dec(&report["totals"]["output_tax"]), dec(&report["totals"]["input_tax"]), dec(&report["totals"]["net_tax"])),
        (Decimal::from(15), Decimal::from(5), Decimal::from(10))
    );
    // It is in the Accounting menu.
    let menu = t.k.registry.schema(t.ctx.tenant).await.unwrap();
    let listed = menu.modules.iter().any(|m| m.name == "accounting" && m.items.iter().any(|i| i == "report:tax_summary"));
    assert!(listed, "listed in Accounting");
}
