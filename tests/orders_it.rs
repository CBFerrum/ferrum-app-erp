//! Orders that change, allowances, credit control and the statement of account, end to end against
//! a real PostgreSQL (`FERRUM_ERP_ORDERS_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_orders_test`; reset first).

use async_trait::async_trait;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, KernelSettings, ListQuery, Result};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn dec(v: &Value) -> Decimal {
    match v {
        Value::String(s) => Decimal::from_str(s).unwrap_or_else(|_| panic!("not a decimal: {s}")),
        Value::Number(n) => Decimal::from_str(&n.to_string()).unwrap(),
        Value::Null => Decimal::ZERO,
        other => panic!("not a decimal: {other}"),
    }
}

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

fn vid(v: &Value) -> Uuid {
    v["id"].as_str().unwrap().parse().unwrap()
}

fn refused<T: std::fmt::Debug>(r: Result<T>, words: &str) {
    let text = match &r {
        Err(Error::Invalid(m)) => m.clone(),
        Err(Error::Validation(e)) => e.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => panic!("expected a refusal with {words:?}, got {other:?}"),
    };
    assert!(text.contains(words), "{words:?} not in {text:?}");
}

/// A hook of the kind another app registers on sales orders (CRM's "an order keeps what was
/// quoted"): it refuses, at submission, more than 50 of anything.
struct Quoted;

#[async_trait]
impl DocHook for Quoted {
    async fn before_submit(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if doc["items"].as_array().unwrap().iter().any(|l| dec(&l["qty"]) > n("50")) {
            return Err(Error::invalid("more than the 50 quoted"));
        }
        Ok(())
    }
}

struct T {
    k: Arc<Kernel>,
    ctx: Ctx,
}

impl T {
    async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.k.insert(&self.ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn update(&self, entity: &str, id: Uuid, v: Value) -> Doc {
        self.k.update(&self.ctx, entity, id, obj(v), None).await.unwrap_or_else(|e| panic!("update {entity}: {e:?}"))
    }
    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.ctx, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    async fn action(&self, entity: &str, id: Uuid, action: &str, params: Value) -> Value {
        self.k.run_action(&self.ctx, entity, id, action, params).await.unwrap_or_else(|e| panic!("{entity}.{action}: {e:?}"))
    }
    async fn change(&self, entity: &str, id: Uuid, params: Value) -> Result<Value> {
        self.k.run_action(&self.ctx, entity, id, "update_items", params).await
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let page =
            self.k.list(&self.ctx, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() }).await.unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
    async fn scalar(&self, sql: &'static str, a: Uuid) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        let v: Option<Decimal> = sqlx::query_scalar(sql).bind(self.ctx.tenant).bind(a).fetch_one(&mut *tx).await.unwrap();
        v.unwrap_or_default()
    }
    async fn reserved(&self, item: Uuid) -> Decimal {
        self.scalar("select sum(reserved_qty) from stock_balance where tenant_id = $1 and item_id = $2", item).await
    }
    async fn report(&self, name: &str, params: &[(&str, String)]) -> Value {
        let raw = params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        ferrum_domain::reports::run(&self.k, &self.ctx, name, &raw).await.unwrap_or_else(|e| panic!("report {name}: {e:?}"))
    }
}

fn line(doc: &Doc, i: usize) -> Doc {
    doc["items"][i].as_object().cloned().unwrap()
}

#[test]
fn orders_allowances_credit_and_statements() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_ORDERS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_orders_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    hooks.register("SalesOrder", Arc::new(Quoted));
    for source in ferrum_app_erp::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let k: Arc<Kernel> = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "oo".into(),
            name: "Orders Co".into(),
            admin_email: "admin@oo.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying"].iter().map(|s| s.to_string()).collect(),
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("oo", "admin@oo.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), ctx };

    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let cid = uid(&t.insert("Company", json!({"company_name": "Orders Co", "abbr": "OC", "default_currency": inr["id"]})).await);
    let c = cid.to_string();
    let wh = t.find_one("Warehouse", json!({"warehouse_name": "Stores", "company": c})).await["id"].as_str().unwrap().to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |code: &str, rate: &str| json!({"item_code": code, "item_name": code, "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": rate});
    let widget = uid(&t.insert("Item", item("Widget", "100")).await);
    let gadget = uid(&t.insert("Item", item("Gadget", "50")).await);
    let steel = uid(&t.insert("Item", item("Steel", "0")).await);
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await);
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await);
    for it in [widget, gadget] {
        let se = t
            .insert("StockEntry", json!({"company": c, "purpose": "Material Receipt", "items": [{"item": it.to_string(), "qty": "500", "basic_rate": "40", "target_warehouse": wh}]}))
            .await;
        t.submit("StockEntry", uid(&se)).await;
    }

    // ───────────── a sales order half delivered and half billed, then changed ─────────────
    let so = uid(&t
        .insert(
            "SalesOrder",
            json!({"company": c, "customer": globex.to_string(), "set_warehouse": wh, "delivery_date": "2026-12-01",
                   "items": [{"item": widget.to_string(), "qty": "10"}, {"item": gadget.to_string(), "qty": "4"}]}),
        )
        .await);
    t.submit("SalesOrder", so).await;
    let dn = vid(&t.action("SalesOrder", so, "make_delivery_note", json!({})).await);
    let dn_doc = t.get("DeliveryNote", dn).await;
    t.update("DeliveryNote", dn, json!({"items": [{"id": line(&dn_doc, 0)["id"], "qty": "5"}]})).await;
    t.submit("DeliveryNote", dn).await;
    let si = vid(&t.action("DeliveryNote", dn, "make_sales_invoice", json!({})).await);
    t.submit("SalesInvoice", si).await;
    let order = t.get("SalesOrder", so).await;
    assert_eq!((dec(&line(&order, 0)["delivered_qty"]), dec(&line(&order, 0)["billed_qty"]), dec(&order["grand_total"])), (n("5"), n("5"), n("1200")));
    assert_eq!(t.reserved(widget).await, n("5"));

    // More of a line, a line added, a line removed, the date moved.
    let out = t.change("SalesOrder", so, json!({"line": 1, "qty": "12"})).await.unwrap();
    assert!(out["changes"].to_string().contains("quantity 10 → 12"), "{out}");
    t.change("SalesOrder", so, json!({"description": "Installation", "qty": "1", "price": "200", "delivery_date": "2026-12-05"})).await.unwrap();
    let order = t.get("SalesOrder", so).await;
    assert_eq!((dec(&order["grand_total"]), order["status"].clone()), (n("1600"), json!("To Deliver and Bill")), "12 × 100 + 4 × 50 + 200");
    assert_eq!((order["delivery_date"].clone(), line(&order, 0)["delivery_date"].clone()), (json!("2026-12-05"), json!("2026-12-05")));
    assert_eq!(t.reserved(widget).await, n("7"), "the reservation follows");
    t.change("SalesOrder", so, json!({"line": 2, "qty": "0"})).await.unwrap();
    let order = t.get("SalesOrder", so).await;
    assert_eq!((order["items"].as_array().unwrap().len(), dec(&order["grand_total"])), (2, n("1400")));
    assert_eq!(t.reserved(gadget).await, Decimal::ZERO, "a removed line holds nothing");
    // What is refused.
    refused(t.change("SalesOrder", so, json!({"line": 1, "qty": "0"})).await, "(Widget) cannot be removed: 5 delivered and 5 billed on it; cut it to 5");
    refused(t.change("SalesOrder", so, json!({"line": 1, "qty": "4"})).await, "(Widget) cannot go below 5");
    refused(t.change("SalesOrder", so, json!({"line": 1, "price": "90"})).await, "takes its price from the price list");
    refused(t.change("SalesOrder", so, json!({"line": 9, "qty": "1"})).await, "there is no line 9");
    // Cut to what was delivered: the widget is done, the installation is still to bill.
    t.change("SalesOrder", so, json!({"line": 1, "qty": "5"})).await.unwrap();
    let order = t.get("SalesOrder", so).await;
    assert_eq!((dec(&order["grand_total"]), order["status"].clone(), dec(&order["per_delivered"])), (n("700"), json!("To Bill"), n("100")));
    assert_eq!(t.reserved(widget).await, Decimal::ZERO);
    let audited = t.scalar("select count(*)::numeric from audit_event where tenant_id = $1 and record_id = $2 and action = 'update_items'", so).await;
    assert_eq!(audited, n("4"), "each change is one audit event");
    // Close and re-open: what was closed short stays visible until then.
    t.action("SalesOrder", so, "close", json!({})).await;
    let order = t.get("SalesOrder", so).await;
    assert_eq!((order["status"].clone(), dec(&line(&order, 1)["closed_qty"])), (json!("Closed"), n("1")));
    refused(t.change("SalesOrder", so, json!({"line": 2, "qty": "2"})).await, "is closed: re-open it");
    t.action("SalesOrder", so, "reopen", json!({})).await;
    let order = t.get("SalesOrder", so).await;
    assert_eq!((order["status"].clone(), dec(&line(&order, 1)["closed_qty"])), (json!("To Bill"), Decimal::ZERO));
    let si2 = vid(&t.action("SalesOrder", so, "make_sales_invoice", json!({})).await);
    t.submit("SalesInvoice", si2).await;
    refused(t.change("SalesOrder", so, json!({"line": 1, "qty": "6"})).await, "is completed");

    // Another app's rule holds after a change as at submission.
    let quoted = uid(&t.insert("SalesOrder", json!({"company": c, "customer": globex.to_string(), "set_warehouse": wh, "items": [{"item": widget.to_string(), "qty": "40"}]})).await);
    t.submit("SalesOrder", quoted).await;
    refused(t.change("SalesOrder", quoted, json!({"line": 1, "qty": "60"})).await, "more than the 50 quoted");
    assert_eq!(dec(&line(&t.get("SalesOrder", quoted).await, 0)["qty"]), n("40"), "nothing changed");
    t.change("SalesOrder", quoted, json!({"line": 1, "qty": "50"})).await.unwrap();

    // ───────────── a purchase order's price changed for what is not yet billed ─────────────
    let po = uid(&t
        .insert("PurchaseOrder", json!({"company": c, "supplier": acme.to_string(), "set_warehouse": wh, "items": [{"item": steel.to_string(), "qty": "10", "price_list_rate": "10"}]}))
        .await);
    t.submit("PurchaseOrder", po).await;
    let pi = vid(&t.action("PurchaseOrder", po, "make_purchase_invoice", json!({})).await);
    let pi_doc = t.get("PurchaseInvoice", pi).await;
    t.update("PurchaseInvoice", pi, json!({"bill_no": "A-1", "items": [{"id": line(&pi_doc, 0)["id"], "qty": "4"}]})).await;
    assert_eq!(dec(&t.submit("PurchaseInvoice", pi).await["grand_total"]), n("40"));
    let out = t.change("PurchaseOrder", po, json!({"line": 1, "price": "12"})).await.unwrap();
    assert!(out["changes"].to_string().contains("price 10 → 12, for the 6 not yet billed"), "{out}");
    assert_eq!(dec(&t.get("PurchaseOrder", po).await["grand_total"]), n("120"));
    let pi2 = vid(&t.action("PurchaseOrder", po, "make_purchase_invoice", json!({})).await);
    t.update("PurchaseInvoice", pi2, json!({"bill_no": "A-2"})).await;
    let pi2_doc = t.submit("PurchaseInvoice", pi2).await;
    assert_eq!((dec(&line(&pi2_doc, 0)["qty"]), dec(&pi2_doc["grand_total"])), (n("6"), n("72")), "the rest at the new price");
    assert_eq!(t.get("PurchaseOrder", po).await["status"], json!("To Receive"));
    refused(t.change("PurchaseOrder", po, json!({"line": 1, "price": "13"})).await, "is fully billed");
}
