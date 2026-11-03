//! A production run and a warehouse together, against a real PostgreSQL
//! (`FERRUM_LINE_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_line_test`;
//! reset first): the plan, the material brought to the line by tasks, the machine's start, quality,
//! and what posts to ERP. What matters at each step is that the intent, the stock and the books agree,
//! or that the chain stops at something a person can see.

use chrono::Utc;
use ferrum_kernel::query::parse_filters;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, SiteRequest};
use ferrum_meta::Catalog;
use ferrum_ops::contract::command::Report;
use ferrum_ops::contract::{Lifecycle, State};
use ferrum_ops::{commands, devices};
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
        self.k.list(&self.admin, entity, &q).await.unwrap().rows.into_iter().next().unwrap_or_else(|| panic!("no {entity} {filters}"))
    }

    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.admin, entity, id, false).await.unwrap()
    }

    async fn call(&self, site: &str, ctx: &Ctx, method: &str, path: &str, query: Value, body: Value) -> Result<Value, Error> {
        let handler = self.k.hooks.site(site).unwrap().clone();
        let req = SiteRequest {
            tenant: self.tenant,
            user: Some(ctx.clone()),
            method: method.into(),
            path: path.into(),
            query: query.as_object().cloned().unwrap_or_default(),
            headers: Map::new(),
            body: body.to_string().into_bytes(),
            client_ip: "::1".into(),
        };
        handler.handle(&self.k, req).await.map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
    }

    async fn wms(&self, ctx: &Ctx, path: &str, body: Value) -> Result<Value, Error> {
        self.call("wms", ctx, "POST", path, Value::Null, body).await
    }

    async fn mes(&self, ctx: &Ctx, path: &str, body: Value) -> Result<Value, Error> {
        self.call("mes", ctx, "POST", path, Value::Null, body).await
    }

    async fn run(&self, id: Uuid) -> Doc {
        self.get("ProductionRun", id).await
    }

    /// Lets what was committed reach whoever is waiting for it.
    async fn deliver(&self) {
        while self.k.dispatch_outbox_once().await.unwrap() > 0 {}
    }

    /// The tasks bringing a run's material.
    async fn line_tasks(&self, run: Uuid) -> Vec<Value> {
        let all = self.call("wms", &self.admin, "GET", "/tasks", json!({"warehouse": self.wh}), Value::Null).await.unwrap();
        let wanted = run.to_string();
        let mut out = Vec::new();
        for t in all["tasks"].as_array().unwrap() {
            let id: Uuid = t["id"].as_str().unwrap().parse().unwrap();
            if self.get("WarehouseTask", id).await["production_run"] == json!(wanted) {
                out.push(t.clone());
            }
        }
        out
    }

    async fn deliver_all(&self, ctx: &Ctx, run: Uuid) {
        for t in self.line_tasks(run).await {
            let (id, from) = (t["id"].as_str().unwrap().to_string(), t["from_bin"].as_str().unwrap().to_string());
            self.wms(ctx, &format!("/tasks/{id}/release"), json!({})).await.expect("released");
            let body = json!({"from_scan": from, "to_scan": "LINE", "item_scan": t["item"], "lot_scan": t["lot"]});
            self.wms(ctx, &format!("/tasks/{id}/confirm"), body).await.unwrap_or_else(|e| panic!("{t}: {e}"));
        }
    }

    async fn reconciled(&self) {
        let v = self.call("wms", &self.admin, "GET", "/reconcile", json!({"warehouse": self.wh}), Value::Null).await.unwrap();
        assert_eq!(v, json!({"books": [], "ledger": []}), "the warehouse, the ledger and the books agree");
    }

    async fn at(&self, code: &str, item: &str) -> Decimal {
        let v = self.call("wms", &self.admin, "GET", &format!("/bins/{code}"), json!({"warehouse": self.wh}), Value::Null).await.unwrap();
        v["positions"].as_array().unwrap().iter().filter(|p| p["item_code"] == json!(item) && p["status"] == json!("Available")).map(|p| dec(&p["qty"])).sum()
    }

    async fn commands_of(&self, run: Uuid) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let c = sqlx::query_scalar("select count(*) from op_command where tenant_id = $1 and workflow_id = $2")
            .bind(self.tenant)
            .bind(format!("mes-run:{run}"))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        c
    }

    async fn person(&self, email: &str, roles: &[&str]) -> Ctx {
        let id = self
            .k
            .create_user(&self.admin, email, "Someone", Some("Correct-Horse-9"), &roles.iter().map(|r| r.to_string()).collect::<Vec<_>>(), false)
            .await
            .expect("user");
        self.k.load_ctx(self.tenant, id, None, "it".into()).await.unwrap()
    }
}

fn text(e: &Error) -> String {
    match e {
        Error::Validation(v) => v.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => other.to_string(),
    }
}

fn hex_key(seed: Uuid) -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(seed.as_bytes());
    ed25519_dalek::SigningKey::from_bytes(&bytes).verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn from_the_plan_to_the_books() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(32 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_LINE_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_line_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().unwrap();
    packages.extend(ferrum_app_erp::packages().unwrap());
    packages.extend(ferrum_stock_ops::packages().unwrap());
    packages.extend(ferrum_mfg_ops::packages().unwrap());
    packages.extend(ferrum_quality::packages().unwrap());
    packages.extend(ferrum_ops::packages().unwrap());
    packages.extend(ferrum_wms::packages().unwrap());
    packages.extend(ferrum_mes::packages().unwrap());
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_stock_ops::register(&mut hooks);
    ferrum_mfg_ops::register(&mut hooks);
    ferrum_quality::register(&mut hooks);
    ferrum_ops::register(&mut hooks);
    ferrum_wms::register(&mut hooks);
    ferrum_mes::register(&mut hooks);
    let k = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.unwrap();
    ferrum_app_erp::maintenance(&k).await.unwrap();
    let tenant = k
        .create_tenant(NewTenant {
            slug: "line".into(),
            name: "Line Co".into(),
            admin_email: "admin@line.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["wms".into(), "mes".into()],
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("tenant");
    let session = k.login("line", "admin@line.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let mut t = T { k: k.clone(), tenant, admin, wh: Uuid::nil() };

    // ───── a company, a managed warehouse with a line, a bill of materials and a machine ─────
    let inr = t.find("Currency", json!({"code": "INR"})).await;
    let company = uid(&t.insert("Company", json!({"company_name": "Line Co", "abbr": "LNC", "default_currency": inr["id"]})).await);
    t.wh = uid(&t.find("Warehouse", json!({"warehouse_name": "Stores", "company": company.to_string()})).await);
    let wh = t.wh;
    let fg = uid(&t.find("Warehouse", json!({"warehouse_name": "Finished Goods", "company": company.to_string()})).await);
    let group = t.find("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find("Uom", json!({"uom_name": "Nos"})).await;
    let item = async |code: &str, batch: bool| {
        uid(&t.insert("Item", json!({"item_code": code, "item_name": code.to_lowercase(), "item_group": group["id"], "stock_uom": nos["id"], "is_stock_item": true, "has_batch_no": batch})).await)
    };
    let (widget, steel) = (item("WIDGET", true).await, item("STEEL", true).await);
    for lot in ["S-1", "S-2"] {
        t.insert("Batch", json!({"batch_id": lot, "item": steel})).await;
    }
    let zone = async |code: &str, kind: &str| uid(&t.insert("StorageZone", json!({"warehouse": wh, "zone_code": code, "kind": kind})).await);
    let (rcv, sto, shp, qua, lin) = (
        zone("RCV", "Receiving").await,
        zone("STO", "Storage").await,
        zone("SHP", "Shipping").await,
        zone("QUA", "Quarantine").await,
        zone("LIN", "Production").await,
    );
    let bin = async |code: &str, zone: Uuid| uid(&t.insert("StorageBin", json!({"warehouse": wh, "zone": zone, "bin_code": code})).await);
    let (dock, door, jail, _shelf, line) =
        (bin("DOCK", rcv).await, bin("DOOR", shp).await, bin("JAIL", qua).await, bin("A-01", sto).await, bin("LINE", lin).await);
    let site = uid(&t.insert("Site", json!({"site_name": "Plant", "site_code": "plant"})).await);
    let managed = uid(&t.insert("ManagedWarehouse", json!({"warehouse": wh, "site": site, "inbound_bin": dock, "outbound_bin": door, "quarantine_bin": jail, "production_bin": line, "allocation_rule": "FEFO"})).await);
    k.run_action(&t.admin, "ManagedWarehouse", managed, "start_managing", json!({})).await.expect("managed");
    let opening = t
        .insert(
            "StockEntry",
            json!({"company": company, "purpose": "Material Receipt", "items": [
        {"item": steel, "qty": 30, "basic_rate": 10, "target_warehouse": wh, "batch_no": "S-1"},
        {"item": steel, "qty": 30, "basic_rate": 12, "target_warehouse": wh, "batch_no": "S-2"}]}),
        )
        .await;
    k.submit(&t.admin, "StockEntry", uid(&opening), None).await.expect("opening");
    for lot in ["S-1", "S-2"] {
        t.wms(
            &t.admin,
            "/positions/move",
            json!({"warehouse": wh, "item": steel, "lot": lot, "from_bin": "DOCK", "to_bin": "A-01", "qty": 30, "reason": "shelved"}),
        )
        .await
        .expect("shelved");
    }
    let bom = t.insert("Bom", json!({"company": company, "item": widget, "quantity": 1, "items": [{"item": steel, "qty": 2}]})).await;
    k.submit(&t.admin, "Bom", uid(&bom), None).await.expect("bom");
    let wo =
        t.insert("WorkOrder", json!({"company": company, "item": widget, "bom": uid(&bom), "qty": 30, "source_warehouse": wh, "target_warehouse": fg})).await;
    k.submit(&t.admin, "WorkOrder", uid(&wo), None).await.expect("work order");
    let device = async |code: &str, kind: &str, ops: Value| {
        let id = uid(&t.insert("Device", json!({"device_name": code, "device_code": code, "kind": kind, "site": site, "operations": ops})).await);
        devices::set_lifecycle(&k, &t.admin, id, Lifecycle::Claimed, "ours").await.unwrap();
        let (code, _) = ferrum_ops::identity::new_enrollment(&k, &t.admin, id).await.unwrap();
        ferrum_ops::identity::enroll(&k, tenant, &ferrum_ops::contract::sign::Enroll { code, public_key: hex_key(id) }).await.unwrap();
        devices::set_lifecycle(&k, &t.admin, id, Lifecycle::Commissioned, "checked").await.unwrap();
        devices::set_lifecycle(&k, &t.admin, id, Lifecycle::Active, "in service").await.unwrap();
        id
    };
    device("GW", "Gateway", json!([])).await;
    let press = device("PRESS-1", "Machine", json!([{"operation": "machine.start_job", "engine_only": true}])).await;
    let station =
        uid(&t.insert("Workstation", json!({"workstation_name": "Press 1", "company": company, "production_capacity": 1, "mes_device": press})).await);
    let operation = uid(&t.insert("Operation", json!({"operation_name": "Pressing", "workstation": station})).await);
    let supervisor = t.person("sup@line.test", &["Production Supervisor"]).await;
    let operator = t.person("op@line.test", &["Production Operator"]).await;
    let storeman = t.person("wm@line.test", &["Warehouse Manager", "Warehouse Operator"]).await;
    let quality = t.person("qa@line.test", &["Quality Manager"]).await;
    let today = Utc::now().date_naive();
    t.insert(
        "OperatorQualification",
        json!({"user_email": "op@line.test", "operation": operation, "valid_until": (today + chrono::Duration::days(30)).to_string()}),
    )
    .await;
    let revision =
        uid(&t.insert("ProductionRevision", json!({"item": widget, "revision": "A", "bom": uid(&bom), "job_number": 7, "ideal_cycle_secs": 60})).await);
    t.mes(&supervisor, &format!("/revisions/{revision}/approve"), json!({})).await.expect("approved");
    let run = async |qty: i64| {
        uid(&t.insert("ProductionRun", json!({"company": company, "work_order": uid(&wo), "revision": revision, "operation": operation, "workstation": station, "operator_email": "op@line.test", "qty": qty})).await)
    };
    let release = async |id: Uuid| t.mes(&supervisor, &format!("/runs/{id}/release"), json!({})).await;

    // ───── released, and the warehouse has not heard yet: the machine stays off ─────
    let first = run(6).await;
    let released = release(first).await.expect("released");
    assert_eq!(released["material_status"], json!("Waiting"));
    assert_eq!(t.commands_of(first).await, 0, "the warehouse's consumer is late, and nothing depends on it being quick");
    assert_eq!(t.line_tasks(first).await.len(), 0);

    // Two runs after the same stock: 60 exist, 12 are taken, 48 are left for the second.
    let greedy = run(25).await;
    assert!(release(greedy).await.is_err_and(|e| text(&e).contains("STEEL: 48 of 50")), "the second cannot have what the first set aside");
    let second = run(14).await;
    release(second).await.expect("what is left");

    t.deliver().await;
    assert_eq!(t.run(first).await["material_status"], json!("Reserved"), "the warehouse has it in hand");
    assert_eq!(t.commands_of(first).await, 0, "and it is not at the machine yet");
    let moves = t.line_tasks(first).await;
    assert!(!moves.is_empty(), "the warehouse opened moves to the line");

    // ───── a lot goes on hold on its way: the run does not start ─────
    let hold = uid(&t.insert("QualityHold", json!({"item": steel, "lot": moves[0]["lot"], "reason": "Certificate missing"})).await);
    t.deliver().await;
    assert_eq!(t.run(first).await["material_status"], json!("Short"), "{:?}", t.run(first).await);
    let id = moves[0]["id"].as_str().unwrap().to_string();
    t.wms(&storeman, &format!("/tasks/{id}/release"), json!({})).await.expect("released");
    let body = json!({"from_scan": "A-01", "to_scan": "LINE", "item_scan": "STEEL", "lot_scan": moves[0]["lot"]});
    assert!(
        t.wms(&storeman, &format!("/tasks/{id}/confirm"), body).await.is_err_and(|e| text(&e).contains("quality hold")),
        "held stock does not reach the line"
    );
    assert_eq!(t.at("LINE", "STEEL").await, Decimal::ZERO);
    assert_eq!(t.commands_of(first).await, 0, "and nothing started the machine");
    t.call("quality", &quality, "POST", &format!("/holds/{hold}/release"), Value::Null, json!({"note": "Certificate arrived"})).await.expect("lifted");
    t.deliver().await;
    assert_eq!(t.run(first).await["material_status"], json!("Reserved"), "lifted, it goes on");

    // ───── the last of it reaches the line, and only that starts the machine ─────
    for (i, m) in t.line_tasks(first).await.iter().enumerate() {
        let id = m["id"].as_str().unwrap().to_string();
        assert_eq!(t.commands_of(first).await, 0, "not while anything is on its way: task {i}");
        if m["status"] == json!("Open") {
            t.wms(&storeman, &format!("/tasks/{id}/release"), json!({})).await.expect("released");
        }
        t.wms(&storeman, &format!("/tasks/{id}/confirm"), json!({"from_scan": "A-01", "to_scan": "LINE", "item_scan": "STEEL", "lot_scan": m["lot"]}))
            .await
            .expect("brought");
        t.deliver().await;
    }
    let ready = t.run(first).await;
    assert_eq!(ready["material_status"], json!("At Station"), "{ready:?}");
    let command: Uuid = ready["command"].as_str().expect("the machine is told").parse().unwrap();
    let start = commands::describe(&k, &t.admin, command).await.unwrap();
    assert_eq!((start["operation"].as_str(), start["state"].as_str()), (Some("machine.start_job"), Some("Queued")));
    assert_eq!(start["payload"]["job"], json!(7));
    assert_eq!(t.at("LINE", "STEEL").await, n("12"));
    t.reconciled().await;

    // The same word again starts nothing new.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    ferrum_mes::runs::materials_arrived(&k, &t.admin, &mut tx, first).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(t.commands_of(first).await, 1);

    // Nor does it once the job has ended: a start that finished is not a start to send again.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    commands::take_for_site(&mut tx, tenant, "plant", 1, 10, GATEWAY, false).await.unwrap();
    let ended = Report {
        report_id: Uuid::now_v7(),
        command_id: command,
        state: State::Completed,
        at_source: Utc::now(),
        epoch: 1,
        detail: Value::Null,
        evidence: Some(json!({"cycle": "done"})),
    };
    commands::apply(&mut tx, tenant, &ended, 1, GATEWAY).await.unwrap();
    tx.commit().await.unwrap();
    t.deliver().await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    ferrum_mes::runs::materials_arrived(&k, &t.admin, &mut tx, first).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(t.commands_of(first).await, 1, "the machine ran this already");

    // A run released before material was tracked has a start and no status: it can still be confirmed.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    sqlx::query("update e_production_run set material_status = null where id = $1").bind(first).execute(&mut *tx).await.unwrap();
    ferrum_mes::runs::materials_arrived(&k, &t.admin, &mut tx, first).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(t.run(first).await["material_status"], json!("At Station"));

    // ───── what is at the line for one run is not at the line for the next ─────
    t.deliver().await;
    let rest = t.line_tasks(second).await;
    let brought: Decimal = rest.iter().map(|m| dec(&m["qty"])).sum();
    assert_eq!(brought, n("28"));
    assert_eq!(t.commands_of(second).await, 0);
    let third = run(4).await;
    release(third).await.expect("released");
    t.deliver().await;
    assert_eq!(t.at("LINE", "STEEL").await, n("12"), "twelve stand at the line, and they are the first run's");
    let own: Decimal = t.line_tasks(third).await.iter().map(|m| dec(&m["qty"])).sum();
    assert_eq!(own, n("8"), "so this run brings all of its own");
    assert_eq!(t.run(third).await["material_status"], json!("Reserved"), "and is not told it is ready on stock it cannot have");
    assert_eq!(t.commands_of(third).await, 0);

    // ───── the run happens: material in, good out, posted once, and everything still adds up ─────
    t.mes(&operator, &format!("/runs/{first}/events"), json!({"event_id": Uuid::now_v7(), "kind": "start"})).await.expect("started");
    t.mes(
        &operator,
        &format!("/runs/{first}/events"),
        json!({"event_id": Uuid::now_v7(), "kind": "consume", "item": steel, "lot": moves[0]["lot"], "qty": 12}),
    )
    .await
    .expect("consumed");
    let scrapped =
        t.mes(&operator, &format!("/runs/{first}/events"), json!({"event_id": Uuid::now_v7(), "kind": "report", "good": 5, "scrap": 1, "lot": "W-1"})).await;
    scrapped.expect("reported");
    assert_eq!(t.at("LINE", "STEEL").await, Decimal::ZERO, "what the books issued left the line");
    t.reconciled().await;
    t.mes(&operator, &format!("/runs/{first}/events"), json!({"event_id": Uuid::now_v7(), "kind": "complete"})).await.expect("finished");
    t.deliver().await;
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let open: i64 =
        sqlx::query_scalar("select count(*) from wms_supply where run_id = $1 and closed_at is null").bind(first).fetch_one(&mut *tx).await.unwrap();
    let kept: i64 = sqlx::query_scalar("select count(*) from mes_reservation where run_id = $1").bind(first).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!((open, kept), (0, 0), "a finished run lets go of the line and of what it set aside");

    // ───── a machine that refuses: a person sees it, and the stock stays put ─────
    t.deliver_all(&storeman, second).await;
    t.deliver().await;
    let waiting = t.run(second).await;
    assert_eq!(waiting["material_status"], json!("At Station"), "{waiting:?}");
    let c: Uuid = waiting["command"].as_str().unwrap().parse().unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    ferrum_mes::runs::on_command(&k, &t.admin, &mut tx, second, State::OutcomeUnknown).await.unwrap();
    tx.commit().await.unwrap();
    assert!(t.run(second).await["note"].as_str().unwrap().contains("Nobody can say"));
    assert_eq!(commands::describe(&k, &t.admin, c).await.unwrap()["state"], json!("Queued"));
    assert_eq!(t.at("LINE", "STEEL").await, n("28"));
    t.reconciled().await;

    // ───── what cannot be found is said, and nothing starts ─────
    let more =
        t.insert("WorkOrder", json!({"company": company, "item": widget, "bom": uid(&bom), "qty": 5, "source_warehouse": wh, "target_warehouse": fg})).await;
    k.submit(&t.admin, "WorkOrder", uid(&more), None).await.expect("work order");
    let unreleasable = uid(&t.insert("ProductionRun", json!({"company": company, "work_order": uid(&more), "revision": revision, "operation": operation, "workstation": station, "operator_email": "op@line.test", "qty": 7})).await);
    assert!(release(unreleasable).await.is_err_and(|e| text(&e).contains("material is short")), "everything is set aside or gone");
    let _ = (fg, widget);

    // ───── a mission lost on its way to the line: the run stops, and gets what is found ─────
    let lose = async |id: &str| {
        let mut tx = k.db.tenant_tx(tenant).await.unwrap();
        ferrum_wms::tasks::stuck(&k, &storeman, &mut tx, id.parse().unwrap(), "Nobody can say what happened to this mission").await.unwrap();
        tx.commit().await.unwrap();
        t.deliver().await;
    };
    let found = async |id: &str, bin: &str, qty: Decimal| {
        t.wms(&storeman, &format!("/tasks/{id}/recover"), json!({"found_bin": bin, "qty": qty, "note": "Walked the aisle"})).await.expect("found");
        t.deliver().await;
    };
    let tasks = t.line_tasks(third).await;
    let wanted: Decimal = tasks.iter().map(|m| dec(&m["qty"])).sum();
    let first_id = tasks[0]["id"].as_str().unwrap().to_string();
    lose(&first_id).await;
    assert_eq!(t.run(third).await["material_status"], json!("Short"), "nothing starts on stock nobody can find");
    found(&first_id, tasks[0]["from_bin"].as_str().unwrap(), dec(&tasks[0]["qty"])).await;
    assert_eq!(t.run(third).await["material_status"], json!("Reserved"), "it never left, and someone will still bring it");
    let again = t.line_tasks(third).await;
    assert_eq!(again.iter().map(|m| dec(&m["qty"])).sum::<Decimal>(), wanted, "all of it is on a task again");
    assert!(again.iter().all(|m| m["id"] != json!(first_id)));
    assert_eq!(t.commands_of(third).await, 0);
    // Found at the line itself: that is arrival, and the last of it there starts the machine.
    let (last, others) = again.split_last().unwrap();
    for m in others {
        let id = m["id"].as_str().unwrap();
        t.wms(&storeman, &format!("/tasks/{id}/release"), json!({})).await.expect("released");
        t.wms(&storeman, &format!("/tasks/{id}/confirm"), json!({"from_scan": m["from_bin"], "to_scan": "LINE", "item_scan": m["item"], "lot_scan": m["lot"]}))
            .await
            .expect("brought");
    }
    t.deliver().await;
    assert_eq!(t.commands_of(third).await, 0, "not all of it yet");
    lose(last["id"].as_str().unwrap()).await;
    found(last["id"].as_str().unwrap(), "LINE", dec(&last["qty"])).await;
    assert_eq!(t.run(third).await["material_status"], json!("At Station"), "found where it was going");
    assert_eq!(t.commands_of(third).await, 1);
    assert!(t.line_tasks(third).await.is_empty());
    t.reconciled().await;
}
