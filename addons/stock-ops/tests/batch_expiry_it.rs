//! Expired and disabled batches against a real PostgreSQL (`FERRUM_BATCH_EXPIRY_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_batch_expiry_test`; reset first).
//!
//! Stock that leaves for use may not come from a disabled batch or one past its expiry on the
//! posting date; writing it off, a transfer, a return to the supplier, a customer's return and
//! cancelling an earlier posting still work.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

fn text(e: &Error) -> String {
    match e {
        Error::Invalid(m) => m.clone(),
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => format!("{other:?}"),
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
    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    /// Inserts and submits.
    async fn post(&self, entity: &str, v: Value) -> Uuid {
        let d = self.insert(entity, v).await;
        uid(&self.submit(entity, uid(&d)).await)
    }
    /// Inserts and submits; returns the refusal.
    async fn refused(&self, entity: &str, v: Value) -> String {
        let d = self.insert(entity, v).await;
        text(&self.k.submit(&self.ctx, entity, uid(&d), None).await.expect_err("should be refused"))
    }
    async fn action(&self, entity: &str, id: Uuid, name: &str) -> Uuid {
        let v = self.k.run_action(&self.ctx, entity, id, name, json!({})).await.unwrap_or_else(|e| panic!("{entity}.{name}: {e:?}"));
        v["id"].as_str().unwrap().parse().unwrap()
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let page =
            self.k.list(&self.ctx, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() }).await.unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
    async fn batch(&self, batch: &str, wh: Uuid) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        sqlx::query_scalar("select coalesce(sum(qty), 0) from stock_move where tenant_id = $1 and batch_no = $2 and warehouse_id = $3")
            .bind(self.ctx.tenant)
            .bind(batch)
            .bind(wh)
            .fetch_one(&mut *tx)
            .await
            .unwrap()
    }
}

#[test]
fn batch_expiry() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_BATCH_EXPIRY_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_batch_expiry_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("platform packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_stock_ops::packages().expect("app packages"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_stock_ops::register(&mut hooks);
    let k = Kernel::boot(db, Catalog::from_packages(packages).expect("catalog"), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("partitions");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    ferrum_stock_ops::maintenance(&k).await.expect("maintenance");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "batches".into(),
            name: "Batch Co".into(),
            admin_email: "admin@batch.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["stock_ops".into()],
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("batches", "admin@batch.test", "Correct-Horse-9").await.expect("login");
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let t = T { k: k.clone(), ctx };

    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let cid = uid(&t.insert("Company", json!({"company_name": "Batch Co", "abbr": "BC", "default_currency": inr["id"]})).await);
    let co = cid.to_string();
    let wh = |name: &'static str| {
        let t = &t;
        let co = co.clone();
        async move { uid(&t.find_one("Warehouse", json!({"warehouse_name": name, "company": co})).await) }
    };
    let stores = wh("Stores").await;
    let all = wh("All Warehouses").await;
    let quarantine = uid(&t.insert("Warehouse", json!({"warehouse_name": "Quarantine", "company": co, "parent_warehouse": all.to_string()})).await);
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let paste = uid(&t
        .insert("Item", json!({"item_code": "PASTE", "item_name": "paste", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "10", "has_batch_no": true}))
        .await)
    .to_string();
    let customer = uid(&t.insert("Customer", json!({"customer_name": "Acme Retail"})).await).to_string();
    let supplier = uid(&t.insert("Supplier", json!({"supplier_name": "Bolt Supplies"})).await).to_string();
    let today = chrono::Utc::now().date_naive();
    let day = |d: i64| (today + chrono::Duration::days(d)).to_string();
    // OLD expired yesterday, OFF is disabled, GOOD keeps for a year.
    for (batch, expiry, disabled) in [("OLD", -1, false), ("OFF", 365, true), ("GOOD", 365, false)] {
        t.insert("Batch", json!({"batch_id": batch, "item": paste, "expiry_date": day(expiry), "disabled": disabled})).await;
    }
    let s = stores.to_string();
    let q = quarantine.to_string();
    let receipt = |batch: &str, on: String| {
        json!({"company": co, "purpose": "Material Receipt", "posting_date": on,
               "items": [{"item": paste, "qty": "10", "basic_rate": "5", "target_warehouse": s, "batch_no": batch}]})
    };
    // Expired stock may still come in.
    for batch in ["OLD", "OFF", "GOOD"] {
        t.post("StockEntry", receipt(batch, day(-10))).await;
    }
    let delivery = |batch: &str, on: String| {
        json!({"company": co, "customer": customer, "posting_date": on,
               "items": [{"item": paste, "qty": "2", "warehouse": s, "batch_no": batch}]})
    };

    // Sold: refused for the expired and the disabled batch, named with its expiry; the good one goes.
    assert_eq!(
        t.refused("DeliveryNote", delivery("OLD", day(0))).await,
        format!("batch OLD of PASTE expired on {}; write it off, return it or move it to another warehouse instead", day(-1))
    );
    assert!(t.refused("DeliveryNote", delivery("OFF", day(0))).await.starts_with("batch OFF of PASTE is disabled"));
    t.post("DeliveryNote", delivery("GOOD", day(0))).await;

    // On its expiry date it may still leave; that delivery's return comes back in today and its
    // cancellation takes the expired goods out again.
    let dn = t.post("DeliveryNote", delivery("OLD", day(-1))).await;
    let ret = t.action("DeliveryNote", dn, "make_sales_return").await;
    t.submit("DeliveryNote", ret).await;
    assert_eq!(t.batch("OLD", stores).await, Decimal::from(10), "the customer's return came back");
    t.k.cancel(&t.ctx, "DeliveryNote", ret, None).await.expect("cancelling the return takes expired stock out again");
    assert_eq!(t.batch("OLD", stores).await, Decimal::from_str("8").unwrap());

    // Moved to quarantine, written off: allowed for expired and disabled alike.
    let transfer = |batch: &str| json!({"company": co, "purpose": "Material Transfer", "items": [{"item": paste, "qty": "3", "source_warehouse": s, "target_warehouse": q, "batch_no": batch}]});
    t.post("StockEntry", transfer("OLD")).await;
    t.post("StockEntry", transfer("OFF")).await;
    let write_off =
        |batch: &str| json!({"company": co, "purpose": "Material Issue", "items": [{"item": paste, "qty": "1", "source_warehouse": q, "batch_no": batch}]});
    t.post("StockEntry", write_off("OLD")).await;
    t.post("StockEntry", write_off("OFF")).await;
    assert_eq!((t.batch("OLD", quarantine).await, t.batch("OFF", quarantine).await), (Decimal::from(2), Decimal::from(2)));

    // Back to the supplier: a purchase return of an expired batch goes.
    let pr = t
        .post(
            "PurchaseReceipt",
            json!({"company": co, "supplier": supplier, "posting_date": day(-10),
                   "items": [{"item": paste, "qty": "4", "price_list_rate": "5", "warehouse": s, "batch_no": "OLD"}]}),
        )
        .await;
    let back = t.action("PurchaseReceipt", pr, "make_purchase_return").await;
    t.submit("PurchaseReceipt", back).await;
    assert_eq!(t.batch("OLD", stores).await, Decimal::from(5), "8 - 3 moved + 4 received - 4 returned");
}
