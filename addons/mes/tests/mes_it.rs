//! A work order on the floor, against a real PostgreSQL (`FERRUM_MES_TEST_DATABASE_URL`, reset
//! first): the release gate, what happened as events, real postings to ERP, a lot followed through
//! both ways, and OEE worked out by hand.

use chrono::{DateTime, Duration, SubsecRound, Utc};
use ferrum_kernel::query::parse_filters;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, SiteRequest, outbox};
use ferrum_meta::Catalog;
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

    async fn api(&self, ctx: &Ctx, method: &str, path: &str, query: Value, body: Value) -> Result<Value, Error> {
        let site = self.k.hooks.site("mes").unwrap().clone();
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
        site.handle(&self.k, req).await.map(|r| serde_json::from_slice(&r.body).unwrap_or(Value::Null))
    }

    async fn release(&self, ctx: &Ctx, run: Uuid) -> Result<Value, Error> {
        self.api(ctx, "POST", &format!("/runs/{run}/release"), Value::Null, json!({})).await
    }

    /// A supervisor says the material is at the station.
    async fn materials(&self, ctx: &Ctx, run: Uuid) -> Result<Value, Error> {
        self.api(ctx, "POST", &format!("/runs/{run}/materials"), Value::Null, json!({"found": "Checked the rack beside it"})).await
    }

    async fn commands_of(&self, run: Uuid) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let n = sqlx::query_scalar("select count(*) from op_command where tenant_id = $1 and workflow_id = $2")
            .bind(self.tenant)
            .bind(format!("mes-run:{run}"))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        n
    }

    async fn event(&self, ctx: &Ctx, run: Uuid, mut body: Value) -> Result<Value, Error> {
        if body.get("event_id").is_none() {
            body["event_id"] = json!(Uuid::now_v7());
        }
        self.api(ctx, "POST", &format!("/runs/{run}/events"), Value::Null, body).await
    }

    async fn balance(&self, item: Uuid, wh: Uuid) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        let q: Option<Decimal> = sqlx::query_scalar("select qty from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = $3")
            .bind(self.tenant)
            .bind(item)
            .bind(wh)
            .fetch_optional(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        q.unwrap_or_default()
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

#[test]
fn a_work_order_on_the_floor() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(32 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

fn hex_key(seed: Uuid) -> String {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(seed.as_bytes());
    ed25519_dalek::SigningKey::from_bytes(&bytes).verifying_key().to_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

async fn scenario() {
    let url = std::env::var("FERRUM_MES_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_mes_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().unwrap();
    packages.extend(ferrum_app_erp::packages().unwrap());
    packages.extend(ferrum_stock_ops::packages().unwrap());
    packages.extend(ferrum_mfg_ops::packages().unwrap());
    packages.extend(ferrum_quality::packages().unwrap());
    packages.extend(ferrum_ops::packages().unwrap());
    packages.extend(ferrum_mes::packages().unwrap());
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_stock_ops::register(&mut hooks);
    ferrum_mfg_ops::register(&mut hooks);
    ferrum_quality::register(&mut hooks);
    ferrum_ops::register(&mut hooks);
    ferrum_mes::register(&mut hooks);
    let catalog = Catalog::from_packages(packages).unwrap();
    assert_eq!(catalog.links_out_of_reach("mes"), Vec::<String>::new(), "whoever fills in a link can look up what it points at");
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.unwrap();
    ferrum_app_erp::maintenance(&k).await.unwrap();
    let tenant = k
        .create_tenant(NewTenant {
            slug: "plant".into(),
            name: "Plant Co".into(),
            admin_email: "admin@plant.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["mes".into()],
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("tenant");
    let session = k.login("plant", "admin@plant.test", "Correct-Horse-9").await.unwrap();
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), tenant, admin };

    // ───── a company, materials, a bill of materials and a machine ─────
    let inr = t.find("Currency", json!({"code": "INR"})).await;
    let company = uid(&t.insert("Company", json!({"company_name": "Plant Co", "abbr": "PLC", "default_currency": inr["id"]})).await);
    let stores = uid(&t.find("Warehouse", json!({"warehouse_name": "Stores", "company": company.to_string()})).await);
    let fg = uid(&t.find("Warehouse", json!({"warehouse_name": "Finished Goods", "company": company.to_string()})).await);
    let group = t.find("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find("Uom", json!({"uom_name": "Nos"})).await;
    let item = async |code: &str, batch: bool| {
        uid(&t.insert("Item", json!({"item_code": code, "item_name": code.to_lowercase(), "item_group": group["id"], "stock_uom": nos["id"], "is_stock_item": true, "has_batch_no": batch})).await)
    };
    let (widget, steel, oil) = (item("WIDGET", true).await, item("STEEL", true).await, item("OIL", false).await);
    for lot in ["S-1", "S-2"] {
        t.insert("Batch", json!({"batch_id": lot, "item": steel})).await;
    }
    let opening = t
        .insert(
            "StockEntry",
            json!({"company": company, "purpose": "Material Receipt", "items": [
        {"item": steel, "qty": 30, "basic_rate": 10, "target_warehouse": stores, "batch_no": "S-1"},
        {"item": steel, "qty": 30, "basic_rate": 12, "target_warehouse": stores, "batch_no": "S-2"},
        {"item": oil, "qty": 100, "basic_rate": 1, "target_warehouse": stores}]}),
        )
        .await;
    k.submit(&t.admin, "StockEntry", uid(&opening), None).await.unwrap();
    let bom = t.insert("Bom", json!({"company": company, "item": widget, "quantity": 1, "items": [{"item": steel, "qty": 2}, {"item": oil, "qty": 1}]})).await;
    k.submit(&t.admin, "Bom", uid(&bom), None).await.expect("bom");
    let other_bom = t
        .insert("Bom", json!({"company": company, "item": widget, "revision": "B", "is_default": false, "quantity": 1, "items": [{"item": steel, "qty": 3}]}))
        .await;
    k.submit(&t.admin, "Bom", uid(&other_bom), None).await.expect("other bom");
    let wo = t
        .insert("WorkOrder", json!({"company": company, "item": widget, "bom": uid(&bom), "qty": 20, "source_warehouse": stores, "target_warehouse": fg}))
        .await;
    k.submit(&t.admin, "WorkOrder", uid(&wo), None).await.expect("work order");
    let site = uid(&t.insert("Site", json!({"site_name": "Plant", "site_code": "plant"})).await);
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
    t.insert("DowntimeReason", json!({"reason": "Jam", "category": "Breakdown"})).await;
    t.insert("DowntimeReason", json!({"reason": "Lunch", "category": "Planned", "planned": true})).await;

    let supervisor = t.person("sup@plant.test", &["Production Supervisor"]).await;
    let operator = t.person("op@plant.test", &["Production Operator"]).await;
    let other = t.person("new@plant.test", &["Production Operator"]).await;

    let revision = uid(&k
        .insert(&supervisor, "ProductionRevision", obj(json!({"item": widget, "revision": "A", "bom": uid(&bom), "job_number": 412, "ideal_cycle_secs": 60})))
        .await
        .expect("revision"));
    let run = async |rev: Uuid, email: &str, qty: i64| {
        uid(&t.insert("ProductionRun", json!({"company": company, "work_order": uid(&wo), "revision": rev, "operation": operation, "workstation": station, "operator_email": email, "qty": qty})).await)
    };

    // ───── the gate: each refuses by name, and a viewer of the floor cannot open it ─────
    let r1 = run(revision, "op@plant.test", 10).await;
    assert!(matches!(t.release(&operator, r1).await, Err(Error::Forbidden(_))), "an operator does not release");
    assert!(t.release(&supervisor, r1).await.is_err_and(|e| text(&e).contains("not approved")));
    assert!(
        t.api(&supervisor, "POST", &format!("/revisions/{revision}/approve"), Value::Null, json!({})).await.is_err_and(|e| matches!(e, Error::Forbidden(_))),
        "its author cannot approve it"
    );
    t.api(&t.admin, "POST", &format!("/revisions/{revision}/approve"), Value::Null, json!({})).await.expect("approved");
    assert!(k.update(&t.admin, "ProductionRevision", revision, obj(json!({"job_number": 413})), None).await.is_err(), "an approved revision does not change");
    let wrong_bom = uid(&k.insert(&supervisor, "ProductionRevision", obj(json!({"item": widget, "revision": "X", "bom": uid(&other_bom)}))).await.unwrap());
    t.api(&t.admin, "POST", &format!("/revisions/{wrong_bom}/approve"), Value::Null, json!({})).await.unwrap();
    assert_eq!(t.get("ProductionRevision", revision).await["status"], json!("Superseded"), "the newest approved takes over");
    let rev_b = uid(&k
        .insert(&supervisor, "ProductionRevision", obj(json!({"item": widget, "revision": "B", "bom": uid(&bom), "job_number": 412, "ideal_cycle_secs": 60})))
        .await
        .unwrap());
    let r_wrong = run(wrong_bom, "op@plant.test", 10).await;
    assert!(t.release(&supervisor, r_wrong).await.is_err(), "a superseded or foreign revision does not run");
    t.api(&t.admin, "POST", &format!("/revisions/{rev_b}/approve"), Value::Null, json!({})).await.unwrap();
    let r_old = run(revision, "op@plant.test", 10).await;
    assert!(t.release(&supervisor, r_old).await.is_err_and(|e| text(&e).contains("not approved")), "the old revision is not approved any more");
    let main = run(rev_b, "op@plant.test", 10).await;
    assert!(t.release(&supervisor, main).await.is_err_and(|e| text(&e).contains("not qualified")));
    let today = Utc::now().date_naive();
    t.insert("OperatorQualification", json!({"user_email": "op@plant.test", "operation": operation, "valid_until": (today - Duration::days(1)).to_string()}))
        .await;
    assert!(t.release(&supervisor, main).await.is_err_and(|e| text(&e).contains("not qualified")), "a lapsed qualification is none");
    let qualification = uid(&t.find("OperatorQualification", json!({"user_email": "op@plant.test"})).await);
    k.update(&t.admin, "OperatorQualification", qualification, obj(json!({"valid_until": (today + Duration::days(30)).to_string()})), None).await.unwrap();
    devices::set_lifecycle(&k, &t.admin, press, Lifecycle::Maintenance, "bearing").await.unwrap();
    assert!(t.release(&supervisor, main).await.is_err_and(|e| text(&e).contains("Maintenance")), "equipment under maintenance cannot run");
    devices::set_lifecycle(&k, &t.admin, press, Lifecycle::Active, "repaired").await.unwrap();
    let short = run(rev_b, "op@plant.test", 40).await;
    assert!(t.release(&supervisor, short).await.is_err_and(|e| text(&e).contains("material is short")), "more steel than there is");
    let hold = uid(&t.insert("QualityHold", json!({"item": steel, "lot": "S-1", "reason": "Certificate missing"})).await);
    let held = run(rev_b, "op@plant.test", 20).await;
    assert!(t.release(&supervisor, held).await.is_err_and(|e| text(&e).contains("STEEL")), "held steel does not count as there");

    // ───── released: material is set aside, and the machine waits for it ─────
    let released = t.release(&supervisor, main).await.expect("released");
    assert_eq!((&released["status"], &released["material_status"]), (&json!("Released"), &json!("Waiting")));
    assert_eq!(released["command"], Value::Null, "stock somewhere in a warehouse is not stock in the machine");
    assert_eq!(t.commands_of(main).await, 0, "and nothing reached the ledger");
    assert!(t.event(&operator, main, json!({"kind": "start"})).await.is_err_and(|e| text(&e).contains("not at the station")), "nor can it be started by hand");
    let rival = run(rev_b, "op@plant.test", 6).await;
    assert!(
        t.release(&supervisor, rival).await.is_err_and(|e| text(&e).contains("STEEL: 10 of 12")),
        "what one run set aside is not there for the next: 30 usable, 20 of it taken"
    );
    assert!(
        t.api(&supervisor, "POST", &format!("/runs/{main}/materials"), Value::Null, json!({"found": " "})).await.is_err(),
        "saying so takes saying what was checked"
    );
    assert!(t.materials(&operator, main).await.is_err_and(|e| matches!(e, Error::Forbidden(_))), "and it is a supervisor's to say");

    // ───── the material is there: the machine gets a reference, never a program ─────
    let released = t.materials(&supervisor, main).await.expect("confirmed");
    assert_eq!(t.get("ProductionRun", main).await["material_status"], json!("At Station"));
    let cmd: Uuid = released["command"].as_str().unwrap().parse().unwrap();
    assert_eq!(t.materials(&supervisor, main).await.unwrap()["command"], released["command"], "saying it again starts nothing new");
    assert_eq!(t.commands_of(main).await, 1);
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let audited: i64 =
        sqlx::query_scalar("select count(*) from audit_event where entity = 'ProductionRun' and record_id = $1 and action = 'mes.materials_confirmed'")
            .bind(main)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert!(audited >= 1, "who said so stays on record");
    let in_ledger = ferrum_ops::commands::describe(&k, &t.admin, cmd).await.unwrap();
    assert_eq!(in_ledger["payload"]["job"], json!(412));
    assert_eq!(in_ledger["operation"], json!("machine.start_job"));
    assert_eq!(in_ledger["state"], json!("Queued"));
    assert!(t.release(&supervisor, main).await.is_err(), "not twice");

    // ───── what happened, as it happened ─────
    // Whole seconds: Linux clocks have nanoseconds and the database keeps microseconds, so an event
    // exactly at the start of a period would otherwise round to just before it.
    let t0 = Utc::now().trunc_subsecs(0) - Duration::hours(2);
    let at = |m: i64| (t0 + Duration::minutes(m)).to_rfc3339();
    assert!(t.event(&operator, r1, json!({"kind": "start"})).await.is_err(), "not released");
    t.event(&operator, main, json!({"kind": "start", "at": at(0)})).await.expect("started");
    let wrong = t.event(&operator, main, json!({"kind": "consume", "item": widget, "qty": 1})).await;
    assert!(wrong.is_err_and(|e| text(&e).contains("bill of materials")), "only what the BOM says");
    let suspect = t.event(&operator, main, json!({"kind": "consume", "item": steel, "lot": "S-1", "qty": 1})).await;
    assert!(suspect.is_err_and(|e| text(&e).contains("hold")), "held material is refused at the machine too");
    let quality = t.person("qa@plant.test", &["Quality Manager"]).await;
    let site_ep = k.hooks.site("quality").unwrap().clone();
    let req = SiteRequest {
        tenant,
        user: Some(quality.clone()),
        method: "POST".into(),
        path: format!("/holds/{hold}/release"),
        query: Map::new(),
        headers: Map::new(),
        body: json!({"note": "Certificate arrived"}).to_string().into_bytes(),
        client_ip: "::1".into(),
    };
    site_ep.handle(&k, req).await.expect("released");
    let lot_scan = Uuid::now_v7();
    let scan = json!({"kind": "consume", "item": steel, "lot": "S-1", "qty": 6, "event_id": lot_scan});
    t.event(&operator, main, scan.clone()).await.expect("consumed");
    assert_eq!(t.event(&operator, main, scan).await.unwrap()["repeated"], json!(true), "a terminal repeating itself counts once");
    t.event(&operator, main, json!({"kind": "consume", "item": steel, "lot": "S-2", "qty": 6})).await.expect("second lot");
    assert!(t.event(&operator, main, json!({"kind": "pause", "reason": "Felt like it", "at": at(20)})).await.is_err(), "a stop has a reason from the list");
    t.event(&operator, main, json!({"kind": "pause", "reason": "Jam", "at": at(20)})).await.expect("paused");
    t.event(&operator, main, json!({"kind": "resume", "at": at(30)})).await.expect("resumed");
    t.event(&operator, main, json!({"kind": "pause", "reason": "Lunch", "at": at(40)})).await.expect("planned stop");
    t.event(&operator, main, json!({"kind": "resume", "at": at(50)})).await.expect("resumed");

    // A lot-tracked product says which lot; a report beyond what was consumed by lot is refused.
    let no_lot = t.event(&operator, main, json!({"kind": "report", "good": 2})).await;
    assert!(no_lot.is_err_and(|e| text(&e).contains("lot")));
    let too_much = t.event(&operator, main, json!({"kind": "report", "good": 8, "lot": "W-1"})).await;
    assert!(too_much.is_err_and(|e| text(&e).contains("must be consumed")), "16 steel needed and 12 consumed");
    assert_eq!(t.balance(widget, fg).await, Decimal::ZERO, "and nothing posted");
    let report = Uuid::now_v7();
    let good = json!({"kind": "report", "good": 4, "lot": "W-1", "at": at(55), "event_id": report});
    t.event(&operator, main, good.clone()).await.expect("reported");
    assert_eq!(t.event(&operator, main, good).await.unwrap()["repeated"], json!(true));
    assert_eq!(t.balance(widget, fg).await, n("4"), "a retry does not post twice");
    assert_eq!(t.balance(steel, stores).await, n("52"), "8 steel went in");
    assert_eq!(t.balance(oil, stores).await, n("96"));
    t.event(&operator, main, json!({"kind": "report", "good": 1, "scrap": 1, "rework": 2, "lot": "W-1", "at": at(58)})).await.expect("scrap and rework");
    assert_eq!(t.balance(widget, fg).await, n("5"));
    assert_eq!(t.balance(steel, stores).await, n("48"), "scrap used steel too");
    assert_eq!(dec(&t.get("WorkOrder", uid(&wo)).await["produced_qty"]), n("5"));
    let doc = t.get("ProductionRun", main).await;
    assert_eq!((dec(&doc["good_qty"]), dec(&doc["scrap_qty"]), dec(&doc["rework_qty"])), (n("5"), n("1"), n("2")));
    assert!(t.event(&operator, main, json!({"kind": "report", "good": 9, "lot": "W-1"})).await.is_err(), "more than the run");
    assert!(
        t.event(&operator, main, json!({"kind": "complete", "at": at(60)})).await.is_err_and(|e| text(&e).contains("short")),
        "short of its quantity: say why"
    );
    t.event(&operator, main, json!({"kind": "complete", "note": "Steel ran out", "at": at(60)})).await.expect("completed");
    assert!(t.event(&operator, main, json!({"kind": "report", "good": 1, "lot": "W-1"})).await.is_err(), "finished is finished");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    assert!(sqlx::query("delete from mes_event where tenant_id = $1").bind(tenant).execute(&mut *tx).await.is_err(), "the record cannot be edited away");
    drop(tx);

    // ───── a lot, followed both ways ─────
    let forward = t.api(&operator, "GET", "/trace/forward", json!({"item": steel, "lot": "S-2"}), Value::Null).await.unwrap();
    let steps = forward["steps"].as_array().unwrap();
    assert!(steps.iter().any(|s| s["into_item"] == json!("WIDGET") && s["into_lot"] == json!("W-1")), "{forward}");
    let back = t.api(&operator, "GET", "/trace/back", json!({"item": widget, "lot": "W-1"}), Value::Null).await.unwrap();
    let lots: Vec<&str> = back["steps"].as_array().unwrap().iter().filter_map(|s| s["lot"].as_str()).collect();
    assert!(lots.contains(&"S-1") && lots.contains(&"S-2"), "{back}");
    let consumed: Decimal = sqlx_sum(&t, main).await;
    assert_eq!(consumed, n("12"), "every kilo of steel taken in is accounted for: 10 in product, 2 in scrap");

    // ───── OEE, by hand ─────
    // 60 minutes: 10 stopped for a jam, 10 of lunch (planned, so not production time), 40 running.
    // Availability 40/50. Six units at 60 s each in 40 minutes: performance 6/40. Quality 5/6.
    let q = json!({"workstation": station, "from": t0.to_rfc3339(), "to": (t0 + Duration::minutes(60)).to_rfc3339()});
    let oee = t.api(&operator, "GET", "/oee", q, Value::Null).await.expect("oee");
    let f = &oee["figures"];
    let close = |v: &Value, want: f64| (v.as_f64().unwrap() - want).abs() < 1e-6;
    assert!(close(&f["availability"], 0.8), "{oee}");
    assert!(close(&f["performance"], 0.15), "{oee}");
    assert!(close(&f["quality"], 5.0 / 6.0), "{oee}");
    assert!(close(&f["oee"], 0.8 * 0.15 * 5.0 / 6.0), "{oee}");
    assert_eq!(f["version"], json!(1), "a figure says which definition it used");

    // ───── a report from a terminal that was offline, arriving late as a fact ─────
    let r2 = run(rev_b, "op@plant.test", 2).await;
    t.release(&supervisor, r2).await.expect("released");
    t.materials(&supervisor, r2).await.expect("at the station");
    t.event(&operator, r2, json!({"kind": "start"})).await.unwrap();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    outbox::enqueue(
        &mut tx,
        tenant,
        "ops.fact.mes.event",
        None,
        None,
        json!({"actor": "pia", "body": {"run": r2, "event_id": Uuid::now_v7(), "kind": "pause", "reason": "Jam"}}),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert_eq!(t.get("ProductionRun", r2).await["status"], json!("Paused"));
    let reference = t.get("ProductionRun", r2).await["number"].clone();
    assert!(reference.as_str().unwrap().starts_with("RUN-"), "{reference}");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    outbox::enqueue(
        &mut tx,
        tenant,
        "ops.fact.mes.event",
        None,
        None,
        json!({"actor": "pia", "body": {"reference": reference, "event_id": Uuid::now_v7(), "kind": "resume"}}),
    )
    .await
    .unwrap();
    outbox::enqueue(
        &mut tx,
        tenant,
        "ops.fact.mes.event",
        None,
        None,
        json!({"actor": "pia", "body": {"reference": "NO-SUCH-RUN", "event_id": Uuid::now_v7(), "kind": "resume"}}),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert_eq!(t.get("ProductionRun", r2).await["status"], json!("Running"), "a console knows a run by the reference on its job sheet");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    outbox::enqueue(
        &mut tx,
        tenant,
        "ops.fact.mes.event",
        None,
        None,
        json!({"actor": "pia", "body": {"reference": reference, "event_id": Uuid::now_v7(), "kind": "report", "good": 9000}}),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    while k.dispatch_outbox_once().await.unwrap() > 0 {}
    assert!(t.get("ProductionRun", r2).await["note"].as_str().unwrap().contains("could not be applied"), "what a run cannot take, it says");

    // ───── a controller that refuses: a person sees it ─────
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    ferrum_mes::runs::on_command(&k, &t.admin, &mut tx, r2, State::OutcomeUnknown).await.unwrap();
    tx.commit().await.unwrap();
    assert!(t.get("ProductionRun", r2).await["note"].as_str().unwrap().contains("Nobody can say"));

    // ───── a hold after release: the start stops, and comes back only when it is lifted ─────
    let r3 = run(rev_b, "op@plant.test", 1).await;
    t.release(&supervisor, r3).await.expect("released");
    let first_start: Uuid = t.materials(&supervisor, r3).await.unwrap()["command"].as_str().unwrap().parse().unwrap();
    let late_hold = uid(&t.insert("QualityHold", json!({"item": oil, "reason": "Wrong viscosity"})).await);
    let stopped = t.get("ProductionRun", r3).await;
    assert_eq!(stopped["material_status"], json!("Short"), "{stopped:?}");
    assert_eq!(stopped["command"], Value::Null);
    assert!(stopped["note"].as_str().unwrap().contains("hold"), "{stopped:?}");
    assert_eq!(ferrum_ops::commands::describe(&k, &t.admin, first_start).await.unwrap()["state"], json!("Aborted"), "what had not left stops here");
    assert!(t.event(&operator, r3, json!({"kind": "start"})).await.is_err(), "nobody starts it by hand");
    assert!(
        t.materials(&supervisor, r3).await.is_err_and(|e| text(&e).contains("material is short")),
        "and saying the material is here does not make it usable"
    );
    assert!(t.materials(&supervisor, r3).await.is_err() && t.get("ProductionRun", r3).await["command"] == Value::Null, "nothing was started by trying");
    let req = SiteRequest {
        tenant,
        user: Some(quality.clone()),
        method: "POST".into(),
        path: format!("/holds/{late_hold}/release"),
        query: Map::new(),
        headers: Map::new(),
        body: json!({"note": "Retested"}).to_string().into_bytes(),
        client_ip: "::1".into(),
    };
    site_ep.handle(&k, req).await.expect("released");
    let second_start = t.materials(&supervisor, r3).await.expect("lifted")["command"].clone();
    assert_ne!(second_start, json!(first_start.to_string()), "a new attempt, since the first ended");

    // ───── the start asks again: what held at release may not hold now ─────
    let lapsing = run(rev_b, "op@plant.test", 1).await;
    t.release(&supervisor, lapsing).await.expect("released");
    k.update(&t.admin, "OperatorQualification", qualification, obj(json!({"valid_until": (today - Duration::days(1)).to_string()})), None).await.unwrap();
    assert!(t.materials(&supervisor, lapsing).await.is_err_and(|e| text(&e).contains("not qualified")), "a qualification that lapsed since");
    assert_eq!(t.commands_of(lapsing).await, 0);
    k.update(&t.admin, "OperatorQualification", qualification, obj(json!({"valid_until": (today + Duration::days(30)).to_string()})), None).await.unwrap();
    devices::set_lifecycle(&k, &t.admin, press, Lifecycle::Maintenance, "bearing").await.unwrap();
    assert!(t.materials(&supervisor, lapsing).await.is_err_and(|e| text(&e).contains("Maintenance")), "a machine that went down since");
    devices::set_lifecycle(&k, &t.admin, press, Lifecycle::Active, "repaired").await.unwrap();
    t.event(&supervisor, lapsing, json!({"kind": "cancel", "reason": "Not needed"})).await.expect("cancelled");

    // ───── a machine already loaded says so, on the record ─────
    let loaded = uid(&t.insert("ProductionRun", json!({"company": company, "work_order": uid(&wo), "revision": rev_b, "operation": operation, "workstation": station, "operator_email": "op@plant.test", "qty": 1, "material_source": "Preloaded"})).await);
    let released = t.release(&supervisor, loaded).await.expect("released");
    assert_eq!(released["material_status"], json!("Preloaded"));
    assert!(released["command"].is_string(), "its start waits for no delivery: {released}");
    t.event(&operator, loaded, json!({"kind": "start"})).await.expect("started");
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let said: i64 = sqlx::query_scalar("select count(*) from audit_event where entity = 'ProductionRun' and record_id = $1 and action = 'mes.preloaded'")
        .bind(loaded)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(said, 1, "whoever released it answers for that");

    // ───── a machine whose start can be asked for around this gate is not behind it ─────
    let open_press = device("PRESS-2", "Machine", json!([{"operation": "machine.start_job"}])).await;
    let open_station =
        uid(&t.insert("Workstation", json!({"workstation_name": "Press 2", "company": company, "production_capacity": 1, "mes_device": open_press})).await);
    let exposed = uid(&t.insert("ProductionRun", json!({"company": company, "work_order": uid(&wo), "revision": rev_b, "operation": operation, "workstation": open_station, "operator_email": "op@plant.test", "qty": 1})).await);
    assert!(t.release(&supervisor, exposed).await.is_err_and(|e| text(&e).contains("Through Its App Only")));

    // ───── a finished run lets go of what it set aside ─────
    t.event(&operator, r3, json!({"kind": "cancel", "reason": "Not needed"})).await.ok();
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    let kept: i64 = sqlx::query_scalar("select count(*) from mes_reservation where run_id = $1").bind(main).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(kept, 0);
    let _ = (other, DateTime::<Utc>::MIN_UTC);
}

async fn sqlx_sum(t: &T, run: Uuid) -> Decimal {
    let mut tx = t.k.db.tenant_tx(t.tenant).await.unwrap();
    let s: Decimal =
        sqlx::query_scalar("select coalesce(sum(qty), 0) from mes_event where run_id = $1 and kind = 'Consume'").bind(run).fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    s
}
