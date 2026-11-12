//! Carriers made by an older release, run by this one (`FERRUM_ERP_CARRIERUP_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_carrierup_test`; reset first). ERP 0.24 called carriers with
//! its own HTTP client and kept their keys on the carrier; this release makes the platform's connections call them. The
//! workspace is installed from the package YAML of ERP 0.24.1 (`OLD`, read from the repository's history with `git`),
//! carriers are made on it (one on, one off, a test carrier), then this build boots on the same database and the
//! workspace's administrator has not applied the update yet: the upkeep puts each carrier on a connection once, turned
//! on only if the carrier was, with its keys moved in and cleared from the carrier, nothing logged at warning level; the
//! carrier then buys a label with the key it had, and the update and a second upkeep change nothing.

mod common;
use common::*;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, HookRegistry, Kernel, KernelSettings};
use ferrum_meta::Catalog;
use serde_json::{Value, json};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const OLD: &str = "4620dd6";

/// ERP's packages as `OLD` had them.
fn old_packages() -> Result<Vec<ferrum_meta::Package>, String> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"));
    let git = |args: &[&str]| -> Result<Vec<u8>, String> {
        let out = Command::new("git").args(["-C", dir]).args(args).output().map_err(|e| e.to_string())?;
        if out.status.success() { Ok(out.stdout) } else { Err(String::from_utf8_lossy(&out.stderr).into_owned()) }
    };
    let listed = String::from_utf8(git(&["ls-tree", "-r", "--name-only", OLD, "--", "packages"])?).unwrap();
    let mut files: Vec<(&'static str, &'static str, &'static str)> = Vec::new();
    for path in listed.lines() {
        let (pkg, rel) = path.strip_prefix("packages/").and_then(|r| r.split_once('/')).ok_or("a path outside a package")?;
        let text = String::from_utf8(git(&["show", &format!("{OLD}:{path}")])?).map_err(|e| e.to_string())?;
        let leak = |s: &str| -> &'static str { Box::leak(s.to_string().into_boxed_str()) };
        files.push((leak(pkg), leak(rel), Box::leak(text.into_boxed_str())));
    }
    ferrum_meta::catalog::load_embedded(&files).map_err(|e| e.to_string())
}

#[test]
fn carriers_upgrade_from_0_24_1() {
    let Ok(old) = old_packages() else {
        assert!(std::env::var("CI").is_err(), "CI must fetch the whole history ({OLD} is read from it)");
        eprintln!("skipped: commit {OLD} is not in this checkout");
        return;
    };
    run(scenario(old));
}

async fn kernel(url: &str, erp: Vec<ferrum_meta::Package>, reset: bool) -> Arc<Kernel> {
    let db = Db::connect(DbConfig { url: url.to_string(), max_connections: 10, ..Default::default() }).await.expect("connect");
    if reset {
        sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    }
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(erp);
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    let settings =
        KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([7; 32])), webhooks_allow_private: true, ..KernelSettings::default() };
    let k = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, settings, None).await.expect("boot");
    k.set_file_store(Arc::new(MemFiles::default()));
    k
}

/// A carrier on a local port that wants the key it was given.
async fn carrier() -> (String, Arc<AtomicUsize>) {
    use axum::Json;
    use axum::http::HeaderMap;
    use axum::routing::post;
    let labels = Arc::new(AtomicUsize::new(0));
    let counted = labels.clone();
    let app = axum::Router::new().route("/v1/rates", post(|| async { Json(json!({"rates": [{"name": "Ground", "price": "9.50"}]})) })).route(
        "/v1/labels",
        post(move |headers: HeaderMap| {
            let counted = counted.clone();
            async move {
                if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some("Bearer key-123") {
                    return (axum::http::StatusCode::UNAUTHORIZED, Json(json!({"error": "bad key"})));
                }
                let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
                (axum::http::StatusCode::OK, Json(json!({"tracking": format!("TRK{n}"), "file": "UERG"})))
            }
        }),
    );
    let app = app.route("/v1/track/{t}", axum::routing::get(|| async { Json(json!({"s": "OK"})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, labels)
}

async fn upkeeps(k: &Arc<Kernel>, warnings: &Warnings, when: &str) {
    ferrum_domain::maintenance(k).await.expect("platform upkeep");
    ferrum_app_erp::maintenance(k).await.expect("ERP's upkeep");
    k.maintenance().await.expect("the kernel's upkeep");
    k.run_jobs_once(20).await.expect("jobs");
    ferrum_app_erp::maintenance(k).await.unwrap();
    assert_eq!(warnings.take(), Vec::<String>::new(), "{when}: the upkeeps logged");
}

async fn scenario(old: Vec<ferrum_meta::Package>) {
    let url =
        std::env::var("FERRUM_ERP_CARRIERUP_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_carrierup_test".into());
    let warnings = Warnings::install();
    let (base, labels) = carrier().await;
    let hmac_inbound = json!({"auth": "hmac", "header": "X-Signature", "tracking_at": "/t", "status_at": "/s", "statuses": {"OK": "Delivered"}});
    let desc = |inbound: Value| {
        json!({"rate": {"url": format!("{base}/v1/rates"), "headers": {"Authorization": "Bearer {{ secret.api_key }}"}, "body": "{\"weight\": {{ total_weight|json }}}",
                        "rates_at": "/rates", "service_at": "/name", "amount_at": "/price"},
               "label": {"url": format!("{base}/v1/labels"), "headers": {"Authorization": "Bearer {{ secret.api_key }}", "Idempotency-Key": "{{ key }}"},
                         "body": "{\"service\": {{ rate.service|json }}}", "tracking_at": "/tracking", "label_at": "/file", "label_type": "zpl", "error_at": "/error"},
               "track": {"url": format!("{base}/v1/track/{{{{ tracking_number }}}}"), "method": "GET", "status_at": "/s", "statuses": {"OK": "Delivered"}},
               "inbound": inbound})
    };

    // The old release's workspace, with carriers whose keys the carrier holds.
    let k = kernel(&url, old, true).await;
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    let names: Vec<String> = k
        .registry
        .catalog()
        .packages()
        .map(|p| p.name().to_string())
        .filter(|n| !["contacts", "project_records", "parties", "products", "manufacturing"].contains(&n.as_str()))
        .collect();
    let tenant = k
        .create_tenant(NewTenant {
            slug: "old".into(),
            name: "Old Co".into(),
            admin_email: "admin@old.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: names.clone(),
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("old", "admin@old.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let mut t = T { k: k.clone(), ctx, company: String::new() };
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    t.company = uid(&t.insert("Company", json!({"company_name": "Old Co", "abbr": "OC", "default_currency": inr["id"]})).await).to_string();
    let c = t.company.clone();
    let live = uid(&t
        .insert("CarrierConnection", json!({"connection_name": "Live", "company": c, "description": desc(hmac_inbound.clone()), "api_key": "key-123", "webhook_secret": "hook-456", "track_every_hours": 0}))
        .await);
    let off = uid(&t
        .insert("CarrierConnection", json!({"connection_name": "Off", "company": c, "enabled": false, "description": desc(Value::Null), "api_key": "key-off"}))
        .await);
    let fixtures = json!({"rate": {"rates": [{"name": "Test", "price": "1"}]}, "label": {"tracking": "FIX1", "file": "UERG"}, "track": {"s": "OK"}});
    let test = uid(&t
        .insert("CarrierConnection", json!({"connection_name": "Test", "company": c, "test": true, "description": desc(Value::Null), "fixtures": fixtures}))
        .await);
    let dock = uid(&t
        .insert(
            "Address",
            json!({"address_title": "Dock", "address_type": "Shipping", "address_line1": "1 Example Street", "city": "Springfield", "country": "India"}),
        )
        .await);
    assert_eq!(t.get("CarrierConnection", live).await["api_key"], json!(true), "0.24.1 keeps the key on the carrier");
    warnings.take();
    drop(t);

    // This build starts on that workspace before its administrator applies the update.
    let k = kernel(&url, ferrum_app_erp::packages().unwrap(), false).await;
    let session = k.login("old", "admin@old.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), ctx, company: c.clone() };
    upkeeps(&k, &warnings, "before the update").await;
    let sctx = ferrum_kernel::Ctx::system(tenant, t.ctx.schema.clone());
    let connections = || async { k.connection_defs(&sctx).await.unwrap().into_iter().filter(|d| d["id"].is_string()).collect::<Vec<_>>() };
    let check = || async {
        let defs = connections().await;
        assert_eq!(defs.len(), 3, "one connection each: {defs:?}");
        let of = |carrier: uuid::Uuid| {
            let defs = defs.clone();
            let t = &t;
            async move {
                let name = t.get("CarrierConnection", carrier).await["connection"].as_str().expect("on a connection").to_string();
                defs.into_iter().find(|d| d["name"] == json!(name)).expect("its connection")
            }
        };
        let (a, b, x) = (of(live).await, of(off).await, of(test).await);
        assert_eq!((a["enabled"].clone(), b["enabled"].clone(), x["enabled"].clone()), (json!(true), json!(false), json!(true)), "on only if the carrier was");
        assert_eq!((a["secrets"].clone(), b["secrets"].clone(), x["secrets"].clone()), (json!(["api_key", "webhook_secret"]), json!(["api_key"]), json!([])));
        let carrier = t.get("CarrierConnection", live).await;
        assert_eq!((carrier["api_key"].clone(), carrier["webhook_secret"].clone()), (Value::Null, Value::Null), "moved, not copied");
        // An hmac carrier has no inbound address (the platform checks no body-only signature): it is tracked by the upkeep.
        assert_eq!((a["description"]["inbound"].clone(), carrier["track_every_hours"].clone()), (Value::Null, json!(6)));
        defs.iter().map(|d| d["id"].clone()).collect::<Vec<_>>()
    };
    let ids = check().await;
    // The key it had still opens the carrier: rates, then the label, once.
    let ship =
        uid(&t.insert("CarrierShipment", json!({"company": c, "carrier": live.to_string(), "ship_to": dock.to_string(), "packages": [{"weight": "2"}]})).await);
    t.action("CarrierShipment", ship, "get_rates", json!({})).await.expect("rates");
    t.action("CarrierShipment", ship, "choose_rate", json!({"service": "Ground"})).await.expect("chosen");
    let bought = t.action("CarrierShipment", ship, "buy_label", json!({})).await.expect("bought");
    assert_eq!((bought["tracking_number"].clone(), labels.load(Ordering::SeqCst)), (json!("TRK1"), 1));
    // The test carrier calls nobody.
    let fix =
        uid(&t.insert("CarrierShipment", json!({"company": c, "carrier": test.to_string(), "ship_to": dock.to_string(), "packages": [{"weight": "2"}]})).await);
    t.action("CarrierShipment", fix, "get_rates", json!({})).await.expect("rates");
    t.action("CarrierShipment", fix, "choose_rate", json!({"service": "Test"})).await.expect("chosen");
    assert_eq!(t.action("CarrierShipment", fix, "buy_label", json!({})).await.expect("bought")["tracking_number"], json!("FIX1"));
    assert_eq!(labels.load(Ordering::SeqCst), 1);

    // The administrator applies the update, and the upkeeps run again: nothing is made twice.
    k.install_packages(tenant, &names).await.expect("the update");
    k.registry.invalidate_all();
    upkeeps(&k, &warnings, "after the update").await;
    assert_eq!(check().await, ids, "the same connections");
    // The secrets rest nowhere in the clear.
    for table in ["audit_event", "job", "conn_call", "e_carrier_connection", "record_secret"] {
        let n: Vec<(i64,)> = t
            .sql(&format!("select count(*) from {table} t where $1::uuid is not null and (t::text like '%key-123%' or t::text like '%hook-456%' or t::text like '%key-off%')"), uuid::Uuid::nil())
            .await;
        assert_eq!(n, vec![(0,)], "a carrier key rests in {table}");
    }
}
