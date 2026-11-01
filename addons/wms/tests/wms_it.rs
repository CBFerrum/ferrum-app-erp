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
use rust_decimal::Decimal;
use serde_json::{Map, Value, json};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

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
        let req = SiteRequest { tenant: self.tenant, user: Some(ctx.clone()), method: method.into(), path: path.into(), query, headers: Map::new(), body: body.to_string().into_bytes(), client_ip: "::1".into() };
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
        v["positions"].as_array().unwrap().iter().filter(|p| p["item_code"] == json!(item)).map(|p| (p["status"].as_str().unwrap().to_string(), dec(&p["qty"]), dec(&p["allocated"]))).collect()
    }

    async fn qty_in(&self, code: &str, item: &str) -> Decimal {
        self.bin(code, item).await.iter().map(|p| p.1).sum()
    }

    async fn on_books(&self, item: Uuid) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let q: Option<Decimal> = sqlx::query_scalar("select qty from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = $3").bind(self.tenant).bind(item).bind(self.wh).fetch_optional(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        q.unwrap_or_default()
    }

    async fn reconciled(&self) {
        let q = obj(json!({"warehouse": self.wh}));
        let v = self.call(&self.admin, "GET", "/reconcile", q, Value::Null).await.unwrap();
        assert_eq!(v, json!({"books": [], "ledger": []}), "the warehouse, the ledger and the books agree");
    }

    async fn person(&self, email: &str, roles: &[&str]) -> Ctx {
        let id = self.k.create_user(&self.admin, email, "Someone", Some("Correct-Horse-9"), &roles.iter().map(|r| r.to_string()).collect::<Vec<_>>(), false).await.expect("user");
        self.k.load_ctx(self.tenant, id, None, "it".into()).await.unwrap()
    }

    /// What a site does for a robot: says what it established, in the ledger, and lets it travel.
    async fn robot_reports(&self, command: Uuid, state: State, evidence: Option<Value>) {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let r = Report { report_id: Uuid::now_v7(), command_id: command, state, at_source: Utc::now(), epoch: 1, detail: Value::Null, evidence };
        commands::apply(&mut tx, self.tenant, &r, 1).await.unwrap();
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
    let catalog = Catalog::from_packages(packages).expect("catalog");
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

    let opening = t.insert("StockEntry", json!({"company": company, "purpose": "Material Receipt", "items": [{"item": nut, "qty": 40, "basic_rate": 2, "target_warehouse": wh}]})).await;
    k.submit(&t.admin, "StockEntry", uid(&opening), None).await.expect("opening stock");

    let zone = async |code: &str, kind: &str| uid(&t.insert("StorageZone", json!({"warehouse": wh, "zone_code": code, "kind": kind})).await);
    let (rcv, sto, pck, shp, qua) = (zone("RCV", "Receiving").await, zone("STO", "Storage").await, zone("PCK", "Picking").await, zone("SHP", "Shipping").await, zone("QUA", "Quarantine").await);
    let bin = async |code: &str, zone: Uuid, extra: Value| {
        let mut v = json!({"warehouse": wh, "zone": zone, "bin_code": code});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        uid(&t.insert("StorageBin", v).await)
    };
    let (dock, door, jail) = (bin("DOCK", rcv, json!({"kind": "Dock"})).await, bin("DOOR", shp, json!({"kind": "Dock"})).await, bin("JAIL", qua, json!({})).await);
    let a1 = bin("A-01", sto, json!({"pick_sequence": 1})).await;
    let a2 = bin("A-02", sto, json!({"pick_sequence": 2, "max_units": 10})).await;
    let p1 = bin("P-01", pck, json!({"pick_sequence": 3})).await;
    let site = uid(&t.insert("Site", json!({"site_name": "North", "site_code": "north"})).await);
    let managed = uid(&t.insert("ManagedWarehouse", json!({"warehouse": wh, "site": site, "inbound_bin": dock, "outbound_bin": door, "quarantine_bin": jail, "allocation_rule": "FEFO"})).await);
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
    sqlx::query("update stock_balance set qty = qty + 1 where tenant_id = $1 and item_id = $2 and warehouse_id = $3").bind(tenant).bind(nut).bind(wh).execute(&mut *tx).await.unwrap();
    assert!(tx.commit().await.is_err(), "nor can the books");
    // The ledger is append-only.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert!(sqlx::query("delete from wms_move where tenant_id = $1").bind(tenant).execute(&mut *tx).await.is_err());
    drop(tx);

    // ───── ERP cannot issue what nobody picked ─────
    let refused = t.insert("StockEntry", json!({"company": company, "purpose": "Material Issue", "items": [{"item": nut, "qty": 5, "source_warehouse": wh}]})).await;
    let e = k.submit(&t.admin, "StockEntry", uid(&refused), None).await.expect_err("nothing is at the outbound bin");
    assert!(text(&e).contains("picked to the outbound bin"), "{e:?}");
    assert_eq!(t.on_books(nut).await, n("40"), "and the whole document rolled back");

    // ───── receiving ─────
    let inbound = t.insert("InboundShipment", json!({"company": company, "warehouse": wh, "supplier": supplier, "reference": "DN-77", "items": [
        {"item": bolt, "expected_qty": 100, "rate": 5},
        {"item": milk, "expected_qty": 10, "lot": "M-1", "expiry": (Utc::now() + Duration::days(40)).date_naive().to_string(), "rate": 3},
        {"item": milk, "expected_qty": 10, "lot": "M-2", "expiry": (Utc::now() + Duration::days(10)).date_naive().to_string(), "rate": 3}]})).await;
    let asn = uid(&inbound);
    let lines = inbound["items"].as_array().unwrap().clone();
    let line = |i: usize| lines[i]["id"].as_str().unwrap().to_string();
    let scan = Uuid::now_v7();
    let path = format!("/inbound/{asn}/scan");
    let body = json!({"scan_id": scan, "line": line(0), "qty": 96});
    assert_eq!(dec(&t.api_as(&operator, "POST", &path, body.clone()).await.unwrap()["received"]), n("96"));
    assert_eq!(t.api_as(&operator, "POST", &path, body).await.unwrap()["repeated"], json!(true), "a scanner repeating itself counts once");
    let over = t.api_as(&operator, "POST", &path, json!({"scan_id": Uuid::now_v7(), "line": line(0), "qty": 10})).await;
    assert!(matches!(over, Err(Error::Forbidden(ref m)) if m.contains("manager")), "{over:?}");
    t.api_as(&operator, "POST", &path, json!({"scan_id": Uuid::now_v7(), "line": line(0), "damaged": 4})).await.expect("damaged");
    assert!(t.api_as(&viewer, "POST", &path, json!({"scan_id": Uuid::now_v7(), "line": line(1), "qty": 1})).await.is_err(), "a viewer does not receive");
    for i in [1, 2] {
        t.api_as(&operator, "POST", &path, json!({"scan_id": Uuid::now_v7(), "line": line(i), "qty": 10})).await.expect("milk");
    }
    t.api_as(&operator, "POST", &format!("/inbound/{asn}/unknown"), json!({"description": "A pallet with no label", "qty": 1})).await.expect("noted");
    assert!(matches!(t.api_as(&operator, "POST", &format!("/inbound/{asn}/close"), Value::Null).await, Err(Error::Forbidden(_))));
    let closed = t.api_as(&manager, "POST", &format!("/inbound/{asn}/close"), Value::Null).await.expect("closed");
    assert_eq!(closed["status"], json!("Discrepancy"), "the unknown pallet is still somebody's to sort out");
    assert_eq!(t.on_books(bolt).await, n("100"), "ERP received it, damaged included: it is here");
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
    assert!(t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), json!({"from_scan": "DOCK", "to_scan": "P-01"})).await.is_ok(), "an operator may choose another that fits");
    k.update(&t.admin, "StorageBin", a1, obj(json!({"status": "Open"})), None).await.unwrap();
    assert_eq!(t.qty_in("P-01", "BOLT").await, n("96"));
    assert!(t.qty_in("DOCK", "BOLT").await.is_zero());
    t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), json!({"to_scan": "P-01", "scan_id": Uuid::now_v7()})).await.expect("a repeat of a finished task with a scan id does nothing");
    t.reconciled().await;
    for (lot, expect) in [("M-1", "A-01"), ("M-2", "A-01")] {
        let task = t.tasks().await.into_iter().find(|x| x["kind"] == json!("Putaway") && x["lot"] == json!(lot)).unwrap();
        assert_eq!(task["to_bin"], json!(expect));
        t.api_as(&operator, "POST", &format!("/tasks/{}/confirm", task["id"].as_str().unwrap()), json!({"to_scan": expect})).await.expect("put away");
    }

    // Hazard rules are not hints.
    let hazard = t.api_as(&manager, "POST", "/positions/move", json!({"warehouse": wh, "item": acid, "from_bin": "DOCK", "to_bin": "A-01", "qty": 1, "reason": "x"})).await;
    assert!(hazard.is_err_and(|e| text(&e).contains("hazard")));

    // ───── a wave, by first to expire ─────
    let order = t.insert("SalesOrder", json!({"company": company, "customer": customer, "delivery_date": Utc::now().date_naive().to_string(), "set_warehouse": wh, "items": [
        {"item": milk, "qty": 15, "rate": 9}, {"item": bolt, "qty": 90, "rate": 9}]})).await;
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
    let robbed = t.api_as(&operator, "POST", "/positions/move", json!({"warehouse": wh, "item": bolt, "from_bin": "P-01", "to_bin": "A-02", "qty": 3, "reason": "tidying"})).await;
    assert!(robbed.is_err_and(|e| text(&e).contains("promised")));
    let no_reason = t.api_as(&operator, "POST", "/positions/move", json!({"warehouse": wh, "item": nut, "from_bin": "DOCK", "to_bin": "A-02", "qty": 3, "reason": " "})).await;
    assert!(no_reason.is_err(), "a move with no task says why");

    // ───── picking, and a short pick ─────
    let picks: Vec<Value> = t.tasks().await.into_iter().filter(|x| x["kind"] == json!("Pick")).collect();
    for p in &picks {
        let id = p["id"].as_str().unwrap();
        let is_short = p["item"] == json!("BOLT") && dec(&p["qty"]) == n("90");
        let wrong_door = t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), json!({"to_scan": "A-02"})).await;
        assert!(wrong_door.is_err(), "picked stock goes where packing expects it");
        let body = if is_short { json!({"to_scan": "DOOR", "qty": 88}) } else { json!({"to_scan": "DOOR"}) };
        let done = t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), body.clone()).await;
        if is_short {
            assert!(done.is_err(), "less than asked says why");
            t.api_as(&operator, "POST", &format!("/tasks/{id}/confirm"), json!({"to_scan": "DOOR", "qty": 88, "reason": "Two were bent"})).await.expect("short");
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
    let wrong_item = t.api_as(&operator, "POST", &format!("/shipments/{ship}/verify"), json!({"item_code": "NUT", "qty": 1})).await;
    assert!(wrong_item.is_err(), "not on this shipment");
    let hand = |tracking: &str, label: &str| json!({"carrier": "DHL", "tracking": tracking, "label": label});
    assert!(t.api_as(&operator, "POST", &format!("/shipments/{ship}/hand-over"), hand("T1", "T1")).await.is_err(), "nothing verified, nothing leaves");
    t.api_as(&operator, "POST", &format!("/shipments/{ship}/verify"), json!({"item_code": "BOLT", "qty": 88})).await.expect("verified");
    assert!(t.api_as(&operator, "POST", &format!("/shipments/{ship}/verify"), json!({"item_code": "BOLT", "qty": 5})).await.is_err(), "more than was picked for it");
    assert!(t.api_as(&operator, "POST", &format!("/shipments/{ship}/hand-over"), hand("T1", "T2")).await.is_err_and(|e| text(&e).contains("label")));
    let went = t.api_as(&operator, "POST", &format!("/shipments/{ship}/hand-over"), hand("T1", "T1")).await.expect("handed over");
    assert_eq!(went["whole"], json!(false), "a partial shipment says it is one");
    let dn = t.get("DeliveryNote", went["delivery_note"].as_str().unwrap().parse().unwrap()).await;
    assert_eq!(dn["docstatus"], json!(1));
    assert_eq!(t.on_books(bolt).await, n("12"), "the books follow what left");
    assert_eq!(t.qty_in("DOOR", "BOLT").await, n("6"), "and only that left");
    assert_eq!(t.get("Shipment", ship).await["status"], json!("Partially Handed Over"));
    t.reconciled().await;

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
    assert_eq!(commands::take_for_site(&mut tx, tenant, "north", 1, 10).await.unwrap().len(), 1, "the ledger holds the mission");
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
    assert!(t.api_as(&operator, "POST", &format!("/tasks/{job}/confirm"), json!({"to_scan": "A-02"})).await.is_err_and(|e| text(&e).contains("goes to")), "wrong station");
    t.api_as(&operator, "POST", &format!("/tasks/{job}/confirm"), json!({"to_scan": "P-01"})).await.expect("confirmed at the station");
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
    commands::take_for_site(&mut tx, tenant, "north", 1, 10).await.unwrap();
    tx.commit().await.unwrap();
    t.robot_reports(c2, State::Executing, None).await;
    t.robot_reports(c2, State::OutcomeUnknown, None).await;
    let doubt = t.get("WarehouseTask", lost).await;
    assert_eq!((doubt["status"].as_str(), doubt["custody"].as_str()), (Some("Exception"), Some("Uncertain")));
    assert_eq!(t.bin("P-01", "MILK").await.iter().map(|p| p.2).sum::<Decimal>(), n("2"), "uncertain stock stays out of reach");
    assert!(t.api_as(&operator, "POST", &format!("/tasks/{lost}/recover"), json!({"found_bin": "A-02", "qty": 2, "note": "x"})).await.is_err(), "a manager says where it is");
    assert!(t.api_as(&manager, "POST", &format!("/tasks/{lost}/recover"), json!({"found_bin": "A-02", "qty": 2, "note": " "})).await.is_err());
    t.api_as(&manager, "POST", &format!("/tasks/{lost}/recover"), json!({"found_bin": "A-02", "qty": 2, "note": "On the floor by A-02, tote intact"})).await.expect("found");
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
    let req = SiteRequest { tenant, user: Some(quality.clone()), method: "POST".into(), path: format!("/holds/{hold}/release"), query: Map::new(), headers: Map::new(), body: json!({"note": "Logger shows 2-6 C throughout"}).to_string().into_bytes(), client_ip: "::1".into() };
    site_ep.handle(&k, req).await.expect("released");
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert_eq!(t.bin("P-01", "MILK").await.iter().filter(|p| p.0 == "Hold").count(), 0, "released, it is available again");
    assert_eq!(t.qty_in("P-01", "MILK").await, held_before, "all of it");
    t.reconciled().await;

    // ───── a count: frozen, blind, twice, and someone else approves ─────
    let spare = uid(&t.insert("StorageZone", json!({"warehouse": wh, "zone_code": "CNT", "kind": "Storage"})).await);
    bin("C-01", spare, json!({})).await;
    t.api_as(&manager, "POST", "/positions/move", json!({"warehouse": wh, "item": nut, "from_bin": "DOCK", "to_bin": "C-01", "qty": 30, "reason": "walked it"})).await.expect("placed");
    let count = uid(&t.insert("CycleCount", json!({"reference": "CC-1", "company": company, "warehouse": wh, "zone": spare, "blind": true, "reason": "Annual count"})).await);
    let lines = t.api_as(&operator, "POST", &format!("/counts/{count}/start"), Value::Null).await.expect("started");
    assert_eq!(lines["lines"], json!(1));
    assert!(t.api_as(&manager, "POST", "/positions/move", json!({"warehouse": wh, "item": nut, "from_bin": "DOCK", "to_bin": "C-01", "qty": 1, "reason": "more"})).await.is_err_and(|e| text(&e).contains("being counted")), "a frozen bin takes nothing");
    let doc = t.get("CycleCount", count).await;
    let line = doc["lines"][0]["id"].as_str().unwrap().to_string();
    let told = t.api_as(&operator, "POST", &format!("/counts/{count}/enter"), json!({"line": line, "qty": 27})).await.unwrap();
    assert!(told.get("expected").is_none(), "a blind count tells the counter nothing");
    assert!(t.api_as(&operator, "POST", &format!("/counts/{count}/finish"), Value::Null).await.is_err_and(|e| text(&e).contains("again")), "a difference is counted twice");
    t.api_as(&operator, "POST", &format!("/counts/{count}/enter"), json!({"line": line, "qty": 28, "recount": true})).await.unwrap();
    assert!(t.api_as(&operator, "POST", &format!("/counts/{count}/finish"), Value::Null).await.is_err_and(|e| text(&e).contains("disagree")));
    t.api_as(&operator, "POST", &format!("/counts/{count}/enter"), json!({"line": line, "qty": 27, "recount": true})).await.unwrap();
    t.api_as(&operator, "POST", &format!("/counts/{count}/finish"), Value::Null).await.expect("finished");
    assert!(t.api_as(&operator, "POST", &format!("/counts/{count}/approve"), Value::Null).await.is_err(), "not a manager");
    t.api_as(&manager, "POST", &format!("/counts/{count}/approve"), Value::Null).await.expect("approved");
    assert_eq!(t.qty_in("C-01", "NUT").await, n("27"));
    assert_eq!(t.on_books(nut).await, n("37"), "ERP followed the count");
    t.reconciled().await;
    assert!(t.api_as(&manager, "POST", "/positions/move", json!({"warehouse": wh, "item": nut, "from_bin": "DOCK", "to_bin": "C-01", "qty": 1, "reason": "after"})).await.is_ok(), "and the bins are open again");

    // ───── returns stay out of reach until someone decides ─────
    let rma = uid(&t.insert("ReturnAuthorization", json!({"company": company, "warehouse": wh, "customer": customer, "items": [
        {"item": bolt, "qty": 3, "rate": 5, "reason": "Wrong size"}, {"item": bolt, "qty": 1, "rate": 5, "reason": "Rusted"}]})).await);
    t.api_as(&operator, "POST", &format!("/returns/{rma}/receive"), Value::Null).await.expect("received");
    assert_eq!(t.bin("JAIL", "BOLT").await.iter().filter(|p| p.0 == "Quarantine").map(|p| p.1).sum::<Decimal>(), n("4"));
    let books_before = t.on_books(bolt).await;
    assert_eq!(books_before, n("16"), "back on the books, off allocation");
    let doc = t.get("ReturnAuthorization", rma).await;
    let (l0, l1) = (doc["items"][0]["id"].as_str().unwrap().to_string(), doc["items"][1]["id"].as_str().unwrap().to_string());
    assert!(t.api_as(&operator, "POST", &format!("/returns/{rma}/dispose"), json!({"line": l0, "decision": "Restock"})).await.is_err(), "deciding is a manager's");
    t.api_as(&manager, "POST", &format!("/returns/{rma}/dispose"), json!({"line": l0, "decision": "Restock", "reason": "Unopened"})).await.expect("restocked");
    t.api_as(&manager, "POST", &format!("/returns/{rma}/dispose"), json!({"line": l1, "decision": "Scrap", "reason": "Rusted through"})).await.expect("scrapped");
    assert_eq!(t.on_books(bolt).await, n("15"), "scrap leaves through ERP");
    assert_eq!(t.get("ReturnAuthorization", rma).await["status"], json!("Dispositioned"));
    t.reconciled().await;

    // ───── a customer's stock is theirs: off the books, and billed ─────
    let goods = item("CRATE", json!({})).await;
    let stored = t.insert("InboundShipment", json!({"company": company, "warehouse": wh, "owner": tenant_customer, "items": [{"item": goods, "expected_qty": 20}]})).await;
    t.api_as(&operator, "POST", &format!("/inbound/{}/scan", uid(&stored)), json!({"scan_id": Uuid::now_v7(), "line": stored["items"][0]["id"], "qty": 20})).await.unwrap();
    t.api_as(&manager, "POST", &format!("/inbound/{}/close", uid(&stored)), Value::Null).await.expect("closed");
    assert_eq!(t.qty_in("DOCK", "CRATE").await, n("20"));
    assert!(t.on_books(goods).await.is_zero(), "it is not ours");
    t.reconciled().await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let company_can_promise = ferrum_wms::waves::available(&mut tx, tenant, wh, goods).await.unwrap();
    tx.commit().await.unwrap();
    assert!(company_can_promise.is_zero(), "and nobody else's order can have it");
    let service = item("STORAGE", json!({"is_stock_item": false})).await;
    t.insert("StorageContract", json!({"customer": tenant_customer, "company": company, "service_item": service, "rate_per_bin_day": 10, "rate_per_unit_moved": 1})).await;
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

    // ───── a lot can be followed, for a recall ─────
    let q = obj(json!({"item": milk, "lot": "M-1"}));
    let moves = t.call(&t.admin, "GET", "/trace", q, Value::Null).await.unwrap()["moves"].as_array().unwrap().len();
    assert!(moves >= 3, "{moves}");

    // ───── another workspace sees none of it ─────
    let other = k
        .create_tenant(NewTenant { slug: "other".into(), name: "Other".into(), admin_email: "a@other.test".into(), admin_password: "Correct-Horse-9".into(), admin_name: None, packages: vec!["wms".into()], settings: None })
        .await
        .unwrap();
    let mut tx = k.db.tenant_tx(other).await.unwrap();
    let seen: i64 = sqlx::query_scalar("select count(*) from wms_position").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(seen, 0);
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
