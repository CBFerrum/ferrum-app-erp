//! One warehouse, receiving through returns, against a real PostgreSQL
//! (`FERRUM_WMS_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_wms_test`;
//! reset first). Through the same endpoints scanners use, with ERP's real documents underneath, so
//! that what is checked at the end is what the assessment asks for: stock reconciles by item, lot,
//! owner, status and location, and the books agree.

use chrono::{Duration, Utc};
use ferrum_kernel::query::parse_filters;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, SiteRequest};
use ferrum_meta::Catalog;
use ferrum_ops::commands;
use ferrum_ops::contract::command::Report;
use ferrum_ops::contract::{Lifecycle, State};
use ferrum_ops::devices;
use ferrum_wms::position;
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

/// The one gateway the site's commands go through: its domains are not pooled.
const GATEWAY: Uuid = Uuid::from_u128(0x6a7e);

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn uid(d: &Doc) -> Uuid {
    d["id"].as_str().unwrap().parse().unwrap()
}

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn dec(v: &Value) -> Decimal {
    match v {
        Value::String(s) => n(s),
        Value::Number(x) => n(&x.to_string()),
        _ => Decimal::ZERO,
    }
}

struct T {
    k: Arc<Kernel>,
    tenant: Uuid,
    admin: Ctx,
    wh: Uuid,
}

impl T {
    async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.k.insert(&self.admin, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }

    async fn find(&self, entity: &str, filters: Value) -> Doc {
        let q = ListQuery { filters: parse_filters(&filters).unwrap(), ..Default::default() };
        let page = self.k.list(&self.admin, entity, &q).await.unwrap();
        page.rows.into_iter().next().unwrap_or_else(|| panic!("no {entity} {filters}"))
    }

    async fn list(&self, entity: &str, filters: Value) -> Vec<Doc> {
        let q = ListQuery { filters: parse_filters(&filters).unwrap(), limit: Some(200), ..Default::default() };
        self.k.list(&self.admin, entity, &q).await.unwrap().rows
    }

    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.admin, entity, id, false).await.unwrap()
    }

    async fn api_as(&self, ctx: &Ctx, method: &str, path: &str, body: Value) -> Result<Value, Error> {
        self.call(ctx, method, path, Map::new(), body).await
    }

    async fn call(&self, ctx: &Ctx, method: &str, path: &str, query: Map<String, Value>, body: Value) -> Result<Value, Error> {
        let site = self.k.hooks.site("wms").unwrap().clone();
        let req = SiteRequest {
            tenant: self.tenant,
            user: Some(ctx.clone()),
            method: method.into(),
            path: path.into(),
            query,
            headers: Map::new(),
            body: body.to_string().into_bytes(),
            client_ip: "::1".into(),
        };
        site.handle(&self.k, req).await.map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
    }

    async fn tasks(&self) -> Vec<Value> {
        let q = obj(json!({"warehouse": self.wh}));
        self.call(&self.admin, "GET", "/tasks", q, Value::Null).await.unwrap()["tasks"].as_array().unwrap().clone()
    }

    async fn task_for(&self, kind: &str, item: &str) -> Value {
        self.tasks().await.into_iter().find(|t| t["kind"] == json!(kind) && t["item"] == json!(item)).unwrap_or_else(|| panic!("no {kind} of {item}"))
    }

    /// Everything in a bin of one item: (status, qty, allocated).
    async fn bin(&self, code: &str, item: &str) -> Vec<(String, Decimal, Decimal)> {
        let q = obj(json!({"warehouse": self.wh}));
        let v = self.call(&self.admin, "GET", &format!("/bins/{code}"), q, Value::Null).await.unwrap();
        v["positions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["item_code"] == json!(item))
            .map(|p| (p["status"].as_str().unwrap().to_string(), dec(&p["qty"]), dec(&p["allocated"])))
            .collect()
    }

    async fn qty_in(&self, code: &str, item: &str) -> Decimal {
        self.bin(code, item).await.iter().map(|p| p.1).sum()
    }

    async fn on_books(&self, item: Uuid) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let q: Option<Decimal> = sqlx::query_scalar("select qty from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = $3")
            .bind(self.tenant)
            .bind(item)
            .bind(self.wh)
            .fetch_optional(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        q.unwrap_or_default()
    }

    async fn reconciled(&self) {
        let q = obj(json!({"warehouse": self.wh}));
        let v = self.call(&self.admin, "GET", "/reconcile", q, Value::Null).await.unwrap();
        assert_eq!(v, json!({"books": [], "ledger": []}), "the warehouse, the ledger and the books agree");
    }

    async fn person(&self, email: &str, roles: &[&str]) -> Ctx {
        let id = self
            .k
            .create_user(&self.admin, email, "Someone", Some("Correct-Horse-9"), &roles.iter().map(|r| r.to_string()).collect::<Vec<_>>(), false)
            .await
            .expect("user");
        self.k.load_ctx(self.tenant, id, None, "it".into()).await.unwrap()
    }

    /// How many times something was recorded, read apart from whatever request wrote it.
    async fn audited(&self, entity: &str, record: Uuid, action: &str) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let n = sqlx::query_scalar("select count(*) from audit_event where tenant_id = $1 and entity = $2 and record_id = $3 and action = $4")
            .bind(self.tenant)
            .bind(entity)
            .bind(record)
            .bind(action)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        n
    }

    /// Commands written for a task, whatever became of them.
    async fn missions(&self, task: Uuid) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let n = sqlx::query_scalar("select count(*) from op_command where tenant_id = $1 and workflow_id = $2")
            .bind(self.tenant)
            .bind(format!("wms-task:{task}"))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        n
    }

    /// What a site does for a robot: says what it established, in the ledger, and lets it travel.
    async fn robot_reports(&self, command: Uuid, state: State, evidence: Option<Value>) {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let r = Report { report_id: Uuid::now_v7(), command_id: command, state, at_source: Utc::now(), epoch: 1, detail: Value::Null, evidence };
        commands::apply(&mut tx, self.tenant, &r, 1, GATEWAY).await.unwrap();
        tx.commit().await.unwrap();
        while self.k.dispatch_outbox_once().await.unwrap() > 0 {}
    }
}

fn text(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

#[test]
fn a_warehouse_from_the_dock_to_the_door() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(32 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_WMS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_wms_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("platform");
    packages.extend(ferrum_app_erp::packages().expect("ERP"));
    packages.extend(ferrum_stock_ops::packages().expect("stock ops"));
    packages.extend(ferrum_ops::packages().expect("ops"));
    packages.extend(ferrum_quality::packages().expect("quality"));
    packages.extend(ferrum_wms::packages().expect("wms"));
    let catalog = Catalog::from_packages(packages).unwrap();
    assert_eq!(catalog.links_out_of_reach("wms"), Vec::<String>::new(), "whoever fills in a link can look up what it points at");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_stock_ops::register(&mut hooks);
    ferrum_ops::register(&mut hooks);
    ferrum_quality::register(&mut hooks);
    ferrum_wms::register(&mut hooks);
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("partitions");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "dock".into(),
            name: "Dock Co".into(),
            admin_email: "admin@dock.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["wms".into()],
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("tenant");
    let session = k.login("dock", "admin@dock.test", "Correct-Horse-9").await.expect("login");
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let mut t = T { k: k.clone(), tenant, admin, wh: Uuid::nil() };

    // ───── a company, a warehouse, and what it already holds ─────
    let inr = t.find("Currency", json!({"code": "INR"})).await;
    let company = uid(&t.insert("Company", json!({"company_name": "Dock Co", "abbr": "DOC", "default_currency": inr["id"]})).await);
    t.wh = uid(&t.find("Warehouse", json!({"warehouse_name": "Stores", "company": company.to_string()})).await);
    let wh = t.wh;
    let group = t.find("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find("Uom", json!({"uom_name": "Nos"})).await;
    let item = async |code: &str, extra: Value| {
        let mut v = json!({"item_code": code, "item_name": code.to_lowercase(), "item_group": group["id"], "stock_uom": nos["id"], "is_stock_item": true});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        uid(&t.insert("Item", v).await)
    };
    let bolt = item("BOLT", json!({"wms_weight_kg": "0.5"})).await;
    let milk = item("MILK", json!({"has_batch_no": true})).await;
    let nut = item("NUT", json!({})).await;
    let acid = item("ACID", json!({"wms_hazard_class": "8"})).await;
    let supplier = uid(&t.insert("Supplier", json!({"supplier_name": "Bolt Supplies"})).await);
    let customer = uid(&t.insert("Customer", json!({"customer_name": "Acme Retail"})).await);
    let tenant_customer = uid(&t.insert("Customer", json!({"customer_name": "Stored Goods Ltd"})).await);

    let opening = t
        .insert(
            "StockEntry",
            json!({"company": company, "purpose": "Material Receipt", "items": [{"item": nut, "qty": 40, "basic_rate": 2, "target_warehouse": wh}]}),
        )
        .await;
    k.submit(&t.admin, "StockEntry", uid(&opening), None).await.expect("opening stock");

    let zone = async |code: &str, kind: &str| uid(&t.insert("StorageZone", json!({"warehouse": wh, "zone_code": code, "kind": kind})).await);
    let (rcv, sto, pck, shp, qua) = (
        zone("RCV", "Receiving").await,
        zone("STO", "Storage").await,
        zone("PCK", "Picking").await,
        zone("SHP", "Shipping").await,
        zone("QUA", "Quarantine").await,
    );
    let bin = async |code: &str, zone: Uuid, extra: Value| {
        let mut v = json!({"warehouse": wh, "zone": zone, "bin_code": code});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        uid(&t.insert("StorageBin", v).await)
    };
    let (dock, door, jail) =
        (bin("DOCK", rcv, json!({"kind": "Dock"})).await, bin("DOOR", shp, json!({"kind": "Dock"})).await, bin("JAIL", qua, json!({})).await);
    let a1 = bin("A-01", sto, json!({"pick_sequence": 1})).await;
    let a2 = bin("A-02", sto, json!({"pick_sequence": 2, "max_units": 10})).await;
    let p1 = bin("P-01", pck, json!({"pick_sequence": 3})).await;
    let site = uid(&t.insert("Site", json!({"site_name": "North", "site_code": "north"})).await);
    let managed = uid(&t
        .insert(
            "ManagedWarehouse",
            json!({"warehouse": wh, "site": site, "inbound_bin": dock, "outbound_bin": door, "quarantine_bin": jail, "allocation_rule": "FEFO"}),
        )
        .await);
    let placed = k.run_action(&t.admin, "ManagedWarehouse", managed, "start_managing", json!({})).await.expect("managed");
    assert_eq!(placed["placed"], json!(1));
    assert_eq!(t.qty_in("DOCK", "NUT").await, n("40"), "what ERP held is at the door, until someone walks it");
    t.reconciled().await;
    let _ = (a2, p1);

    let operator = t.person("op@dock.test", &["Warehouse Operator"]).await;
    let manager = t.person("mgr@dock.test", &["Warehouse Manager"]).await;
    let viewer = t.person("view@dock.test", &["Warehouse Viewer"]).await;

    // ───── two balances cannot exist: anything that gets around this app fails at commit ─────
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update wms_position set qty = qty + 1 where tenant_id = $1 and item_id = $2").bind(tenant).bind(nut).execute(&mut *tx).await.unwrap();
    let e = tx.commit().await.expect_err("positions alone cannot change");
    assert!(e.to_string().contains("stock and positions differ"), "{e}");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update stock_balance set qty = qty + 1 where tenant_id = $1 and item_id = $2 and warehouse_id = $3")
        .bind(tenant)
        .bind(nut)
        .bind(wh)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(tx.commit().await.is_err(), "nor can the books");
    // The ledger is append-only.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert!(sqlx::query("delete from wms_move where tenant_id = $1").bind(tenant).execute(&mut *tx).await.is_err());
    drop(tx);

    // ───── ERP cannot issue what nobody picked ─────
    let refused =
        t.insert("StockEntry", json!({"company": company, "purpose": "Material Issue", "items": [{"item": nut, "qty": 5, "source_warehouse": wh}]})).await;
    let e = k.submit(&t.admin, "StockEntry", uid(&refused), None).await.expect_err("nothing is at the outbound bin");
    assert!(text(&e).contains("picked to the outbound bin"), "{e:?}");
    assert_eq!(t.on_books(nut).await, n("40"), "and the whole document rolled back");

    // ───── receiving ─────
    let inbound = t
        .insert(
            "InboundShipment",
            json!({"company": company, "warehouse": wh, "supplier": supplier, "reference": "DN-77", "items": [
        {"item": bolt, "expected_qty": 100, "rate": 5},
        {"item": milk, "expected_qty": 10, "lot": "M-1", "expiry": (Utc::now() + Duration::days(40)).date_naive().to_string(), "rate": 3},
        {"item": milk, "expected_qty": 10, "lot": "M-2", "expiry": (Utc::now() + Duration::days(10)).date_naive().to_string(), "rate": 3}]}),
        )
        .await;
    let asn = uid(&inbound);
    assert!(
        k.update(&operator, "InboundShipment", asn, obj(json!({"reference": "DN-78"})), None).await.is_err(),
        "a shipment changes through its scans, not by editing it"
    );
    let scan = Uuid::now_v7();
    let path = format!("/inbound/{asn}/scan");
    assert!(
        t.api_as(&operator, "POST", &path, json!({"scan_id": Uuid::now_v7(), "code": "OLT", "qty": 1}))
            .await
            .is_err_and(|e| text(&e).contains("not on this shipment")),
        "part of a code is no match"
    );
    let body = json!({"scan_id": scan, "code": "BOLT", "qty": 96});
    assert_eq!(dec(&t.api_as(&operator, "POST", &path, body.clone()).await.unwrap()["received"]), n("96"));
    assert_eq!(t.api_as(&operator, "POST", &path, body).await.unwrap()["repeated"], json!(true), "a scanner repeating itself counts once");
    let over = t.api_as(&operator, "POST", &path, json!({"scan_id": Uuid::now_v7(), "code": "bolt", "qty": 10})).await;
    assert!(matches!(over, Err(Error::Forbidden(ref m)) if m.contains("manager")), "{over:?}");
    t.api_as(&operator, "POST", &path, json!({"scan_id": Uuid::now_v7(), "code": "BOLT", "damaged": 4})).await.expect("damaged");
    assert!(t.api_as(&viewer, "POST", &path, json!({"scan_id": Uuid::now_v7(), "code": "M-1", "qty": 1})).await.is_err(), "a viewer does not receive");
    for lot in ["M-1", "M-2"] {
        t.api_as(&operator, "POST", &path, json!({"scan_id": Uuid::now_v7(), "code": lot, "qty": 10})).await.expect("milk");
    }
    t.api_as(&operator, "POST", &format!("/inbound/{asn}/unknown"), json!({"description": "A pallet with no label", "qty": 1})).await.expect("noted");
    assert!(matches!(t.api_as(&operator, "POST", &format!("/inbound/{asn}/close"), Value::Null).await, Err(Error::Forbidden(_))));
    let closed = t.api_as(&manager, "POST", &format!("/inbound/{asn}/close"), Value::Null).await.expect("closed");
    assert_eq!(closed["status"], json!("Discrepancy"), "the unknown pallet is still somebody's to sort out");
    assert_eq!(t.on_books(bolt).await, n("100"), "ERP received it, damaged included: it is here");
    assert!(
        t.api_as(&manager, "POST", &format!("/inbound/{asn}/close"), Value::Null).await.is_err_and(|e| text(&e).contains("already received")),
        "a shipment that closed with a difference does not receive twice"
    );
    assert_eq!(t.on_books(bolt).await, n("100"));
    assert!(
        t.api_as(&operator, "POST", &format!("/inbound/{asn}/settle"), json!({"note": "Returned to sender"})).await.is_err(),
        "settling a difference is a manager's"
    );
    assert!(t.api_as(&manager, "POST", &format!("/inbound/{asn}/settle"), json!({"note": " "})).await.is_err(), "and says what became of it");
    t.api_as(&manager, "POST", &format!("/inbound/{asn}/settle"), json!({"note": "Returned to sender"})).await.expect("settled");
    let settled = t.get("InboundShipment", asn).await;
    assert_eq!(settled["status"], json!("Received"));
    assert!(settled["notes"].as_str().unwrap().contains("Returned to sender"), "{settled:?}");
    assert_eq!(t.qty_in("DOCK", "BOLT").await, n("96"));
    assert_eq!(t.bin("JAIL", "BOLT").await, vec![("Damaged".to_string(), n("4"), Decimal::ZERO)], "damaged goods are out of reach");
    t.reconciled().await;

    // ───── putaway: the bin is checked whoever chooses it ─────
    let task = t.task_for("Putaway", "BOLT").await;
    let id = task["id"].as_str().unwrap().to_string();
    assert_eq!(task["to_bin"], json!("A-01"), "the emptiest bin that fits, {task}");
    assert_eq!(t.bin("DOCK", "BOLT").await[0].2, n("96"), "promised to the task, so nothing else can take it");
    let wrong = t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), json!({"from_scan": "A-02", "to_scan": "A-01"})).await;
    assert!(wrong.is_err(), "the wrong source");
    let small = t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), json!({"from_scan": "DOCK", "to_scan": "A-02"})).await;
    assert!(small.is_err_and(|e| text(&e).contains("no room")), "A-02 holds ten");
    k.update(&t.admin, "StorageBin", a1, obj(json!({"status": "Blocked", "blocked_reason": "racking damaged"})), None).await.unwrap();
    let put_scan = Uuid::now_v7();
    assert!(
        t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), json!({"from_scan": "DOCK", "to_scan": "P-01", "item_scan": "BOLT", "scan_id": put_scan}))
            .await
            .is_ok(),
        "an operator may choose another that fits"
    );
    k.update(&t.admin, "StorageBin", a1, obj(json!({"status": "Open"})), None).await.unwrap();
    assert_eq!(t.qty_in("P-01", "BOLT").await, n("96"));
    assert!(t.qty_in("DOCK", "BOLT").await.is_zero());
    t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), json!({"to_scan": "P-01", "scan_id": put_scan}))
        .await
        .expect("the same scan again, after a lost answer, does nothing");
    assert!(
        t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), json!({"to_scan": "P-01", "scan_id": Uuid::now_v7()}))
            .await
            .is_err_and(|e| text(&e).contains("is Done")),
        "another scan of a finished task is someone who thinks it is open"
    );
    assert_eq!(t.qty_in("P-01", "BOLT").await, n("96"));
    t.reconciled().await;
    for (lot, expect) in [("M-1", "A-01"), ("M-2", "A-01")] {
        let task = t.tasks().await.into_iter().find(|x| x["kind"] == json!("Putaway") && x["lot"] == json!(lot)).unwrap();
        assert_eq!(task["to_bin"], json!(expect));
        t.api_as(
            &operator,
            "POST",
            &format!("/tasks/{}/confirm", task["id"].as_str().unwrap()),
            json!({"to_scan": expect, "item_scan": "MILK", "lot_scan": lot}),
        )
        .await
        .expect("put away");
    }

    // Hazard rules are not hints.
    let hazard = t
        .api_as(&manager, "POST", "/positions/move", json!({"warehouse": wh, "item": acid, "from_bin": "DOCK", "to_bin": "A-01", "qty": 1, "reason": "x"}))
        .await;
    assert!(hazard.is_err_and(|e| text(&e).contains("hazard")));

    // ───── a wave, by first to expire ─────
    let order = t
        .insert(
            "SalesOrder",
            json!({"company": company, "customer": customer, "delivery_date": Utc::now().date_naive().to_string(), "set_warehouse": wh, "items": [
        {"item": milk, "qty": 15, "rate": 9}, {"item": bolt, "qty": 90, "rate": 9}]}),
        )
        .await;
    k.submit(&t.admin, "SalesOrder", uid(&order), None).await.expect("order");
    let wave = uid(&t.insert("PickWave", json!({"company": company, "warehouse": wh, "orders": [{"sales_order": uid(&order)}]})).await);
    assert!(k.run_action(&viewer, "PickWave", wave, "release", json!({})).await.is_err());
    let released = k.run_action(&manager, "PickWave", wave, "release", json!({})).await.expect("released");
    assert_eq!(released["shortfall"], json!([]), "{released}");
    let picks: Vec<Value> = t.tasks().await.into_iter().filter(|x| x["kind"] == json!("Pick")).collect();
    let milk_picks: Vec<(&str, Decimal)> = picks.iter().filter(|p| p["item"] == json!("MILK")).map(|p| (p["lot"].as_str().unwrap(), dec(&p["qty"]))).collect();
    assert_eq!(milk_picks.iter().find(|p| p.0 == "M-2").unwrap().1, n("10"), "the lot that expires first goes first: {milk_picks:?}");
    assert_eq!(milk_picks.iter().find(|p| p.0 == "M-1").unwrap().1, n("5"), "then the rest from the next");
    // What is promised cannot be promised again.
    let order2 = t.insert("SalesOrder", json!({"company": company, "customer": customer, "delivery_date": Utc::now().date_naive().to_string(), "set_warehouse": wh, "items": [{"item": bolt, "qty": 50, "rate": 9}]})).await;
    k.submit(&t.admin, "SalesOrder", uid(&order2), None).await.unwrap();
    let wave2 = uid(&t.insert("PickWave", json!({"company": company, "warehouse": wh, "orders": [{"sales_order": uid(&order2)}]})).await);
    let second = k.run_action(&manager, "PickWave", wave2, "release", json!({})).await.expect("second");
    assert_eq!(dec(&second["shortfall"][0]["allocated"]), n("6"), "only what was free, and the shortfall is written down: {second}");
    assert_eq!(dec(&second["shortfall"][0]["wanted"]), n("50"));
    // People can take what they want off the shelf, but not what a task is counting on.
    let robbed = t
        .api_as(
            &operator,
            "POST",
            "/positions/move",
            json!({"warehouse": wh, "item": bolt, "from_bin": "P-01", "to_bin": "A-02", "qty": 3, "reason": "tidying"}),
        )
        .await;
    assert!(robbed.is_err_and(|e| text(&e).contains("promised")));
    let no_reason = t
        .api_as(&operator, "POST", "/positions/move", json!({"warehouse": wh, "item": nut, "from_bin": "DOCK", "to_bin": "A-02", "qty": 3, "reason": " "}))
        .await;
    assert!(no_reason.is_err(), "a move with no task says why");

    // ───── picking, and a short pick ─────
    let picks: Vec<Value> = t.tasks().await.into_iter().filter(|x| x["kind"] == json!("Pick")).collect();
    for p in &picks {
        let id = p["id"].as_str().unwrap();
        let is_short = p["item"] == json!("BOLT") && dec(&p["qty"]) == n("90");
        let wrong_door = t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), json!({"to_scan": "A-02"})).await;
        assert!(wrong_door.is_err(), "picked stock goes where packing expects it");
        let load = json!({"item_scan": p["item"], "lot_scan": p["lot"]});
        let picked = |extra: Value| {
            let mut body = json!({"to_scan": "DOOR", "item_scan": load["item_scan"], "lot_scan": load["lot_scan"]});
            for (k, v) in extra.as_object().unwrap() {
                body[k] = v.clone();
            }
            body
        };
        let body = if is_short { picked(json!({"qty": 88})) } else { picked(json!({})) };
        let done = t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), body.clone()).await;
        if is_short {
            assert!(done.is_err(), "less than asked says why");
            t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), picked(json!({"qty": 88, "reason": "Two were bent"}))).await.expect("short");
        } else {
            done.expect("picked");
        }
    }
    assert_eq!(t.qty_in("DOOR", "BOLT").await, n("94"), "88 and the six the second wave found");
    assert_eq!(t.bin("P-01", "BOLT").await[0].2, Decimal::ZERO, "a short pick gives back what it did not take");
    t.reconciled().await;
    assert_eq!(k.get(&t.admin, "PickWave", wave, false).await.unwrap()["status"], json!("Picked"));

    // ───── packing and the door ─────
    let shipments = t.list("Shipment", json!({"customer": customer.to_string()})).await;
    let mut ship = Uuid::nil();
    for s in &shipments {
        let full = t.get("Shipment", uid(s)).await;
        if full["items"].as_array().unwrap().iter().any(|l| dec(&l["qty"]) == n("90")) {
            ship = uid(s);
        }
    }
    let wrong_item = t.api_as(&operator, "POST", &format!("/shipments/{ship}/verify"), json!({"code": "NUT", "qty": 1})).await;
    assert!(wrong_item.is_err(), "not on this shipment");
    assert_eq!(t.audited("Shipment", ship, "wms.wrong_scan").await, 1, "and what was refused stays on record");
    let hand = |tracking: &str, label: &str| json!({"carrier": "DHL", "tracking": tracking, "label": label});
    assert!(t.api_as(&operator, "POST", &format!("/shipments/{ship}/hand-over"), hand("T1", "T1")).await.is_err(), "nothing verified, nothing leaves");
    t.api_as(&operator, "POST", &format!("/shipments/{ship}/verify"), json!({"code": "BOLT", "qty": 88})).await.expect("verified");
    assert!(t.api_as(&operator, "POST", &format!("/shipments/{ship}/verify"), json!({"code": "BOLT", "qty": 5})).await.is_err(), "more than was picked for it");
    assert!(t.api_as(&operator, "POST", &format!("/shipments/{ship}/hand-over"), hand("T1", "T2")).await.is_err_and(|e| text(&e).contains("label")));
    let went = t.api_as(&operator, "POST", &format!("/shipments/{ship}/hand-over"), hand("T1", "T1")).await.expect("handed over");
    assert_eq!(went["whole"], json!(false), "a partial shipment says it is one");
    let dn = t.get("DeliveryNote", went["delivery_note"].as_str().unwrap().parse().unwrap()).await;
    assert_eq!(dn["docstatus"], json!(1));
    assert_eq!(t.on_books(bolt).await, n("12"), "the books follow what left");
    assert_eq!(t.qty_in("DOOR", "BOLT").await, n("6"), "and only that left");
    assert_eq!(t.get("Shipment", ship).await["status"], json!("Partially Handed Over"));
    t.reconciled().await;
    let again = t.api_as(&operator, "POST", &format!("/shipments/{ship}/hand-over"), hand("T2", "T2")).await;
    assert!(again.is_err_and(|e| text(&e).contains("verified")), "what has gone does not go again");
    assert_eq!(t.on_books(bolt).await, n("12"), "and the books did not move");

    // ───── a robot: arriving is not delivering ─────
    let (gw, amr) = fleet(&t, site).await;
    let mover = async |lot: &str, qty: &str, from: &str, to: &str| {
        let body = json!({"warehouse": wh, "item": milk, "lot": lot, "from_bin": from, "to_bin": to, "qty": qty});
        new_move(&t, body).await
    };
    let job = mover("M-1", "5", "A-01", "P-01").await;
    let released = t.api_as(&manager, "POST", &format!("/tasks/{job}/release"), json!({"robot": amr})).await;
    released.expect("sent to the robot");
    let cmd: Uuid = t.get("WarehouseTask", job).await["command"].as_str().unwrap().parse().unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert_eq!(commands::take_for_site(&mut tx, tenant, "north", 1, 10, GATEWAY, false).await.unwrap().len(), 1, "the ledger holds the mission");
    tx.commit().await.unwrap();
    t.robot_reports(cmd, State::Accepted, None).await;
    t.robot_reports(cmd, State::Executing, None).await;
    let moving = t.get("WarehouseTask", job).await;
    assert_eq!((moving["status"].as_str(), moving["custody"].as_str()), (Some("Moving"), Some("In Transit")));
    assert_eq!(t.bin("A-01", "MILK").await.iter().map(|p| p.2).sum::<Decimal>(), n("5"), "still promised, so nothing else can take it");
    assert!(t.api_as(&operator, "POST", &format!("/tasks/{job}/confirm"), json!({"to_scan": "P-01"})).await.is_err(), "a robot has it");
    t.robot_reports(cmd, State::Completed, Some(json!({"lastNodeId": "P-01"}))).await;
    let arrived = t.get("WarehouseTask", job).await;
    assert_eq!(arrived["status"], json!("Arrived"));
    assert!(t.qty_in("P-01", "MILK").await.is_zero(), "the robot says it got there. The stock has not moved");
    assert!(
        t.api_as(&operator, "POST", &format!("/tasks/{job}/confirm"), json!({"to_scan": "A-02"})).await.is_err_and(|e| text(&e).contains("goes to")),
        "wrong station"
    );
    t.api_as(&operator, "POST", &format!("/tasks/{job}/confirm"), json!({"to_scan": "P-01", "item_scan": "MILK", "lot_scan": "M-1"}))
        .await
        .expect("confirmed at the station");
    assert_eq!(t.qty_in("P-01", "MILK").await, n("5"));
    t.reconciled().await;
    // The same event again, as an at-least-once outbox may send it, changes nothing.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let ctx = t.admin.clone();
    ferrum_wms::tasks::on_command(&k, &ctx, &mut tx, job, State::Completed, cmd, &Value::Null).await.expect("a repeat");
    tx.commit().await.unwrap();
    assert_eq!(t.get("WarehouseTask", job).await["status"], json!("Done"));

    // ───── a robot fails with the load on board ─────
    let lost = mover("M-1", "2", "P-01", "A-01").await;
    t.api_as(&manager, "POST", &format!("/tasks/{lost}/release"), json!({"robot": amr})).await.expect("sent");
    let c2: Uuid = t.get("WarehouseTask", lost).await["command"].as_str().unwrap().parse().unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    commands::take_for_site(&mut tx, tenant, "north", 1, 10, GATEWAY, false).await.unwrap();
    tx.commit().await.unwrap();
    t.robot_reports(c2, State::Executing, None).await;
    t.robot_reports(c2, State::OutcomeUnknown, None).await;
    let doubt = t.get("WarehouseTask", lost).await;
    assert_eq!((doubt["status"].as_str(), doubt["custody"].as_str()), (Some("Exception"), Some("Uncertain")));
    assert_eq!(t.bin("P-01", "MILK").await.iter().map(|p| p.2).sum::<Decimal>(), n("2"), "uncertain stock stays out of reach");
    assert!(
        t.api_as(&operator, "POST", &format!("/tasks/{lost}/recover"), json!({"found_bin": "A-02", "qty": 2, "note": "x"})).await.is_err(),
        "a manager says where it is"
    );
    assert!(t.api_as(&manager, "POST", &format!("/tasks/{lost}/recover"), json!({"found_bin": "A-02", "qty": 2, "note": " "})).await.is_err());
    t.api_as(&manager, "POST", &format!("/tasks/{lost}/recover"), json!({"found_bin": "A-02", "qty": 2, "note": "On the floor by A-02, tote intact"}))
        .await
        .expect("found");
    assert_eq!(t.qty_in("A-02", "MILK").await, n("2"), "where it was found is where it is");
    let settled = commands::describe(&k, &t.admin, c2).await.unwrap();
    assert_eq!(settled["state"], json!("Completed"), "and the ledger's unknown is settled by the same finding");
    t.reconciled().await;
    let _ = gw;

    // ───── a quality hold takes stock out of reach, everywhere, and its release gives it back ─────
    let quality = t.person("qa@dock.test", &["Quality Manager"]).await;
    let held_before = t.qty_in("P-01", "MILK").await;
    assert!(held_before > Decimal::ZERO);
    let hold = uid(&k.insert(&t.admin, "QualityHold", obj(json!({"item": milk, "lot": "M-1", "reason": "Cold chain broken"}))).await.expect("hold"));
    assert!(t.bin("P-01", "MILK").await.iter().filter(|p| p.0 == "Hold").map(|p| p.1).sum::<Decimal>() > Decimal::ZERO, "it changes status where it stands");
    let suspect = t.insert("SalesOrder", json!({"company": company, "customer": customer, "delivery_date": Utc::now().date_naive().to_string(), "set_warehouse": wh, "items": [{"item": milk, "qty": 1, "rate": 9}]})).await;
    k.submit(&t.admin, "SalesOrder", uid(&suspect), None).await.unwrap();
    let wave3 = uid(&t.insert("PickWave", json!({"company": company, "warehouse": wh, "orders": [{"sales_order": uid(&suspect)}]})).await);
    let blocked = k.run_action(&manager, "PickWave", wave3, "release", json!({})).await.expect("released");
    assert_eq!(dec(&blocked["shortfall"][0]["allocated"]), Decimal::ZERO, "nothing held can be promised: {blocked}");
    let site_ep = k.hooks.site("quality").unwrap().clone();
    let req = SiteRequest {
        tenant,
        user: Some(quality.clone()),
        method: "POST".into(),
        path: format!("/holds/{hold}/release"),
        query: Map::new(),
        headers: Map::new(),
        body: json!({"note": "Logger shows 2-6 C throughout"}).to_string().into_bytes(),
        client_ip: "::1".into(),
    };
    site_ep.handle(&k, req).await.expect("released");
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert_eq!(t.bin("P-01", "MILK").await.iter().filter(|p| p.0 == "Hold").count(), 0, "released, it is available again");
    assert_eq!(t.qty_in("P-01", "MILK").await, held_before, "all of it");
    t.reconciled().await;

    // ───── a count: frozen, blind, twice, and someone else approves ─────
    let spare = uid(&t.insert("StorageZone", json!({"warehouse": wh, "zone_code": "CNT", "kind": "Storage"})).await);
    bin("C-01", spare, json!({})).await;
    t.api_as(
        &manager,
        "POST",
        "/positions/move",
        json!({"warehouse": wh, "item": nut, "from_bin": "DOCK", "to_bin": "C-01", "qty": 30, "reason": "walked it"}),
    )
    .await
    .expect("placed");
    let count = uid(&t
        .insert("CycleCount", json!({"reference": "CC-1", "company": company, "warehouse": wh, "zone": spare, "blind": true, "reason": "Annual count"}))
        .await);
    let lines = t.api_as(&operator, "POST", &format!("/counts/{count}/start"), Value::Null).await.expect("started");
    assert_eq!(lines["lines"], json!(1));
    assert!(
        t.api_as(&manager, "POST", "/positions/move", json!({"warehouse": wh, "item": nut, "from_bin": "DOCK", "to_bin": "C-01", "qty": 1, "reason": "more"}))
            .await
            .is_err_and(|e| text(&e).contains("being counted")),
        "a frozen bin takes nothing"
    );
    let doc = t.get("CycleCount", count).await;
    let line = doc["lines"][0]["id"].as_str().unwrap().to_string();
    let told = t.api_as(&operator, "POST", &format!("/counts/{count}/enter"), json!({"line": line, "qty": 27})).await.unwrap();
    assert!(told.get("expected").is_none(), "a blind count tells the counter nothing");
    assert!(
        t.api_as(&operator, "POST", &format!("/counts/{count}/finish"), Value::Null).await.is_err_and(|e| text(&e).contains("again")),
        "a difference is counted twice"
    );
    t.api_as(&operator, "POST", &format!("/counts/{count}/enter"), json!({"line": line, "qty": 28, "recount": true})).await.unwrap();
    assert!(t.api_as(&operator, "POST", &format!("/counts/{count}/finish"), Value::Null).await.is_err_and(|e| text(&e).contains("disagree")));
    t.api_as(&operator, "POST", &format!("/counts/{count}/enter"), json!({"line": line, "qty": 27, "recount": true})).await.unwrap();
    t.api_as(&operator, "POST", &format!("/counts/{count}/finish"), Value::Null).await.expect("finished");
    assert!(t.api_as(&operator, "POST", &format!("/counts/{count}/approve"), Value::Null).await.is_err(), "not a manager");
    t.api_as(&manager, "POST", &format!("/counts/{count}/approve"), Value::Null).await.expect("approved");
    assert_eq!(t.qty_in("C-01", "NUT").await, n("27"));
    assert_eq!(t.on_books(nut).await, n("37"), "ERP followed the count");
    t.reconciled().await;
    assert!(
        t.api_as(&manager, "POST", "/positions/move", json!({"warehouse": wh, "item": nut, "from_bin": "DOCK", "to_bin": "C-01", "qty": 1, "reason": "after"}))
            .await
            .is_ok(),
        "and the bins are open again"
    );

    // ───── returns stay out of reach until someone decides ─────
    let rma = uid(&t
        .insert(
            "ReturnAuthorization",
            json!({"company": company, "warehouse": wh, "customer": customer, "items": [
        {"item": bolt, "qty": 3, "rate": 5, "reason": "Wrong size"}, {"item": bolt, "qty": 1, "rate": 5, "reason": "Rusted"}]}),
        )
        .await);
    t.api_as(&operator, "POST", &format!("/returns/{rma}/receive"), Value::Null).await.expect("received");
    assert_eq!(t.bin("JAIL", "BOLT").await.iter().filter(|p| p.0 == "Quarantine").map(|p| p.1).sum::<Decimal>(), n("4"));
    let books_before = t.on_books(bolt).await;
    assert_eq!(books_before, n("16"), "back on the books, off allocation");
    let doc = t.get("ReturnAuthorization", rma).await;
    let (l0, l1) = (doc["items"][0]["id"].as_str().unwrap().to_string(), doc["items"][1]["id"].as_str().unwrap().to_string());
    assert!(
        t.api_as(&operator, "POST", &format!("/returns/{rma}/dispose"), json!({"line": l0, "decision": "Restock"})).await.is_err(),
        "deciding is a manager's"
    );
    t.api_as(&manager, "POST", &format!("/returns/{rma}/dispose"), json!({"line": l0, "decision": "Restock", "reason": "Unopened"})).await.expect("restocked");
    t.api_as(&manager, "POST", &format!("/returns/{rma}/dispose"), json!({"line": l1, "decision": "Scrap", "reason": "Rusted through"}))
        .await
        .expect("scrapped");
    assert_eq!(t.on_books(bolt).await, n("15"), "scrap leaves through ERP");
    assert_eq!(t.get("ReturnAuthorization", rma).await["status"], json!("Dispositioned"));
    t.reconciled().await;

    // ───── a customer's stock is theirs: off the books, and billed ─────
    let goods = item("CRATE", json!({})).await;
    let stored = t
        .insert("InboundShipment", json!({"company": company, "warehouse": wh, "owner": tenant_customer, "items": [{"item": goods, "expected_qty": 20}]}))
        .await;
    t.api_as(&operator, "POST", &format!("/inbound/{}/scan", uid(&stored)), json!({"scan_id": Uuid::now_v7(), "code": "CRATE", "qty": 20})).await.unwrap();
    t.api_as(&manager, "POST", &format!("/inbound/{}/close", uid(&stored)), Value::Null).await.expect("closed");
    assert_eq!(t.qty_in("DOCK", "CRATE").await, n("20"));
    assert!(t.on_books(goods).await.is_zero(), "it is not ours");
    t.reconciled().await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let company_can_promise = ferrum_wms::waves::available(&mut tx, tenant, wh, goods).await.unwrap();
    tx.commit().await.unwrap();
    assert!(company_can_promise.is_zero(), "and nobody else's order can have it");
    let service = item("STORAGE", json!({"is_stock_item": false})).await;
    t.insert(
        "StorageContract",
        json!({"customer": tenant_customer, "company": company, "service_item": service, "rate_per_bin_day": 10, "rate_per_unit_moved": 1}),
    )
    .await;
    let today = Utc::now().date_naive();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    ferrum_wms::billing::snapshot(&mut tx, tenant, today).await.unwrap();
    ferrum_wms::billing::snapshot(&mut tx, tenant, today).await.unwrap();
    tx.commit().await.unwrap();
    let bill = t.api_as(&manager, "POST", "/billing", json!({"customer": tenant_customer, "from": today, "to": today})).await.expect("billed");
    assert_eq!(bill["bin_days"], json!(1), "counted once a day, however often it runs");
    assert_eq!(dec(&bill["handled"]), n("20"));
    assert!(bill["invoice"].is_string(), "{bill}");

    // ───── replenishment opens once ─────
    let mut restock_from_a2 = json!({"warehouse": wh, "item": milk, "pick_bin": p1, "min_qty": 5, "max_qty": 7, "active": true});
    restock_from_a2["active"] = json!(true);
    t.insert("ReplenishmentRule", restock_from_a2).await;
    assert!(t.qty_in("P-01", "MILK").await < n("5"));
    ferrum_wms::maintenance(&k).await.expect("upkeep");
    let before = t.tasks().await.iter().filter(|x| x["kind"] == json!("Replenish")).count();
    assert!(before >= 1, "below its minimum");
    ferrum_wms::maintenance(&k).await.expect("upkeep");
    assert_eq!(t.tasks().await.iter().filter(|x| x["kind"] == json!("Replenish")).count(), before, "not again while one is on its way");

    // ───── a second parcel carries only what the first did not ─────
    let bolts = t.on_books(bolt).await;
    t.api_as(&operator, "POST", &format!("/shipments/{ship}/verify"), json!({"code": "BOLT", "qty": 2})).await.expect("the rest verified");
    t.api_as(&operator, "POST", &format!("/shipments/{ship}/hand-over"), hand("T3", "T3")).await.expect("the rest handed over");
    assert_eq!(t.on_books(bolt).await, bolts - n("2"), "two left, not ninety");
    t.reconciled().await;

    // ───── the line: a released run brings its material, and production takes it from there ─────
    let part = item("PART", json!({})).await;
    let more = t
        .insert(
            "StockEntry",
            json!({"company": company, "purpose": "Material Receipt", "items": [{"item": part, "qty": 10, "basic_rate": 2, "target_warehouse": wh}]}),
        )
        .await;
    k.submit(&t.admin, "StockEntry", uid(&more), None).await.unwrap();
    t.api_as(
        &manager,
        "POST",
        "/positions/move",
        json!({"warehouse": wh, "item": part, "from_bin": "DOCK", "to_bin": "P-01", "qty": 10, "reason": "Walked the warehouse"}),
    )
    .await
    .expect("shelved");
    let line_side = bin("LINE", zone("LIN", "Production").await, json!({})).await;
    k.update(&t.admin, "ManagedWarehouse", managed, obj(json!({"production_bin": line_side})), None).await.expect("a line");
    let run = Uuid::now_v7();
    let materials = [json!({"item": part, "warehouse": wh, "qty": 12})];
    for _ in 0..2 {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        ferrum_wms::waves::supply(&k, &t.admin, &mut tx, run, &materials).await.expect("supplied");
        tx.commit().await.unwrap();
    }
    let moves: Vec<Value> = t.tasks().await.into_iter().filter(|x| x["item"] == json!("PART")).collect();
    assert_eq!(moves.len(), 1, "a run is supplied once, however often the event arrives");
    assert_eq!(moves[0]["to_bin"], json!("LINE"));
    assert_eq!(dec(&moves[0]["qty"]), n("10"), "as much as there is");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    // One task proves little here: the first delivery took all the stock. What it wanted is the proof.
    let wanted: Decimal =
        sqlx::query_scalar("select need from wms_supply where tenant_id = $1 and run_id = $2").bind(tenant).bind(run).fetch_one(&mut *tx).await.unwrap();
    assert_eq!(wanted, n("12"), "and an event that arrives twice is not wanted twice");
    let told: Option<String> =
        sqlx::query_scalar("select payload->>'short' from outbox where tenant_id = $1 and topic = 'wms.supply.short' and record_id = $2")
            .bind(tenant)
            .bind(run)
            .fetch_optional(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(told.as_deref(), Some("PART: 10 of 12"), "and what is missing goes back to the run");
    let issue = |qty: i64| json!({"company": company, "purpose": "Material Issue", "items": [{"item": part, "qty": qty, "source_warehouse": wh}]});
    let early = t.insert("StockEntry", issue(4)).await;
    let e = k.submit(&t.admin, "StockEntry", uid(&early), None).await.expect_err("it has not reached the line");
    assert!(text(&e).contains("brought to the line"), "{e:?}");
    let id = moves[0]["id"].as_str().unwrap();
    t.api_as(&manager, "POST", &format!("/tasks/{id}/release"), json!({})).await.expect("released");
    t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), json!({"from_scan": "P-01", "to_scan": "LINE", "item_scan": "PART"})).await.expect("brought");
    k.submit(&t.admin, "StockEntry", uid(&early), None).await.expect("taken from the line");
    assert_eq!(t.qty_in("LINE", "PART").await, n("6"));
    assert_eq!(t.on_books(part).await, n("6"));
    t.reconciled().await;

    // ───── what a run is told as things change at the line ─────
    let told = async |topic: &str, about: Uuid| -> i64 {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        let c = sqlx::query_scalar("select count(*) from outbox where tenant_id = $1 and topic = $2 and record_id = $3")
            .bind(tenant)
            .bind(topic)
            .bind(about)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        c
    };
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    ferrum_wms::waves::close_supply(&mut tx, tenant, run).await.unwrap();
    let next_run = Uuid::now_v7();
    ferrum_wms::waves::supply(&k, &t.admin, &mut tx, next_run, &[json!({"item": part, "warehouse": wh, "qty": 3})]).await.expect("supplied");
    tx.commit().await.unwrap();
    assert_eq!(told("wms.supply.ready", next_run).await, 1, "the last run let go of it, so what stands at the line is this one's");
    let quality_hold = uid(&t.insert("QualityHold", json!({"item": part, "reason": "Rust found on one"})).await);
    assert_eq!(told("wms.supply.short", next_run).await, 1, "a hold takes it, and the run hears at once");
    let release_hold = SiteRequest {
        tenant,
        user: Some(t.person("qa2@dock.test", &["Quality Manager"]).await),
        method: "POST".into(),
        path: format!("/holds/{quality_hold}/release"),
        query: Map::new(),
        headers: Map::new(),
        body: json!({"note": "It wiped off"}).to_string().into_bytes(),
        client_ip: "::1".into(),
    };
    k.hooks.site("quality").unwrap().clone().handle(&k, release_hold).await.expect("lifted");
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert_eq!(told("wms.supply.ready", next_run).await, 2, "and hears again when it is lifted");

    // Part of what a run needs sits where no line reaches: a warehouse cannot say all of it is there.
    let elsewhere = uid(&t.find("Warehouse", json!({"warehouse_name": "Finished Goods", "company": company.to_string()})).await);
    let mixed = Uuid::now_v7();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let materials = [json!({"item": part, "warehouse": wh, "qty": 1}), json!({"item": part, "warehouse": elsewhere, "qty": 1})];
    ferrum_wms::waves::supply(&k, &t.admin, &mut tx, mixed, &materials).await.expect("supplied");
    ferrum_wms::waves::supply_progress(&mut tx, &t.admin, mixed).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(told("wms.supply.ready", mixed).await, 0, "what it brings is there, and nothing here can see the rest");
    assert!(told("wms.supply.reserved", mixed).await >= 1, "so that part is a person's to confirm");

    // ───── a document taken back: the books and the bins go back together, or not at all ─────
    let mistake = t
        .insert(
            "StockEntry",
            json!({"company": company, "purpose": "Material Receipt", "items": [{"item": nut, "qty": 5, "basic_rate": 2, "target_warehouse": wh}]}),
        )
        .await;
    k.submit(&t.admin, "StockEntry", uid(&mistake), None).await.expect("received by mistake");
    let (books, at_dock) = (t.on_books(nut).await, t.qty_in("DOCK", "NUT").await);
    let shelved = t
        .api_as(
            &manager,
            "POST",
            "/positions/move",
            json!({"warehouse": wh, "item": nut, "from_bin": "DOCK", "to_bin": "C-01", "qty": at_dock, "reason": "put away"}),
        )
        .await;
    shelved.expect("moved on");
    let e = k.cancel(&t.admin, "StockEntry", uid(&mistake), None).await.expect_err("it has been put away: taking it back would leave the bins wrong");
    assert!(text(&e).contains("left at the inbound bin"), "and it says what to do: {e:?}");
    assert_eq!(t.on_books(nut).await, books, "and nothing was taken back");
    t.reconciled().await;
    t.api_as(
        &manager,
        "POST",
        "/positions/move",
        json!({"warehouse": wh, "item": nut, "from_bin": "C-01", "to_bin": "DOCK", "qty": at_dock, "reason": "walked it back"}),
    )
    .await
    .expect("moved back");
    k.cancel(&t.admin, "StockEntry", uid(&mistake), None).await.expect("still at the door, so it can go back");
    assert_eq!(t.on_books(nut).await, books - n("5"));
    assert_eq!(t.qty_in("DOCK", "NUT").await, at_dock - n("5"));
    t.reconciled().await;

    // An issue taken back: what left comes back in by the door, and the books follow.
    t.api_as(&manager, "POST", "/positions/move", json!({"warehouse": wh, "item": nut, "from_bin": "DOCK", "to_bin": "DOOR", "qty": 2, "reason": "picked"}))
        .await
        .expect("picked");
    let issued =
        t.insert("StockEntry", json!({"company": company, "purpose": "Material Issue", "items": [{"item": nut, "qty": 2, "source_warehouse": wh}]})).await;
    k.submit(&t.admin, "StockEntry", uid(&issued), None).await.expect("issued");
    let (books, at_dock) = (t.on_books(nut).await, t.qty_in("DOCK", "NUT").await);
    k.cancel(&t.admin, "StockEntry", uid(&issued), None).await.expect("nothing is left to issue, and this is not an issue");
    assert_eq!(t.on_books(nut).await, books + n("2"));
    assert_eq!(t.qty_in("DOCK", "NUT").await, at_dock + n("2"), "it comes back in by the door");
    t.reconciled().await;

    // ───── a lot can be followed, for a recall ─────
    let q = obj(json!({"item": milk, "lot": "M-1"}));
    let moves = t.call(&t.admin, "GET", "/trace", q, Value::Null).await.unwrap()["moves"].as_array().unwrap().len();
    assert!(moves >= 3, "{moves}");

    what_a_move_has_to_prove(&t, company, &operator, &manager, amr).await;

    // ───── another workspace sees none of it ─────
    let other = k
        .create_tenant(NewTenant {
            slug: "other".into(),
            name: "Other".into(),
            admin_email: "a@other.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["wms".into()],
            settings: None,
        })
        .await
        .unwrap();
    let mut tx = k.db.tenant_tx(other).await.unwrap();
    let seen: i64 = sqlx::query_scalar("select count(*) from wms_position").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(seen, 0);
}

/// What a release and a confirmation have to hold to, with stock of their own.
async fn what_a_move_has_to_prove(t: &T, company: Uuid, operator: &Ctx, manager: &Ctx, amr: Uuid) {
    let (k, wh, tenant) = (&t.k, t.wh, t.tenant);
    let group = t.find("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find("Uom", json!({"uom_name": "Nos"})).await;
    let gear = uid(&t.insert("Item", json!({"item_code": "GEAR", "item_name": "gear", "item_group": group["id"], "stock_uom": nos["id"], "is_stock_item": true, "has_batch_no": true, "barcode": "4006381333931"})).await);
    for lot in ["G-1", "G-2"] {
        t.insert("Batch", json!({"batch_id": lot, "item": gear})).await;
    }
    let bin = async |code: &str| {
        let zone = t.find("StorageZone", json!({"zone_code": "STO"})).await;
        uid(&t.insert("StorageBin", json!({"warehouse": wh, "zone": zone["id"], "bin_code": code})).await)
    };
    let (shelf, bench) = (bin("S-01").await, bin("S-02").await);
    let stock = t
        .insert(
            "StockEntry",
            json!({"company": company, "purpose": "Material Receipt", "items": [
        {"item": gear, "qty": 30, "basic_rate": 2, "target_warehouse": wh, "batch_no": "G-1"},
        {"item": gear, "qty": 30, "basic_rate": 2, "target_warehouse": wh, "batch_no": "G-2"}]}),
        )
        .await;
    k.submit(&t.admin, "StockEntry", uid(&stock), None).await.expect("stock");
    for lot in ["G-1", "G-2"] {
        t.api_as(
            manager,
            "POST",
            "/positions/move",
            json!({"warehouse": wh, "item": gear, "lot": lot, "from_bin": "DOCK", "to_bin": "S-01", "qty": 30, "reason": "shelved"}),
        )
        .await
        .expect("shelved");
    }
    let task =
        async |lot: &str, qty: &str| new_move(t, json!({"warehouse": wh, "item": gear, "lot": lot, "from_bin": "S-01", "to_bin": "S-02", "qty": qty})).await;
    let release = async |who: Option<Uuid>, id: Uuid| t.api_as(manager, "POST", &format!("/tasks/{id}/release"), json!({"robot": who})).await;
    let command = async |id: Uuid| -> Uuid { t.get("WarehouseTask", id).await["command"].as_str().unwrap().parse().unwrap() };
    let confirm = async |who: &Ctx, id: Uuid, body: Value| t.api_as(who, "POST", &format!("/tasks/{id}/confirm"), body).await;

    // ───── a release said twice is one mission ─────
    let job = task("G-1", "2").await;
    let first = release(Some(amr), job).await.expect("released");
    assert_eq!(first["repeated"], json!(false));
    let again = release(Some(amr), job).await.expect("the answer was lost, and the client asks again");
    assert_eq!((&again["command"], &again["repeated"]), (&first["command"], &json!(true)), "the same mission, and it says so");
    assert_eq!(t.missions(job).await, 1, "nothing new was written");
    assert!(release(None, job).await.is_err_and(|e| text(&e).contains("already on its way")), "nor can it quietly change hands");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert_eq!(commands::take_for_site(&mut tx, tenant, "north", 1, 50, GATEWAY, false).await.unwrap().len(), 1, "one mission reaches the site");
    tx.commit().await.unwrap();

    // Two at once: the task's lock puts them in a line.
    let racing = task("G-1", "1").await;
    let (a, b) = tokio::join!(release(Some(amr), racing), release(Some(amr), racing));
    let (a, b) = (a.expect("one"), b.expect("the other"));
    assert_eq!(a["command"], b["command"]);
    assert_eq!(t.missions(racing).await, 1, "two requests at once are one mission");

    // A mission that never started leaves the task open, and only then is there a new attempt. A
    // report about the old one, arriving late, says nothing about the new one.
    let old = command(racing).await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    commands::take_for_site(&mut tx, tenant, "north", 1, 50, GATEWAY, false).await.unwrap();
    tx.commit().await.unwrap();
    t.robot_reports(old, State::Rejected, None).await;
    assert_eq!(t.get("WarehouseTask", racing).await["status"], json!("Open"));
    let next = release(Some(amr), racing).await.expect("a second attempt");
    assert_ne!(next["command"], json!(old.to_string()));
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    ferrum_wms::tasks::on_command(k, &t.admin, &mut tx, racing, State::Accepted, old, &Value::Null).await.expect("late");
    tx.commit().await.unwrap();
    assert_eq!(t.get("WarehouseTask", racing).await["status"], json!("Released"), "the old mission's late word moves nothing");

    // ───── turned down after it was taken: it may have been moving ─────
    let taken = command(racing).await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    commands::take_for_site(&mut tx, tenant, "north", 1, 50, GATEWAY, false).await.unwrap();
    tx.commit().await.unwrap();
    t.robot_reports(taken, State::Accepted, None).await;
    t.robot_reports(taken, State::Rejected, None).await;
    let doubt = t.get("WarehouseTask", racing).await;
    assert_eq!((doubt["status"].as_str(), doubt["custody"].as_str()), (Some("Exception"), Some("Uncertain")), "not left on its way for ever");
    t.api_as(manager, "POST", &format!("/tasks/{racing}/recover"), json!({"found_bin": "S-01", "qty": 1, "note": "It never left the shelf"}))
        .await
        .expect("found");
    assert_eq!(t.get("WarehouseTask", racing).await["status"], json!("Cancelled"));

    // ───── arriving with the wrong thing proves nothing ─────
    let cmd = command(job).await;
    t.robot_reports(cmd, State::Accepted, None).await;
    t.robot_reports(cmd, State::Completed, Some(json!({"lastNodeId": "S-02"}))).await;
    let right = json!({"to_scan": "S-02", "item_scan": "GEAR", "lot_scan": "G-1"});
    let with = |key: &str, value: Value| {
        let mut body = right.clone();
        body[key] = value;
        body
    };
    let before = t.qty_in("S-02", "GEAR").await;
    let wrong_lot = confirm(operator, job, with("lot_scan", json!("G-2"))).await;
    assert!(wrong_lot.is_err_and(|e| text(&e).contains("this task moves G-1")), "the right item, the wrong lot");
    assert_eq!(t.audited("WarehouseTask", job, "wms.wrong_scan").await, 1, "the refused scan stays on record");
    assert!(confirm(operator, job, with("item_scan", json!("BOLT"))).await.is_err_and(|e| text(&e).contains("this task moves GEAR")), "the wrong item");
    assert!(
        confirm(operator, job, with("to_scan", json!("A-02"))).await.is_err_and(|e| text(&e).contains("this goes to")),
        "the right load in the wrong place"
    );
    assert_eq!(t.audited("WarehouseTask", job, "wms.wrong_scan").await, 3, "each one of them");
    let nothing = json!({"to_scan": "S-02"});
    assert!(confirm(operator, job, nothing).await.is_err_and(|e| text(&e).contains("scan GEAR")), "a location alone says nothing about the load");
    assert_eq!(t.qty_in("S-02", "GEAR").await, before, "and through all of that no stock moved");
    assert_eq!(t.get("WarehouseTask", job).await["status"], json!("Arrived"));
    assert!(
        confirm(operator, job, with("manual", json!("Label torn off"))).await.is_err_and(|e| matches!(e, Error::Forbidden(_))),
        "not an operator's to wave through"
    );
    let by_barcode = with("item_scan", json!("4006381333931"));
    let scan = Uuid::now_v7();
    let mut scanned = by_barcode.clone();
    scanned["scan_id"] = json!(scan);
    confirm(operator, job, scanned.clone()).await.expect("the right one, by its barcode");
    let done = t.get("WarehouseTask", job).await;
    assert_eq!((done["status"].as_str(), done["evidence_source"].as_str()), (Some("Done"), Some("Scan")));
    assert_eq!(done["evidence"]["load"]["lot"], json!("G-1"), "what proved it is kept: {done:?}");
    confirm(operator, job, scanned).await.expect("a lost answer repeats nothing");
    assert_eq!(t.qty_in("S-02", "GEAR").await, before + n("2"), "it moved once");

    // ───── a scan is used once: on another task it cannot finish anything ─────
    let other = task("G-1", "1").await;
    let mut reused = right.clone();
    reused["scan_id"] = json!(scan);
    assert!(confirm(operator, other, reused).await.is_err_and(|e| text(&e).contains("already used")), "that scan moved stock for another task");
    assert_eq!(t.get("WarehouseTask", other).await["status"], json!("Open"), "and this one is not finished by it");

    // ───── a manager can say so, and it never looks like a scan ─────
    let mut waved = right.clone();
    waved.as_object_mut().unwrap().retain(|k, _| k == "to_scan");
    waved["manual"] = json!("Label torn off. Counted by hand with Pia");
    confirm(manager, other, waved).await.expect("said by a manager");
    let said = t.get("WarehouseTask", other).await;
    assert_eq!((said["status"].as_str(), said["evidence_source"].as_str()), (Some("Done"), Some("Manual")));
    assert_eq!(t.audited("WarehouseTask", other, "wms.manual_confirm").await, 1);

    // ───── a unit is known by its label: the wrong tote at the right place proves nothing ─────
    let tote = uid(&t.insert("HandlingUnit", json!({"unit_code": "T-100", "warehouse": wh})).await);
    let twin = t.k.insert(&t.admin, "HandlingUnit", obj(json!({"unit_code": "T-100", "warehouse": wh}))).await;
    assert!(twin.is_err(), "two units cannot carry one label");
    t.insert("HandlingUnit", json!({"unit_code": "T-200", "warehouse": wh})).await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let loose = position::Key { bin: shelf, item: gear, owner: None, lot: "G-2".into(), serial: String::new(), unit: None, status: "Available".into() };
    let boxed = position::Key { unit: Some(tote), ..loose.clone() };
    position::transfer(&mut tx, &t.admin, wh, position::Kind::Move, &loose, &boxed, n("6"), Decimal::ZERO, &position::Cause::why("packed into a tote"))
        .await
        .unwrap();
    let tasked = ferrum_wms::tasks::create(
        k,
        &t.admin,
        &mut tx,
        ferrum_wms::tasks::NewTask {
            warehouse: wh,
            kind: "Move",
            item: gear,
            qty: n("6"),
            lot: "G-2".into(),
            unit: Some(tote),
            from_bin: shelf,
            to_bin: Some(bench),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    release(Some(amr), tasked).await.expect("sent");
    let c = command(tasked).await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    commands::take_for_site(&mut tx, tenant, "north", 1, 50, GATEWAY, false).await.unwrap();
    tx.commit().await.unwrap();
    t.robot_reports(c, State::Completed, Some(json!({"lastNodeId": "S-02"}))).await;
    assert!(
        confirm(operator, tasked, json!({"to_scan": "S-02", "unit_scan": "T-200"})).await.is_err_and(|e| text(&e).contains("this task moves T-100")),
        "the right place, the wrong tote"
    );
    assert_eq!(t.audited("WarehouseTask", tasked, "wms.wrong_scan").await, 1);

    // Only part of it came. What did not could be anywhere, so it stays out of reach.
    let partial = json!({"to_scan": "S-02", "unit_scan": "T-100", "qty": 4, "reason": "The tote is light"});
    confirm(operator, tasked, partial).await.expect("what came is confirmed");
    let light = t.get("WarehouseTask", tasked).await;
    assert_eq!((light["status"].as_str(), light["custody"].as_str()), (Some("Exception"), Some("Uncertain")), "a robot's short load is not a short pick");
    assert_eq!(dec(&light["done_qty"]), n("4"));
    let promised: Decimal = t.bin("S-01", "GEAR").await.iter().map(|p| p.2).sum();
    assert_eq!(promised, n("2"), "the rest is not free to promise until someone finds it");
    assert!(
        t.api_as(manager, "POST", &format!("/tasks/{tasked}/recover"), json!({"found_bin": "S-02", "qty": 3, "note": "x"}))
            .await
            .is_err_and(|e| text(&e).contains("between nothing and 2")),
        "what already arrived is not part of what was lost"
    );
    t.api_as(manager, "POST", &format!("/tasks/{tasked}/recover"), json!({"found_bin": "S-02", "qty": 1, "note": "One was on the robot's deck, one is gone"}))
        .await
        .expect("found");
    let settled = t.get("WarehouseTask", tasked).await;
    assert_eq!((settled["status"].as_str(), settled["evidence_source"].as_str()), (Some("Short"), Some("Recovery")));
    assert_eq!(dec(&settled["done_qty"]), n("5"));
    let left: Decimal = t.bin("S-01", "GEAR").await.iter().map(|p| p.2).sum();
    assert_eq!(left, Decimal::ZERO, "and nothing is promised any more");
    t.reconciled().await;

    // ───── a person's task through the site's station ─────
    let station = {
        let d = t.insert("Device", json!({"device_name": "Bench", "device_code": "STN-1", "kind": "Station", "site": t.find("Site", json!({"site_code": "north"})).await["id"], "operations": [{"operation": "warehouse.pick"}]})).await;
        let id = uid(&d);
        devices::set_lifecycle(k, &t.admin, id, Lifecycle::Claimed, "ours").await.unwrap();
        let (code, _) = ferrum_ops::identity::new_enrollment(k, &t.admin, id).await.unwrap();
        ferrum_ops::identity::enroll(k, tenant, &ferrum_ops::contract::sign::Enroll { code, public_key: hex(id) }).await.unwrap();
        devices::set_lifecycle(k, &t.admin, id, Lifecycle::Commissioned, "checked").await.unwrap();
        devices::set_lifecycle(k, &t.admin, id, Lifecycle::Active, "in service").await.unwrap();
        id
    };
    let managed = t.find("ManagedWarehouse", json!({"warehouse": wh.to_string()})).await;
    k.update(&t.admin, "ManagedWarehouse", uid(&managed), obj(json!({"person_station": station})), None).await.expect("a station");
    let at_bench = task("G-1", "1").await;
    release(None, at_bench).await.expect("to the station");
    let c = command(at_bench).await;
    let sent = commands::describe(k, &t.admin, c).await.unwrap();
    assert_eq!(sent["payload"]["load"], json!({"item": ["4006381333931", "GEAR"], "lot": "G-1"}), "the station is told what to scan: {sent}");
    assert!(confirm(operator, at_bench, right.clone()).await.is_err_and(|e| text(&e).contains("at the station")), "it is being done there");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    commands::take_for_site(&mut tx, tenant, "north", 1, 50, GATEWAY, false).await.unwrap();
    tx.commit().await.unwrap();
    let from_station = |load: Value| json!({"by": "Pia", "found": {"scan": "S-02", "load": load}});
    t.robot_reports(c, State::Accepted, None).await;
    t.robot_reports(c, State::Completed, Some(from_station(json!({"item": "BOLT", "lot": "G-1"})))).await;
    let stuck = t.get("WarehouseTask", at_bench).await;
    assert_eq!(stuck["status"], json!("Exception"), "a station's word that does not add up finishes nothing, and does not retry for ever");
    assert!(stuck["reason"].as_str().unwrap().contains("does not match"), "{stuck:?}");
    assert_eq!(t.audited("WarehouseTask", at_bench, "wms.wrong_scan").await, 1);
    t.api_as(manager, "POST", &format!("/tasks/{at_bench}/recover"), json!({"found_bin": "S-01", "qty": 0, "note": "Never left"})).await.expect("found");
    let good = task("G-1", "1").await;
    release(None, good).await.expect("to the station");
    let c = command(good).await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    commands::take_for_site(&mut tx, tenant, "north", 1, 50, GATEWAY, false).await.unwrap();
    tx.commit().await.unwrap();
    t.robot_reports(c, State::Accepted, None).await;
    t.robot_reports(c, State::Completed, Some(from_station(json!({"item": "4006381333931", "lot": "G-1"})))).await;
    let finished = t.get("WarehouseTask", good).await;
    assert_eq!((finished["status"].as_str(), finished["evidence_source"].as_str()), (Some("Done"), Some("Station")));
    k.update(&t.admin, "ManagedWarehouse", uid(&managed), obj(json!({"person_station": Value::Null})), None).await.expect("no station");
    t.reconciled().await;

    // ───── what a lost mission was bringing is still owed ─────
    let buyer = t.insert("Customer", json!({"customer_name": "Owed Ltd"})).await;
    let wave_for = async |qty: i64| {
        let order = t.insert("SalesOrder", json!({"company": company, "customer": buyer["id"], "delivery_date": Utc::now().date_naive().to_string(), "set_warehouse": wh, "items": [{"item": gear, "qty": qty, "rate": 9}]})).await;
        k.submit(&t.admin, "SalesOrder", uid(&order), None).await.unwrap();
        let wave = uid(&t.insert("PickWave", json!({"company": company, "warehouse": wh, "orders": [{"sales_order": uid(&order)}]})).await);
        k.run_action(manager, "PickWave", wave, "release", json!({})).await.expect("released");
        wave
    };
    let open_of = async |wave: Uuid| -> Vec<Uuid> {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        sqlx::query_scalar("select id from e_warehouse_task where tenant_id = $1 and wave = $2 and status = 'Open'")
            .bind(tenant)
            .bind(wave)
            .fetch_all(&mut *tx)
            .await
            .unwrap()
    };
    let shipment_of = async |wave: Uuid| -> Uuid {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        sqlx::query_scalar("select id from e_shipment where tenant_id = $1 and wave = $2").bind(tenant).bind(wave).fetch_one(&mut *tx).await.unwrap()
    };
    let lose = async |id: Uuid| {
        release(Some(amr), id).await.expect("sent");
        let c = command(id).await;
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        commands::take_for_site(&mut tx, tenant, "north", 1, 50, GATEWAY, false).await.unwrap();
        tx.commit().await.unwrap();
        t.robot_reports(c, State::Accepted, None).await;
        t.robot_reports(c, State::OutcomeUnknown, None).await;
    };
    let found = async |id: Uuid, bin: &str, qty: i64| {
        t.api_as(manager, "POST", &format!("/tasks/{id}/recover"), json!({"found_bin": bin, "qty": qty, "note": "Walked the aisle"})).await.expect("found");
    };
    let wave = wave_for(5).await;
    let lost = open_of(wave).await[0];
    let lot = t.get("WarehouseTask", lost).await["lot"].as_str().unwrap().to_string();
    let promised = async |bin: &str| t.bin(bin, "GEAR").await.iter().map(|p| p.2).sum::<Decimal>();
    let shelved = promised("S-01").await;
    lose(lost).await;
    found(lost, "S-01", 5).await;
    assert_eq!(t.get("WarehouseTask", lost).await["status"], json!("Cancelled"), "that mission is over");
    assert_eq!(t.get("PickWave", wave).await["status"], json!("Released"), "and the order's stock has not reached it");
    let on = open_of(wave).await;
    assert_eq!(on.len(), 1, "someone still has to bring it");
    let next = t.get("WarehouseTask", on[0]).await;
    assert_eq!((dec(&next["qty"]), next["lot"].as_str(), next["from_bin"].as_str()), (n("5"), Some(lot.as_str()), Some(shelf.to_string().as_str())));
    assert_eq!(promised("S-01").await, shelved, "promised all along: nobody took it meanwhile");
    assert!(t.get("PickWave", wave).await["shortfall"].as_array().is_none_or(|s| s.is_empty()), "nothing is missing yet");

    // Found on the way, and not all of it: the rest of the journey starts there, and what is gone is
    // written down.
    lose(on[0]).await;
    found(on[0], "S-02", 4).await;
    let on2 = open_of(wave).await;
    assert_eq!(on2.len(), 1);
    let next = t.get("WarehouseTask", on2[0]).await;
    assert_eq!((dec(&next["qty"]), next["from_bin"].as_str()), (n("4"), Some(bench.to_string().as_str())));
    assert_eq!(t.get("WarehouseTask", on[0]).await["status"], json!("Cancelled"), "it never got there, so it is not done");
    let short = t.get("PickWave", wave).await;
    assert_eq!((short["status"].as_str(), short["shortfall"].as_array().map(Vec::len)), (Some("Released"), Some(1)));
    assert_eq!(short["shortfall"][0]["item"], json!("GEAR"));
    assert_eq!(dec(&short["shortfall"][0]["allocated"]), n("4"));

    // Found where it was going: that is arrival, and the wave is over.
    lose(on2[0]).await;
    found(on2[0], "DOOR", 4).await;
    assert!(open_of(wave).await.is_empty(), "it is there");
    let arrived = t.get("WarehouseTask", on2[0]).await;
    assert_eq!((arrived["status"].as_str(), dec(&arrived["done_qty"])), (Some("Done"), n("4")));
    assert_eq!(t.get("PickWave", wave).await["status"], json!("Picked"));
    assert_eq!(t.get("Shipment", shipment_of(wave).await).await["status"], json!("Picking"), "what arrived can still go");

    // A pick given up on is written down too, and a shipment nothing reached is over.
    let gone = wave_for(3).await;
    let dropped = open_of(gone).await[0];
    t.api_as(manager, "POST", &format!("/tasks/{dropped}/cancel"), json!({"reason": "Customer called"})).await.expect("cancelled");
    let over = t.get("PickWave", gone).await;
    assert_eq!((over["status"].as_str(), over["shortfall"].as_array().map(Vec::len)), (Some("Picked"), Some(1)));
    assert_eq!(t.get("Shipment", shipment_of(gone).await).await["status"], json!("Cancelled"));
    t.reconciled().await;
}

/// A gateway holding `north`, and a robot it can reach.
async fn fleet(t: &T, site: Uuid) -> (Uuid, Uuid) {
    let device = async |code: &str, kind: &str, ops: Value| {
        let d = t.insert("Device", json!({"device_name": code, "device_code": code, "kind": kind, "site": site, "operations": ops})).await;
        let id = uid(&d);
        devices::set_lifecycle(&t.k, &t.admin, id, Lifecycle::Claimed, "ours").await.unwrap();
        let (code, _) = ferrum_ops::identity::new_enrollment(&t.k, &t.admin, id).await.unwrap();
        let key = hex(id);
        ferrum_ops::identity::enroll(&t.k, t.tenant, &ferrum_ops::contract::sign::Enroll { code, public_key: key }).await.unwrap();
        devices::set_lifecycle(&t.k, &t.admin, id, Lifecycle::Commissioned, "checked").await.unwrap();
        devices::set_lifecycle(&t.k, &t.admin, id, Lifecycle::Active, "in service").await.unwrap();
        id
    };
    let gw = device("GW", "Gateway", json!([])).await;
    let amr = device("AMR-1", "Robot", json!([{"operation": "warehouse.move"}])).await;
    (gw, amr)
}

fn hex(seed: Uuid) -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(seed.as_bytes());
    let key = ed25519_dalek::SigningKey::from_bytes(&bytes);
    key.verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

/// A move that a person opens by hand.
async fn new_move(t: &T, body: Value) -> Uuid {
    let code = |b: &Value| b.as_str().unwrap().to_string();
    let mut tx = t.k.db.tenant_tx(t.tenant).await.unwrap();
    let from = ferrum_wms::slotting::bin_by_code(&mut tx, t.tenant, t.wh, &code(&body["from_bin"])).await.unwrap();
    let to = ferrum_wms::slotting::bin_by_code(&mut tx, t.tenant, t.wh, &code(&body["to_bin"])).await.unwrap();
    let id = ferrum_wms::tasks::create(
        &t.k,
        &t.admin,
        &mut tx,
        ferrum_wms::tasks::NewTask {
            warehouse: t.wh,
            kind: "Move",
            item: body["item"].as_str().unwrap().parse().unwrap(),
            qty: dec(&body["qty"]),
            lot: code(&body["lot"]),
            from_bin: from.id,
            to_bin: Some(to.id),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id
}
