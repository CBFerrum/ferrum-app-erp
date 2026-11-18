//! The United Kingdom end to end against a real PostgreSQL (`FERRUM_GB_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_gb_test`; reset first) and a fake
//! HMRC: VAT on sales and purchases (with the CIS reverse charge, an export and an import of
//! services), a VAT return's nine boxes, connecting through HMRC's sign-in, filing with a refreshed
//! token and the fraud prevention headers, and a refused return.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, Error, EventSink, HookRegistry, Kernel, KernelSettings, ListQuery, Site, SiteRequest};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
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

#[derive(Default)]
struct Hmrc {
    grants: Vec<String>,
    filed: Vec<(String, Value, HashMap<String, String>)>,
    /// The period key of every return posted, taken or not.
    posts: Vec<String>,
    /// Takes the next return, then never answers: the replica filing it dies before it hears back.
    hang: Option<Arc<tokio::sync::Notify>>,
    /// Posts to refuse with 429 (MESSAGE_THROTTLED_OUT) before taking any, and how often a return was looked up.
    throttle: usize,
    views: usize,
}

/// HMRC's token endpoint (code CODE-1, refresh tokens R-n) and the returns API (period DUPL is a
/// duplicate, as is a period it has a return for; a filed return can be viewed by its period key).
async fn fake_hmrc(books: Arc<Mutex<Hmrc>>) -> String {
    async fn token(State(b): State<Arc<Mutex<Hmrc>>>, Form(f): Form<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
        let grant = f.get("grant_type").cloned().unwrap_or_default();
        let ok = match grant.as_str() {
            "authorization_code" => f.get("code").map(String::as_str) == Some("CODE-1") && f.get("client_secret").map(String::as_str) == Some("app-secret"),
            "refresh_token" => f.get("refresh_token").is_some_and(|r| r.starts_with("R-")),
            _ => false,
        };
        let mut b = b.lock().unwrap();
        b.grants.push(grant);
        let n = b.grants.len();
        if !ok {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant", "error_description": "no"})));
        }
        (StatusCode::OK, Json(json!({"access_token": format!("A-{n}"), "refresh_token": format!("R-{n}"), "expires_in": 14400, "token_type": "bearer"})))
    }
    async fn file(
        State(b): State<Arc<Mutex<Hmrc>>>,
        Path(vrn): Path<String>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, HeaderMap, Json<Value>) {
        let mut out = HeaderMap::new();
        let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default();
        if !auth.starts_with("Bearer A-") || headers.get("accept").and_then(|v| v.to_str().ok()) != Some("application/vnd.hmrc.1.0+json") {
            return (StatusCode::UNAUTHORIZED, out, Json(json!({"code": "INVALID_CREDENTIALS", "message": "Invalid Authentication information provided"})));
        }
        let period = body["periodKey"].as_str().unwrap_or_default().to_string();
        let hs: HashMap<String, String> =
            headers.iter().filter(|(k, _)| k.as_str().starts_with("gov-")).map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_string())).collect();
        let died = {
            let mut books = b.lock().unwrap();
            books.posts.push(period.clone());
            if books.throttle > 0 {
                books.throttle -= 1;
                return (StatusCode::TOO_MANY_REQUESTS, out, Json(json!({"code": "MESSAGE_THROTTLED_OUT", "message": "The request for the API is throttled"})));
            }
            if period == "DUPL" || books.filed.iter().any(|(_, f, _)| f["periodKey"] == json!(period)) {
                return (
                    StatusCode::FORBIDDEN,
                    out,
                    Json(json!({"code": "DUPLICATE_SUBMISSION", "message": "The VAT return was already submitted for the given period"})),
                );
            }
            books.filed.push((vrn, body, hs));
            books.hang.take()
        };
        if let Some(died) = died {
            died.notify_one();
            std::future::pending::<()>().await;
        }
        out.insert("Receipt-ID", "2d1c8d76-8d2b-4b3e-9a4b-6a7a0d0b1c2d".parse().unwrap());
        (
            StatusCode::CREATED,
            out,
            Json(
                json!({"processingDate": "2026-10-31T10:00:00.000Z", "paymentIndicator": "BANK", "formBundleNumber": "256660290587", "chargeRefNumber": "aCxFaNx0FZsCvyWF"}),
            ),
        )
    }
    async fn view(State(b): State<Arc<Mutex<Hmrc>>>, Path((_, period)): Path<(String, String)>) -> (StatusCode, Json<Value>) {
        let mut books = b.lock().unwrap();
        books.views += 1;
        match books.filed.iter().find(|(_, f, _)| f["periodKey"] == json!(period)) {
            Some((_, f, _)) => (StatusCode::OK, Json(f.clone())),
            None => (StatusCode::NOT_FOUND, Json(json!({"code": "NOT_FOUND", "message": "The remote endpoint has indicated that no data can be found"}))),
        }
    }
    let app = Router::new()
        .route("/oauth/token", post(token))
        .route("/organisations/vat/{vrn}/returns", post(file))
        .route("/organisations/vat/{vrn}/returns/{period}", get(view))
        .with_state(books);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_GB_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_gb_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists vault cascade; drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    ferrum_kernel::vault::open(&db, opener(&url), false).await.expect("the vault");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_gb::packages().expect("the UK's package"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    let erp_packages: Vec<String> = ferrum_app_erp::packages().unwrap().iter().map(|p| p.name().to_string()).collect();
    hooks.app("erp", &[], ferrum_app_erp::register);
    hooks.own("erp", &erp_packages);
    let gb_packages: Vec<String> = ferrum_gb::packages().unwrap().iter().map(|p| p.name().to_string()).collect();
    hooks.app("gb", &gb_packages, ferrum_gb::register);
    hooks.own("gb", &gb_packages);
    let plugins = ferrum_plugins::PluginHost::new(false).expect("plugin host");
    hooks.register("*", Arc::new(ferrum_plugins::PluginHook(plugins)));
    // The server's public address (PUBLIC_URL) is what HMRC is told the server is at.
    let settings = KernelSettings { webhooks_allow_private: true, public_url: Some("https://203.0.113.9".into()), ..Default::default() };
    Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot")
}

/// What opens the test's vault: the database password of `url`, with Argon2id kept cheap for tests.
fn opener(url: &str) -> ferrum_kernel::vault::Opener {
    ferrum_kernel::vault::Opener {
        database_url: url.to_string(),
        file: "/nonexistent/root.key".into(),
        kdf: ferrum_kernel::vault::Kdf { m_kib: 64, t: 1, p: 1 },
        ..Default::default()
    }
}

#[test]
fn united_kingdom_end_to_end() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let books = Arc::new(Mutex::new(Hmrc::default()));
    let hmrc = fake_hmrc(books.clone()).await;
    // SAFETY: set before any connector runs, in this test's own process.
    unsafe { std::env::set_var("FERRUM_GB_HMRC_URL", &hmrc) };
    let k = fresh_kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "uk".into(),
            name: "Acme UK".into(),
            admin_email: "admin@acme.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying", "gb"].iter().map(|s| s.to_string()).collect(),
            settings: Some(json!({"fiscal_year_start_month": 4})),
        })
        .await
        .expect("tenant");
    let session = k.login("uk", "admin@acme.test", "Correct-Horse-9").await.expect("login");
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
    let gbp = first("Currency", json!({"code": "GBP"})).await;
    let bad = k
        .insert(
            &ctx,
            "Company",
            obj(json!({"company_name": "Bad Ltd", "abbr": "BL", "default_currency": gbp["id"], "country": "United Kingdom", "gb_vrn": "123456781"})),
        )
        .await;
    assert!(matches!(bad, Err(Error::Validation(ref e)) if e.iter().any(|f| f.field == "gb_vrn")), "{bad:?}");
    let co = insert(
        "Company",
        json!({"company_name": "Acme UK Ltd", "abbr": "AU", "default_currency": gbp["id"], "country": "United Kingdom", "gb_vrn": "GB 123 4567 82"}),
    )
    .await;
    let cid = uid(&co);
    assert_eq!(k.get(&ctx, "Company", cid, false).await.unwrap()["gb_vrn"], json!("123456782"));
    let nos = first("Uom", json!({"uom_name": "Nos"})).await;
    let group = first("ItemGroup", json!({"group_name": "Products"})).await;
    let item = |code: &'static str, more: Value| {
        let (g, u) = (group["id"].clone(), nos["id"].clone());
        async move {
            let mut v = json!({"item_code": code, "item_name": code, "item_group": g, "stock_uom": u, "is_stock_item": false, "standard_rate": "100", "gb_kind": "Goods"});
            v.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap_or_default());
            uid(&insert("Item", v).await)
        }
    };
    let widget = item("WIDGET", json!({})).await;
    let book = item("BOOK", json!({"gb_vat_category": "Zero Rated", "standard_rate": "50"})).await;
    let heating = item("HEATING", json!({"gb_vat_category": "Reduced"})).await;
    let build = item("BUILD", json!({"gb_kind": "Services", "gb_cis_reverse_charge": true, "standard_rate": "1000"})).await;
    let advice = item("ADVICE", json!({"gb_kind": "Services", "standard_rate": "500"})).await;
    let party = |entity: &'static str, name: &'static str, more: Value, country: &'static str| async move {
        let (field, link) = if entity == "Customer" { ("customer_name", "customer") } else { ("supplier_name", "supplier") };
        let mut v = more.as_object().cloned().unwrap_or_default();
        v.insert(field.into(), json!(name));
        let p = uid(&insert(entity, Value::Object(v)).await);
        let mut a = obj(json!({"address_type": "Billing", "address_line1": "1 High St", "city": "City", "country": country, "is_primary": true}));
        a.insert(link.into(), json!(p.to_string()));
        insert("Address", Value::Object(a)).await;
        p
    };
    let brit = party("Customer", "Brit Ltd", json!({"gb_vrn": "123456727"}), "United Kingdom").await;
    let builder = party("Customer", "Builder Ltd", json!({"gb_vrn": "123456727"}), "United Kingdom").await;
    let yank = party("Customer", "Yank Inc", json!({"customer_type": "Company", "tax_id": "12-3456789"}), "United States").await;
    let supplier = party("Supplier", "Supply Ltd", json!({"gb_vrn": "123456782"}), "United Kingdom").await;
    let consultant = party("Supplier", "Consult LLC", json!({}), "United States").await;
    let doc = |entity: &'static str, party: Uuid, lines: Vec<(Uuid, &'static str, &'static str)>| async move {
        let items: Vec<Value> = lines
            .iter()
            .map(
                |(i, q, rate)| {
                    if rate.is_empty() { json!({"item": i.to_string(), "qty": q}) } else { json!({"item": i.to_string(), "qty": q, "price_list_rate": rate}) }
                },
            )
            .collect();
        let mut v = obj(json!({"company": cid.to_string(), "items": items}));
        if entity == "SalesInvoice" {
            v.insert("customer".into(), json!(party.to_string()));
        } else {
            v.insert("supplier".into(), json!(party.to_string()));
            v.insert("bill_no".into(), json!(format!("B-{}", Uuid::now_v7())));
        }
        insert(entity, Value::Object(v)).await
    };

    // ───────────── sales: 20%, zero-rated and 5%; the CIS reverse charge; an export ─────────────
    let si = doc("SalesInvoice", brit, vec![(widget, "1", ""), (book, "1", ""), (heating, "1", "")]).await;
    assert_eq!((dec(&si["net_total"]), dec(&si["total_taxes"])), (n("250"), n("25")), "20 + 0 + 5");
    k.submit(&ctx, "SalesInvoice", uid(&si), None).await.expect("sale");
    let cis = doc("SalesInvoice", builder, vec![(build, "1", "")]).await;
    assert_eq!((dec(&cis["total_taxes"]), cis["tax_breakdown"][0]["category"].clone()), (n("0"), json!("Reverse Charge")));
    assert!(cis["gb_vat_notes"].as_str().unwrap().contains("section 55A"));
    k.submit(&ctx, "SalesInvoice", uid(&cis), None).await.expect("CIS sale");
    let export = doc("SalesInvoice", yank, vec![(widget, "1", "")]).await;
    assert_eq!((dec(&export["total_taxes"]), export["tax_breakdown"][0]["category"].clone()), (n("0"), json!("Export")));
    k.submit(&ctx, "SalesInvoice", uid(&export), None).await.expect("export");

    // ───────────── purchases: at home, and services from abroad reverse-charged ─────────────
    let pi = doc("PurchaseInvoice", supplier, vec![(widget, "2", "100")]).await;
    assert_eq!(dec(&pi["total_taxes"]), n("40"));
    k.submit(&ctx, "PurchaseInvoice", uid(&pi), None).await.expect("purchase");
    let import = doc("PurchaseInvoice", consultant, vec![(advice, "1", "500")]).await;
    assert_eq!(
        (dec(&import["taxes"][0]["tax_amount"]), import["taxes"][0]["reverse_charge"].clone(), dec(&import["grand_total"])),
        (n("100"), json!(true), n("500"))
    );
    k.submit(&ctx, "PurchaseInvoice", uid(&import), None).await.expect("import of services");

    // ───────────── the VAT return's nine boxes ─────────────
    let today = ctx.today();
    let period =
        |key: &'static str| json!({"company": cid.to_string(), "period_key": key, "from_date": "2000-01-01", "to_date": today.format("%Y-%m-%d").to_string()});
    let ret = insert("GbVatReturn", period("26A1")).await;
    let b: Vec<Decimal> = (1..=9).map(|i| dec(&ret[format!("box{i}").as_str()])).collect();
    // 1: 25 on sales + 100 reverse-charged; 4: 40 + 100; 6: 250 + 1000 + 100; 7: 200 + 500.
    assert_eq!(b, vec![n("125"), n("0"), n("125"), n("140"), n("15"), n("1350"), n("700"), n("0"), n("0")]);
    let undeclared = k.submit(&ctx, "GbVatReturn", uid(&ret), None).await;
    assert!(undeclared.is_err(), "HMRC's declaration first");
    k.update(&ctx, "GbVatReturn", uid(&ret), obj(json!({"finalised": true})), None).await.expect("declare");

    // ───────────── connecting to HMRC, then filing from the browser ─────────────
    k.update(
        &ctx,
        "Company",
        cid,
        obj(json!({"gb_hmrc_mode": "Sandbox", "gb_hmrc_client_id": "app-1", "gb_hmrc_client_secret": "app-secret", "gb_hmrc_redirect_uri": "https://ferrum.test/api/app/gb/hmrc/callback"})),
        None,
    )
    .await
    .expect("HMRC settings");
    let started = k.run_action(&ctx, "Company", cid, "gb_connect_hmrc", json!({})).await.expect("connect");
    let url = started["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{hmrc}/oauth/authorize?response_type=code&client_id=app-1&scope=read%3Avat%20write%3Avat")), "{url}");
    let state = url.split("state=").nth(1).unwrap().split('&').next().unwrap().replace("%2E", ".");
    let site = ferrum_gb::mtd::HmrcSite;
    let request = |method: &str, path: &str, query: Value, body: Value| SiteRequest {
        tenant,
        user: Some(ctx.clone()),
        method: method.into(),
        path: path.into(),
        query: query.as_object().cloned().unwrap_or_default(),
        headers: json!({"user-agent": "Mozilla/5.0 (Test)"}).as_object().cloned().unwrap(),
        body: serde_json::to_vec(&body).unwrap(),
        client_ip: "198.51.100.7".into(),
    };
    // HMRC's error comes back in the address: a crafted link's text is shown as text, never run.
    let script = "<script>alert(1)</script>";
    let refused = site.handle(&k, request("GET", "/hmrc/callback", json!({"error": script, "error_description": script}), Value::Null)).await.unwrap();
    let page = String::from_utf8_lossy(&refused.body);
    assert!(refused.status == 400 && !page.contains("<script") && page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"), "{page}");
    let forged = site.handle(&k, request("GET", "/hmrc/callback", json!({"code": "CODE-1", "state": format!("{cid}.forged")}), Value::Null)).await.unwrap();
    assert_eq!(forged.status, 400, "a state Ferrum did not issue is refused");
    let back = site.handle(&k, request("GET", "/hmrc/callback", json!({"code": "CODE-1", "state": state}), Value::Null)).await.unwrap();
    assert_eq!(back.status, 200, "{}", String::from_utf8_lossy(&back.body));
    assert!(k.get(&ctx, "Company", cid, false).await.unwrap()["gb_hmrc_connected_at"].is_string());
    // The access token has lapsed: filing refreshes it first.
    let mut tx = k.db.tenant_tx(tenant).await.unwrap();
    k.set_values_tx(&ctx, &mut tx, "Company", cid, obj(json!({"gb_hmrc_token_expires": "2020-01-01T00:00:00Z"}))).await.unwrap();
    tx.commit().await.unwrap();
    let browser = json!({"js_user_agent": "Mozilla/5.0 (Test)", "screens": "width=1920&height=1080&scaling-factor=1&colour-depth=24", "window_size": "width=1256&height=803",
                         "timezone": "UTC+00:00", "device_id": "beec798b-b366-47fa-b1f8-92cede14a1ce"});
    let filed = site.handle(&k, request("POST", &format!("/returns/{}/file", uid(&ret)), Value::Null, browser)).await.unwrap();
    assert_eq!(filed.status, 200);
    while k.dispatch_outbox_once().await.expect("outbox") > 0 {}
    let ret = k.get(&ctx, "GbVatReturn", uid(&ret), false).await.unwrap();
    assert_eq!(
        (ret["hmrc_status"].clone(), ret["form_bundle_number"].clone(), ret["receipt_id"].clone()),
        (json!("Filed"), json!("256660290587"), json!("2d1c8d76-8d2b-4b3e-9a4b-6a7a0d0b1c2d"))
    );
    {
        let b = books.lock().unwrap();
        assert_eq!(b.grants, vec!["authorization_code", "refresh_token"], "signed in once, refreshed once");
        let (vrn, body, headers) = &b.filed[0];
        assert_eq!(
            (vrn.as_str(), body["vatDueSales"].clone(), body["netVatDue"].clone(), body["totalValueSalesExVAT"].clone()),
            ("123456782", json!(125.0), json!(15.0), json!(1350.0))
        );
        for (h, v) in [
            ("gov-client-connection-method", "WEB_APP_VIA_SERVER"),
            ("gov-client-public-ip", "198.51.100.7"),
            ("gov-client-screens", "width=1920&height=1080&scaling-factor=1&colour-depth=24"),
            ("gov-client-user-ids", "ferrum=admin%40acme.test"),
            ("gov-client-device-id", "beec798b-b366-47fa-b1f8-92cede14a1ce"),
            ("gov-vendor-public-ip", "203.0.113.9"),
        ] {
            assert_eq!(headers.get(h).map(String::as_str), Some(v), "{h}");
        }
    }
    let cancel = k.cancel(&ctx, "GbVatReturn", uid(&ret), None).await;
    assert!(matches!(cancel, Err(Error::Invalid(ref m)) if m.contains("HMRC keeps")), "{cancel:?}");

    // ───────────── HMRC refuses a duplicate ─────────────
    let dup =
        insert("GbVatReturn", json!({"company": cid.to_string(), "period_key": "DUPL", "from_date": "2000-01-01", "to_date": "2000-03-31", "finalised": true}))
            .await;
    k.submit(&ctx, "GbVatReturn", uid(&dup), None).await.expect("submitted");
    while k.dispatch_outbox_once().await.expect("outbox") > 0 {}
    let dup = k.get(&ctx, "GbVatReturn", uid(&dup), false).await.unwrap();
    assert_eq!(dup["hmrc_status"], json!("Failed"));
    assert!(dup["hmrc_error"].as_str().unwrap().contains("DUPLICATE_SUBMISSION"), "{dup:?}");

    // ───────────── a filing delivered again: HMRC is asked to file it once ─────────────
    // The event of a return, as the outbox holds it; `take` marks it handled, as a replica's dispatcher takes it.
    let event = |rid: Uuid, take: bool| {
        let k = k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let sql = if take {
                "update outbox set processed_at = now() where topic = 'gb.mtd.submit_requested' and record_id = $1 returning id, tenant_id, payload"
            } else {
                "select id, tenant_id, payload from outbox where topic = 'gb.mtd.submit_requested' and record_id = $1"
            };
            let (id, tenant_id, payload): (i64, Uuid, Value) = sqlx::query_as(sql).bind(rid).fetch_one(&mut *tx).await.unwrap();
            tx.commit().await.unwrap();
            ferrum_kernel::outbox::Event { id, tenant_id, topic: "gb.mtd.submit_requested".into(), record_id: Some(rid), payload, ..Default::default() }
        }
    };
    let posts = |period: &str| books.lock().unwrap().posts.iter().filter(|p| *p == period).count();
    let declared =
        |key: &'static str| json!({"company": cid.to_string(), "period_key": key, "from_date": "2001-01-01", "to_date": "2001-03-31", "finalised": true});
    // Filed, then the same event again (a replica that died after the sink finished, before the event was marked).
    let twice = uid(&insert("GbVatReturn", declared("26A2")).await);
    k.submit(&ctx, "GbVatReturn", twice, None).await.expect("submitted");
    while k.dispatch_outbox_once().await.expect("outbox") > 0 {}
    ferrum_gb::mtd::Mtd.on_event(&k, &event(twice, false).await).await.expect("delivered again");
    let doc = k.get(&ctx, "GbVatReturn", twice, false).await.unwrap();
    assert_eq!((posts("26A2"), &doc["hmrc_status"], &doc["form_bundle_number"]), (1, &json!("Filed"), &json!("256660290587")), "{:?}", doc["hmrc_error"]);
    // HMRC takes the return, and the replica filing it dies before it hears back: the next asks HMRC, files nothing.
    let died = uid(&insert("GbVatReturn", declared("26A3")).await);
    k.submit(&ctx, "GbVatReturn", died, None).await.expect("submitted");
    let ev = event(died, true).await;
    let gone = Arc::new(tokio::sync::Notify::new());
    books.lock().unwrap().hang = Some(gone.clone());
    tokio::select! {
        r = ferrum_gb::mtd::Mtd.on_event(&k, &ev) => panic!("HMRC never answers this one: {r:?}"),
        () = gone.notified() => {}
    }
    ferrum_gb::mtd::Mtd.on_event(&k, &ev).await.expect("the next replica");
    let doc = k.get(&ctx, "GbVatReturn", died, false).await.unwrap();
    assert_eq!((posts("26A3"), &doc["hmrc_status"]), (1, &json!("Filed")), "{:?}", doc["hmrc_error"]);

    // ───────────── a failure that certainly filed nothing is retried by itself ─────────────
    // HMRC throttles the first post (429), and the ordinary retry files it, without asking HMRC first; then a connection
    // never made (nothing listens at the address), and the retry files it.
    let filed = |period: &str| books.lock().unwrap().filed.iter().filter(|(_, f, _)| f["periodKey"] == json!(period)).count();
    let views = || books.lock().unwrap().views;
    for (period, refuse) in [("26A4", "throttled"), ("26A5", "refused")] {
        let rid = uid(&insert("GbVatReturn", declared(period)).await);
        k.submit(&ctx, "GbVatReturn", rid, None).await.expect("submitted");
        let ev = event(rid, true).await;
        let looked = views();
        if refuse == "throttled" {
            books.lock().unwrap().throttle = 1;
        } else {
            let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
            // SAFETY: this test's own process; set back below.
            unsafe { std::env::set_var("FERRUM_GB_HMRC_URL", format!("http://{closed}")) };
        }
        let first = ferrum_gb::mtd::Mtd.on_event(&k, &ev).await;
        unsafe { std::env::set_var("FERRUM_GB_HMRC_URL", &hmrc) };
        assert!(first.is_err(), "{refuse}: tried again later");
        ferrum_gb::mtd::Mtd.on_event(&k, &ev).await.expect("the retry");
        let doc = k.get(&ctx, "GbVatReturn", rid, false).await.unwrap();
        assert_eq!(
            (filed(period), views() - looked, &doc["hmrc_status"], &doc["hmrc_error"]),
            (1, 0, &json!("Filed"), &Value::Null),
            "{refuse}: filed once, by the retry, HMRC not asked"
        );
    }
}
