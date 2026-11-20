//! The EU's rules end to end against a real PostgreSQL (`FERRUM_EU_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_eu_test`; the database is reset first), with
//! fake VIES and TEDB services: a German company selling at home, to a business and to consumers
//! in France, outside the EU and buying from France; VAT numbers checked in VIES; rates from TEDB;
//! the EC sales list and the OSS return; EN 16931 e-invoices and their Peppol hand-over.

#[path = "../../../tests/common/chart.rs"]
mod chart;

use axum::routing::post;
use axum::{Json, Router};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
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

/// How many checks the fake VIES was asked.
static VIES_ASKED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// VIES's REST API: FR12345678901 is valid (with a consultation number when asked by a requester),
/// FR99999999999 is not, anything else is refused as bad input.
async fn fake_vies() -> String {
    async fn check(Json(body): Json<Value>) -> (axum::http::StatusCode, Json<Value>) {
        VIES_ASKED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let number = format!("{}{}", body["countryCode"].as_str().unwrap_or(""), body["vatNumber"].as_str().unwrap_or(""));
        let requester = body.get("requesterNumber").and_then(Value::as_str).is_some();
        match number.as_str() {
            "FR12345678901" | "FR23456789012" => (
                axum::http::StatusCode::OK,
                Json(json!({"countryCode": "FR", "vatNumber": &number[2..], "requestDate": "2026-10-31T10:00:00.000Z", "valid": true,
                            "requestIdentifier": if requester { json!("WAPIAAAAZ1A2B3C4") } else { json!("") }, "name": "CLIENT SARL", "address": "1 RUE DE LA PAIX\n75002 PARIS"})),
            ),
            "FR99999999999" => (
                axum::http::StatusCode::OK,
                Json(
                    json!({"countryCode": "FR", "vatNumber": "99999999999", "requestDate": "2026-10-31T10:00:00.000Z", "valid": false, "requestIdentifier": "", "name": "---", "address": "---"}),
                ),
            ),
            _ => (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"actionSucceed": false, "errorWrappers": [{"error": "INVALID_INPUT", "message": "The provided Vat Number is invalid"}]})),
            ),
        }
    }
    serve(Router::new().route("/check-vat-number", post(check))).await
}

/// TEDB's VAT Retrieval Service: France's and Germany's standard rates and their reduced rates
/// for books (CN 4901), on the day asked.
async fn fake_tedb() -> String {
    async fn rates(body: String) -> ([(&'static str, &'static str); 1], String) {
        let day = body.split("<t:situationOn>").nth(1).and_then(|r| r.split('<').next()).unwrap_or("2026-10-31").to_string();
        let result = |ms: &str, kind: &str, rate_kind: &str, value: &str, cn: &str| {
            let codes =
                if cn.is_empty() { String::new() } else { format!("<cnCodes><code><value>{cn}</value><description>Books</description></code></cnCodes>") };
            format!(
                "<vatRateResults><memberState>{ms}</memberState><type>{kind}</type><rate><type>{rate_kind}</type><value>{value}</value></rate><situationOn>{day}+01:00</situationOn>{codes}<category><identifier>BOOKS</identifier><description>Books</description></category></vatRateResults>"
            )
        };
        let xml = format!(
            "<env:Envelope xmlns:env=\"http://schemas.xmlsoap.org/soap/envelope/\"><env:Body><ns0:retrieveVatRatesRespMsg xmlns=\"urn:ec.europa.eu:taxud:tedb:services:v1:IVatRetrievalService:types\" xmlns:ns0=\"urn:ec.europa.eu:taxud:tedb:services:v1:IVatRetrievalService\"><additionalInformation/>{}{}{}{}</ns0:retrieveVatRatesRespMsg></env:Body></env:Envelope>",
            result("FR", "STANDARD", "DEFAULT", "20.0", ""),
            result("FR", "REDUCED", "REDUCED_RATE", "5.5", "4901"),
            result("DE", "STANDARD", "DEFAULT", "19.0", ""),
            result("DE", "REDUCED", "REDUCED_RATE", "7.0", "4901"),
        );
        ([("content-type", "text/xml")], xml)
    }
    serve(Router::new().route("/", post(rates))).await
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_EU_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_eu_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists vault cascade; drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    ferrum_kernel::vault::open(&db, opener(&url), false).await.expect("the vault");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_eu::packages().expect("the EU's package"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    let erp_packages: Vec<String> = ferrum_app_erp::packages().unwrap().iter().map(|p| p.name().to_string()).collect();
    hooks.app("erp", &[], ferrum_app_erp::register);
    hooks.own("erp", &erp_packages);
    ferrum_eu::register(&mut hooks);
    for source in ferrum_app_erp::reports().into_iter().chain(ferrum_eu::reports()) {
        ferrum_domain::reports::add_source(source);
    }
    let plugins = ferrum_plugins::PluginHost::new(false).expect("plugin host");
    hooks.register("*", Arc::new(ferrum_plugins::PluginHook(plugins)));
    let settings = KernelSettings { webhooks_allow_private: true, ..Default::default() };
    Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot")
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
    async fn list(&self, entity: &str, filters: Value) -> Vec<Doc> {
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), limit: Some(500), ..Default::default() };
        self.k.list(&self.ctx, entity, &q).await.unwrap().rows
    }
    /// Debit minus credit on an account (by number) of a company.
    async fn gl(&self, company: Uuid, number: &str) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        sqlx::query_scalar(
            "select coalesce(sum(l.debit - l.credit), 0) from journal_line l join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
             where l.tenant_id = $1 and a.company = $2 and a.account_number = $3",
        )
        .bind(self.ctx.tenant)
        .bind(company)
        .bind(number)
        .fetch_one(&mut *tx)
        .await
        .unwrap()
    }
    async fn drain(&self) {
        while self.k.dispatch_outbox_once().await.expect("outbox") > 0 {}
    }
    async fn report(&self, name: &str, params: &[(&str, String)]) -> Value {
        let p: HashMap<String, String> = params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        ferrum_domain::reports::run(&self.k, &self.ctx, name, &p).await.unwrap_or_else(|e| panic!("report {name}: {e:?}"))
    }
}

/// A line's (category, rate) for the company's tax account `account` in an invoice's breakdown.
fn breakdown(doc: &Doc) -> Vec<(String, Decimal, Decimal, Decimal)> {
    let mut b: Vec<(String, Decimal, Decimal, Decimal)> = doc["tax_breakdown"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["category"].as_str().unwrap().to_string(), dec(&g["rate"]), dec(&g["taxable_amount"]), dec(&g["tax_amount"])))
        .collect();
    b.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    b
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
fn eu_end_to_end() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let (vies, tedb) = (fake_vies().await, fake_tedb().await);
    // SAFETY: set before any connector runs, in this test's own process.
    unsafe {
        std::env::set_var("FERRUM_EU_VIES_URL", &vies);
        std::env::set_var("FERRUM_EU_TEDB_URL", format!("{tedb}/"));
    }
    let k = fresh_kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "mg".into(),
            name: "Muster".into(),
            admin_email: "admin@mg.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying", "eu"].iter().map(|s| s.to_string()).collect(),
            settings: Some(json!({"fiscal_year_start_month": 1})),
        })
        .await
        .expect("tenant");
    let session = k.login("mg", "admin@mg.test", "Correct-Horse-9").await.expect("login");
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let t = T { k: k.clone(), ctx };
    let today = t.ctx.today().format("%Y-%m-%d").to_string();
    let period = [("from_date", "2000-01-01".to_string()), ("to_date", "2100-12-31".to_string())];

    // ───────────── a German company, its VAT accounts and the seeded rates ─────────────
    assert_eq!(t.list("EuVatRate", json!({"member_state": "FR", "kind": "Standard"})).await.len(), 1, "TEDB's standard rates come with the package");
    let eur = t.list("Currency", json!({"code": "EUR"})).await.remove(0);
    let bad =
        t.k.insert(
            &t.ctx,
            "Company",
            obj(json!({"company_name": "Bad", "abbr": "BD", "default_currency": eur["id"], "country": "Germany", "eu_vat_number": "DE12"})),
        )
        .await;
    assert!(matches!(bad, Err(Error::Validation(ref e)) if e.iter().any(|f| f.field == "eu_vat_number")), "{bad:?}");
    let co = t
        .insert(
            "Company",
            json!({"company_name": "Muster GmbH", "abbr": "MG", "default_currency": eur["id"], "country": "Deutschland", "eu_vat_number": "de 123 456 789"}),
        )
        .await;
    let cid = uid(&co);
    let co = t.get("Company", cid).await;
    assert_eq!((co["country_code"].clone(), co["eu_vat_number"].clone()), (json!("DE"), json!("DE123456789")));
    for number in ["2153", "2151", "2131", "1151"] {
        assert_eq!(t.list("Account", json!({"company": cid.to_string(), "account_number": number})).await.len(), 1, "account {number}");
    }
    let nos = t.list("Uom", json!({"uom_name": "Nos"})).await.remove(0);
    let group = t.list("ItemGroup", json!({"group_name": "Products"})).await.remove(0);
    let item = |code: &'static str, rate: &'static str, more: Value| {
        let (k, ctx, g, u) = (t.k.clone(), t.ctx.clone(), group["id"].clone(), nos["id"].clone());
        async move {
            let mut v = json!({"item_code": code, "item_name": code, "item_group": g, "stock_uom": u, "is_stock_item": false, "standard_rate": rate});
            v.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap_or_default());
            uid(&k.insert(&ctx, "Item", obj(v)).await.unwrap_or_else(|e| panic!("item {code}: {e:?}")))
        }
    };
    let advice = item("ADVICE", "100", json!({"eu_kind": "Services"})).await;
    let widget = item("WIDGET", "50", json!({"eu_kind": "Goods"})).await;
    let app = item("APP", "10", json!({"eu_kind": "Digital Services"})).await;
    let book = item(
        "BOOK",
        "20",
        json!({"eu_kind": "Goods", "eu_vat_category": "Reduced", "eu_rates": [{"member_state": "DE", "rate": "7"}, {"member_state": "FR", "rate": "5.5"}]}),
    )
    .await;
    let cover = item("COVER", "50", json!({"eu_kind": "Services", "eu_vat_category": "Exempt", "eu_exemption_reason": "VATEX-EU-132-1E"})).await;
    let customer = |name: &'static str, vat: Option<&'static str>, country: &'static str, more: Value| {
        let (k, ctx) = (t.k.clone(), t.ctx.clone());
        async move {
            let mut v = json!({"customer_name": name, "eu_vat_number": vat});
            v.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap_or_default());
            let c = uid(&k.insert(&ctx, "Customer", obj(v)).await.unwrap_or_else(|e| panic!("customer {name}: {e:?}")));
            let a = json!({"address_type": "Billing", "address_line1": "1 Main Street", "city": "City", "postal_code": "12345", "country": country, "is_primary": true, "customer": c.to_string()});
            k.insert(&ctx, "Address", obj(a)).await.unwrap_or_else(|e| panic!("address of {name}: {e:?}"));
            c
        }
    };
    let kunde = customer("Kunde AG", None, "Deutschland", json!({})).await;
    let client = customer("Client SARL", Some("FR 12 345 678 901"), "France", json!({})).await;
    let neu = customer("Neu SARL", Some("FR99999999999"), "France", json!({})).await;
    let jean = customer("Jean Dupont", None, "France", json!({"customer_type": "Individual"})).await;
    let acme = customer("Acme Inc", None, "United States", json!({"customer_type": "Company", "tax_id": "12-3456789"})).await;

    // ───────────── VIES ─────────────
    let checking = t.get("Customer", client).await;
    assert_eq!((checking["eu_vat_number"].clone(), checking["eu_vies_valid"].clone()), (json!("FR12345678901"), json!(false)), "asked, not answered yet");
    t.drain().await;
    let client_doc = t.get("Customer", client).await;
    assert_eq!(
        (client_doc["eu_vies_valid"].clone(), client_doc["eu_vies_consultation"].clone(), client_doc["eu_vies_name"].clone()),
        (json!(true), json!("WAPIAAAAZ1A2B3C4"), json!("CLIENT SARL")),
        "VIES answered for the company's VAT number"
    );
    assert_eq!(t.get("Customer", neu).await["eu_vies_valid"], json!(false));
    assert!(t.get("Customer", neu).await["eu_vies_error"].as_str().unwrap().contains("not a valid VAT number"));

    // ───────────── a sandbox asks VIES nothing ─────────────
    // A sandbox is a copy of a workspace that must reach nothing outside. The platform marks its row when it is made
    // (and never after: a trigger refuses the change, set aside here as only a test does); the mark is all the
    // refusal reads. Its check of a VAT number is refused before any address is looked up, and waits, saying why.
    let try_tenant = k
        .create_tenant(NewTenant {
            slug: "mg-try".into(),
            name: "Muster (sandbox)".into(),
            admin_email: "admin@mg.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying", "eu"].iter().map(|s| s.to_string()).collect(),
            settings: Some(json!({"fiscal_year_start_month": 1})),
        })
        .await
        .expect("a second workspace");
    {
        let mut tx = k.db.system_tx().await.unwrap();
        sqlx::raw_sql("alter table tenant disable trigger tenant_sandbox_kept").execute(&mut *tx).await.unwrap();
        sqlx::query("update tenant set sandbox_of = $2, sandbox_from = 'mg', sandbox_until = current_date + 30 where id = $1")
            .bind(try_tenant)
            .bind(tenant)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::raw_sql("alter table tenant enable trigger tenant_sandbox_kept").execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }
    let s = k.login("mg-try", "admin@mg.test", "Correct-Horse-9").await.expect("login to the sandbox");
    let sandbox = T { k: k.clone(), ctx: k.load_ctx(try_tenant, s.user, Some(s.token_version), "it".into()).await.expect("ctx") };
    let asked = VIES_ASKED.load(std::sync::atomic::Ordering::SeqCst);
    let copy = uid(&sandbox.insert("Customer", json!({"customer_name": "Client SARL", "eu_vat_number": "FR12345678901"})).await);
    t.drain().await;
    assert_eq!(VIES_ASKED.load(std::sync::atomic::Ordering::SeqCst), asked, "the sandbox's check reached VIES");
    let waiting: Vec<(Option<String>, bool)> = {
        let mut tx = k.db.system_tx().await.unwrap();
        let w = sqlx::query_as("select last_error, processed_at is not null from outbox where tenant_id = $1 and topic = 'eu.vies.requested'")
            .bind(try_tenant)
            .fetch_all(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        w
    };
    assert!(
        matches!(waiting.as_slice(), [(Some(why), false)] if why.contains(ferrum_kernel::sandbox::SANDBOX)),
        "the sandbox's check waits, saying why: {waiting:?}"
    );
    assert_eq!(sandbox.get("Customer", copy).await["eu_vies_valid"], json!(false), "and nothing came back");

    let sale = |customer: Uuid, lines: Vec<(Uuid, &'static str)>| {
        let (k, ctx) = (t.k.clone(), t.ctx.clone());
        async move {
            let items: Vec<Value> = lines.iter().map(|(i, q)| json!({"item": i.to_string(), "qty": q})).collect();
            k.insert(&ctx, "SalesInvoice", obj(json!({"company": cid.to_string(), "customer": customer.to_string(), "items": items}))).await
        }
    };

    // ───────────── at home: standard, reduced and exempt ─────────────
    let si = sale(kunde, vec![(advice, "2"), (book, "10"), (cover, "1")]).await.expect("domestic sale");
    assert_eq!((si["eu_supply"].clone(), si["eu_destination"].clone()), (json!("Domestic"), json!("DE")));
    assert_eq!((dec(&si["net_total"]), dec(&si["total_taxes"]), dec(&si["grand_total"])), (n("450"), n("52"), n("502")));
    assert_eq!(
        breakdown(&si),
        vec![("Exempt".into(), n("0"), n("50"), n("0")), ("Reduced".into(), n("7"), n("200"), n("14")), ("Standard".into(), n("19"), n("200"), n("38"))]
    );
    let si = t.submit("SalesInvoice", uid(&si)).await;
    assert_eq!(t.gl(cid, "2131").await, n("-52"));
    let ubl = t.action("SalesInvoice", uid(&si), "eu_ubl", json!({})).await;
    let xml = ubl["file"]["content"].as_str().unwrap();
    assert_eq!(ubl["peppol"], json!(false), "no Peppol IDs: plain EN 16931");
    for part in [
        "<cbc:CustomizationID>urn:cen.eu:en16931:2017</cbc:CustomizationID>",
        "<cbc:TaxAmount currencyID=\"EUR\">52.00</cbc:TaxAmount>",
        "<cbc:TaxExemptionReasonCode>VATEX-EU-132-1E</cbc:TaxExemptionReasonCode>",
        "<cbc:CompanyID>DE123456789</cbc:CompanyID>",
    ] {
        assert!(xml.contains(part), "{part} in {xml}");
    }

    // ───────────── to a business in France: no VAT, the customer's VIES check on the invoice ─────────────
    let unchecked = sale(neu, vec![(widget, "1")]).await.expect("draft");
    let refused = t.k.submit(&t.ctx, "SalesInvoice", uid(&unchecked), None).await;
    assert!(matches!(refused, Err(Error::Invalid(ref m)) if m.contains("not confirmed in VIES")), "{refused:?}");
    let b2b = sale(client, vec![(widget, "4"), (advice, "1")]).await.expect("B2B sale");
    assert_eq!(
        (b2b["eu_supply"].clone(), b2b["eu_customer_vat"].clone(), b2b["eu_vies_consultation"].clone()),
        (json!("Intra-EU B2B"), json!("FR12345678901"), json!("WAPIAAAAZ1A2B3C4"))
    );
    assert_eq!((dec(&b2b["total_taxes"]), dec(&b2b["grand_total"])), (n("0"), n("300")));
    assert_eq!(breakdown(&b2b), vec![("Intra-Community".into(), n("0"), n("200"), n("0")), ("Reverse Charge".into(), n("0"), n("100"), n("0"))]);
    let notes = b2b["eu_legal_text"].as_str().unwrap();
    assert!(notes.contains("Article 138") && notes.contains("Article 196"), "{notes}");
    let b2b = t.submit("SalesInvoice", uid(&b2b)).await;

    // ───────────── to a consumer in France: home VAT, then the destination's through the OSS ─────────────
    let home = sale(jean, vec![(widget, "2")]).await.expect("B2C sale");
    assert_eq!((home["eu_supply"].clone(), dec(&home["total_taxes"])), (json!("Intra-EU B2C (home VAT)"), n("19")));
    t.update("Company", cid, json!({"eu_distance_sales": "Destination's VAT (OSS)"})).await;
    let mixed = sale(jean, vec![(widget, "1"), (advice, "1")]).await;
    assert!(matches!(mixed, Err(Error::Invalid(ref m)) if m.contains("One-Stop Shop")), "{mixed:?}");
    let oss = sale(jean, vec![(widget, "2"), (book, "10"), (app, "1")]).await.expect("OSS sale");
    assert_eq!((oss["eu_supply"].clone(), oss["eu_destination"].clone()), (json!("Intra-EU B2C (OSS)"), json!("FR")));
    // 20% of 100 (widgets) and 10 (the app), 5.5% of 200 (books).
    assert_eq!((dec(&oss["total_taxes"]), dec(&oss["grand_total"])), (n("33"), n("343")));
    assert_eq!(breakdown(&oss), vec![("Reduced".into(), n("5.5"), n("200"), n("11")), ("Standard".into(), n("20"), n("110"), n("22"))]);
    assert!(oss["eu_legal_text"].as_str().unwrap().contains("One-Stop Shop"));
    t.submit("SalesInvoice", uid(&oss)).await;
    assert_eq!(t.gl(cid, "2153").await, n("-33"), "OSS VAT is owed apart from the home VAT");
    assert_eq!(t.gl(cid, "2150").await, n("0"), "never on the standard chart's 2150, Deferred Revenue");

    // ───────────── outside the EU ─────────────
    let export = sale(acme, vec![(widget, "2"), (advice, "1")]).await.expect("export");
    assert_eq!((export["eu_supply"].clone(), dec(&export["total_taxes"])), (json!("Export"), n("0")));
    assert_eq!(breakdown(&export), vec![("Export".into(), n("0"), n("200"), n("0"))]);
    let notes = export["eu_legal_text"].as_str().unwrap();
    assert!(notes.contains("Article 146") && notes.contains("Articles 44 and 59"), "{notes}");

    // ───────────── buying from a business in France: reverse charge, nothing extra owed ─────────────
    let fournisseur = uid(&t.insert("Supplier", json!({"supplier_name": "Fournisseur SA", "eu_vat_number": "FR23456789012"})).await);
    let pi = t
        .insert(
            "PurchaseInvoice",
            json!({"company": cid.to_string(), "supplier": fournisseur.to_string(), "bill_no": "F-1", "items": [{"item": widget.to_string(), "qty": "2", "price_list_rate": "50"}]}),
        )
        .await;
    assert_eq!((pi["eu_supply"].clone(), pi["eu_origin"].clone()), (json!("Intra-EU Acquisition"), json!("FR")));
    assert_eq!((dec(&pi["taxes"][0]["tax_amount"]), pi["taxes"][0]["reverse_charge"].clone(), dec(&pi["grand_total"])), (n("19"), json!(true), n("100")));
    let pi = t.submit("PurchaseInvoice", uid(&pi)).await;
    assert_eq!(dec(&pi["outstanding_amount"]), n("100"));
    assert_eq!((t.gl(cid, "1151").await, t.gl(cid, "2151").await), (n("19"), n("-19")));

    // ───────────── a_template_company_is_still_taxed (erp-4) ─────────────
    // A company made from a national chart that lacks the EU's own accounts: they are made under the group standing
    // for Duties and Taxes, and VAT books to the accounts standing for 2131, 1151 and 2151.
    let template = chart::national_template(&t.k, &t.ctx, &cid.to_string(), "German National", &["2153", "2151"]).await;
    let nat = t
        .insert(
            "Company",
            json!({"company_name": "Muster National GmbH", "abbr": "MN", "default_currency": eur["id"], "country": "Deutschland",
                   "eu_vat_number": "DE123456789", "chart_template": template}),
        )
        .await;
    let nid = uid(&nat).to_string();
    let oss_account = t.get("Account", chart::account(&t.k, &t.ctx, &nid, "2153").await.parse().unwrap()).await;
    let duties = chart::account(&t.k, &t.ctx, &nid, "2130").await;
    assert_eq!((oss_account["account_number"].clone(), oss_account["parent_account"].clone()), (Value::Null, json!(duties)), "made under the 2130 role");
    let si = t.insert("SalesInvoice", json!({"company": nid, "customer": kunde.to_string(), "items": [{"item": advice.to_string(), "qty": "1"}]})).await;
    t.submit("SalesInvoice", uid(&si)).await;
    let output = chart::account(&t.k, &t.ctx, &nid, "2131").await;
    assert!(chart::posted(&t.k, &t.ctx, si["id"].as_str().unwrap()).await.contains(&(output, n("-19"))), "VAT on the account standing for 2131");
    let pi = t
        .insert(
            "PurchaseInvoice",
            json!({"company": nid, "supplier": fournisseur.to_string(), "bill_no": "F-N", "items": [{"item": widget.to_string(), "qty": "2", "price_list_rate": "50"}]}),
        )
        .await;
    t.submit("PurchaseInvoice", uid(&pi)).await;
    let lines = chart::posted(&t.k, &t.ctx, pi["id"].as_str().unwrap()).await;
    let (input, reverse) = (chart::account(&t.k, &t.ctx, &nid, "1151").await, chart::account(&t.k, &t.ctx, &nid, "2151").await);
    assert!(
        lines.contains(&(input, n("19"))) && lines.contains(&(reverse, n("-19"))),
        "the reverse charge on the accounts standing for 1151 and 2151: {lines:?}"
    );

    // ───────────── TEDB's rates, and an item's suggested from its CN code ─────────────
    let before = t.list("EuVatRate", json!({})).await.len();
    t.action("Company", cid, "eu_update_rates", json!({})).await;
    t.drain().await;
    let books = t.list("EuVatRate", json!({"member_state": "FR", "kind": "Reduced"})).await;
    assert_eq!(books.len(), 1, "the reduced rate for books");
    assert_eq!((dec(&books[0]["rate"]), books[0]["cn_codes"].clone()), (n("5.5"), json!("4901")));
    assert!(t.list("EuVatRate", json!({})).await.len() > before);
    t.action("Company", cid, "eu_update_rates", json!({})).await;
    t.drain().await;
    assert_eq!(t.list("EuVatRate", json!({"member_state": "FR", "kind": "Reduced"})).await.len(), 1, "fetching again adds nothing twice");
    let novel = item("NOVEL", "15", json!({"eu_kind": "Goods", "eu_vat_category": "Reduced", "eu_cn_code": "4901 99 00"})).await;
    let suggested = t.action("Item", novel, "eu_suggest_rates", json!({})).await;
    assert!(suggested["suggested"].as_array().unwrap().len() >= 27, "{suggested}");
    let rates: HashMap<String, Decimal> =
        t.get("Item", novel).await["eu_rates"].as_array().unwrap().iter().map(|r| (r["member_state"].as_str().unwrap().to_string(), dec(&r["rate"]))).collect();
    assert_eq!((rates["FR"], rates["DE"], rates["IT"]), (n("5.5"), n("7"), n("22")), "books' reduced rates where TEDB has them, else the standard rate");

    // ───────────── the EC sales list and the OSS return ─────────────
    let ecsl = t.report("eu_ec_sales_list", &[("company", cid.to_string()), period[0].clone(), period[1].clone()]).await;
    assert_eq!(ecsl["rows"].as_array().unwrap().len(), 1);
    let row = &ecsl["rows"][0];
    assert_eq!(
        (row["country"].clone(), row["vat_number"].clone(), dec(&row["goods"]), dec(&row["services"])),
        (json!("FR"), json!("12345678901"), n("200"), n("100"))
    );
    let ossr = t.report("eu_oss_return", &[("company", cid.to_string()), period[0].clone(), period[1].clone()]).await;
    let lines: Vec<(String, Decimal, Decimal, Decimal)> = ossr["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["member_state"].as_str().unwrap().to_string(), dec(&r["rate"]), dec(&r["taxable"]), dec(&r["vat"])))
        .collect();
    assert_eq!(lines, vec![("FR".into(), n("5.5"), n("200"), n("11")), ("FR".into(), n("20"), n("110"), n("22"))]);
    assert_eq!((dec(&ossr["totals"]["taxable"]), dec(&ossr["totals"]["vat"])), (n("310"), n("33")));

    // ───────────── Peppol: the invoice goes to the access point through the outbox ─────────────
    let no_ids = t.k.run_action(&t.ctx, "SalesInvoice", uid(&b2b), "eu_send_peppol", json!({})).await;
    assert!(matches!(no_ids, Err(Error::Invalid(ref m)) if m.contains("Peppol ID")), "{no_ids:?}");
    t.update("Company", cid, json!({"eu_peppol_id": "9930:DE123456789"})).await;
    t.update("Customer", client, json!({"eu_peppol_id": "9957:FR12345678901"})).await;
    let peppol = t.action("SalesInvoice", uid(&b2b), "eu_ubl", json!({})).await;
    let xml = peppol["file"]["content"].as_str().unwrap();
    assert_eq!(peppol["peppol"], json!(true));
    for part in [
        "billing:3.0</cbc:CustomizationID>",
        "<cbc:EndpointID schemeID=\"9957\">FR12345678901</cbc:EndpointID>",
        "<cbc:ID>K</cbc:ID><cbc:Percent>0</cbc:Percent><cbc:TaxExemptionReasonCode>VATEX-EU-IC</cbc:TaxExemptionReasonCode>",
        "<cbc:ID>AE</cbc:ID>",
        "<cac:Delivery><cbc:ActualDeliveryDate>",
    ] {
        assert!(xml.contains(part), "{part} in {xml}");
    }
    assert_eq!(t.action("SalesInvoice", uid(&b2b), "eu_send_peppol", json!({})).await["status"], json!("Pending"));
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    let queued: Value = sqlx::query_scalar("select payload from outbox where tenant_id = $1 and topic = 'eu.peppol.send_requested'")
        .bind(t.ctx.tenant)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!((queued["receiver"].clone(), queued["document_type"].clone()), (json!("9957:FR12345678901"), json!("invoice")));
    assert!(queued["ubl"].as_str().unwrap().contains("<cbc:ID>SINV"));
    let cancel = t.k.cancel(&t.ctx, "SalesInvoice", uid(&b2b), None).await;
    assert!(matches!(cancel, Err(Error::Invalid(ref m)) if m.contains("credit note")), "{cancel:?}");
    t.action("SalesInvoice", uid(&b2b), "eu_record_peppol", json!({"message_id": "AP-42"})).await;
    let sent = t.get("SalesInvoice", uid(&b2b)).await;
    assert_eq!((sent["eu_peppol_status"].clone(), sent["eu_peppol_message_id"].clone()), (json!("Sent"), json!("AP-42")));
    let _ = (today, export);

    // ───────────── Intrastat (erp-17) ─────────────
    let stores = t.list("Warehouse", json!({"company": cid.to_string(), "warehouse_name": "Stores"})).await.remove(0)["id"].clone();
    let goods = |code: &'static str, rate: &'static str, more: Value| {
        item(code, rate, {
            let mut m = more;
            m["is_stock_item"] = json!(true);
            m["eu_kind"] = json!("Goods");
            m
        })
    };
    let machine = goods("MACHINE", "100", json!({"eu_cn_code": "84713000", "eu_net_mass": "2.5", "eu_origin_country": "CN"})).await;
    let wine = goods("WINE", "20", json!({"eu_cn_code": "22042110", "eu_net_mass": "1.2", "eu_supplementary_factor": "0.75"})).await;
    let cable = goods("CABLE", "5", json!({})).await;
    let bad =
        t.k.insert(
            &t.ctx,
            "Item",
            obj(json!({"item_code": "BAD", "item_name": "BAD", "item_group": group["id"], "stock_uom": nos["id"], "eu_origin_country": "cn"})),
        )
        .await;
    assert!(bad.is_err(), "an origin that is not two capital letters");
    t.submit(
        "StockEntry",
        uid(&t
            .insert(
                "StockEntry",
                json!({"company": cid.to_string(), "purpose": "Material Receipt", "items": [
                    {"item": machine.to_string(), "qty": "20", "basic_rate": "40", "target_warehouse": stores},
                    {"item": wine.to_string(), "qty": "20", "basic_rate": "5", "target_warehouse": stores},
                    {"item": cable.to_string(), "qty": "20", "basic_rate": "1", "target_warehouse": stores}]}),
            )
            .await),
    )
    .await;
    let cliente = customer("Cliente SpA", Some("IT09876543217"), "Italia", json!({"customer_type": "Company"})).await;
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    t.k.set_values_tx(
        &ferrum_domain::util::sys(&t.ctx),
        &mut tx,
        "Customer",
        cliente,
        obj(json!({"eu_vies_valid": true, "eu_vies_consultation": "WAPIAAAAW9"})),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let swiss = customer("Kunde Schweiz AG", None, "Schweiz", json!({"customer_type": "Company"})).await;
    let deliver = |customer: Uuid, country: &'static str, lines: Vec<(Uuid, &'static str)>| {
        let items: Vec<Value> = lines.iter().map(|(i, q)| json!({"item": i.to_string(), "qty": q, "warehouse": stores})).collect();
        json!({"company": cid.to_string(), "customer": customer.to_string(), "delivery_country": country, "items": items})
    };
    // dispatches_and_arrivals_add_up_by_partner_and_code
    let dn1 = t.insert("DeliveryNote", deliver(client, "FR", vec![(machine, "2")])).await;
    let dn1 = t.submit("DeliveryNote", uid(&dn1)).await;
    let dn = t.insert("DeliveryNote", deliver(cliente, "IT", vec![(wine, "1")])).await;
    t.submit("DeliveryNote", uid(&dn)).await;
    for (who, country) in [(kunde, "DE"), (swiss, "CH")] {
        let dn = t.insert("DeliveryNote", deliver(who, country, vec![(machine, "1")])).await;
        t.submit("DeliveryNote", uid(&dn)).await;
    }
    // lines_without_a_code_or_mass_are_listed_as_incomplete
    let dn = t.insert("DeliveryNote", deliver(client, "FR", vec![(cable, "3")])).await;
    t.submit("DeliveryNote", uid(&dn)).await;
    // services_are_left_out: a plain invoice of advice to France (no goods line).
    // an_invoice_that_updates_stock_counts_and_a_plain_invoice_does_not
    let mut si = deliver(client, "FR", vec![(machine, "1")]);
    si["update_stock"] = json!(true);
    let si = t.insert("SalesInvoice", si).await;
    t.submit("SalesInvoice", uid(&si)).await;
    let plain = t.insert("SalesInvoice", json!({"company": cid.to_string(), "customer": client.to_string(), "delivery_country": "FR", "items": [{"item": machine.to_string(), "qty": "1"}, {"item": advice.to_string(), "qty": "1"}]})).await;
    t.submit("SalesInvoice", uid(&plain)).await;
    let polski = uid(&t.insert("Supplier", json!({"supplier_name": "Dostawca Sp. z o.o.", "eu_vat_number": "PL5260250995"})).await);
    let warsaw = uid(&t
        .insert("Address", json!({"address_type": "Billing", "address_line1": "ul. Prosta 1", "city": "Warszawa", "postal_code": "00-001", "country": "Polska", "is_primary": true, "supplier": polski.to_string()}))
        .await);
    let pr = t
        .insert(
            "PurchaseReceipt",
            json!({"company": cid.to_string(), "supplier": polski.to_string(), "supplier_address": warsaw.to_string(), "items": [{"item": machine.to_string(), "qty": "4", "price_list_rate": "50", "warehouse": stores}]}),
        )
        .await;
    t.submit("PurchaseReceipt", uid(&pr)).await;
    // a_return_is_the_other_flow: one machine back from France, nature 21.
    let back = t.action("DeliveryNote", uid(&dn1), "make_sales_return", json!({})).await;
    let back = t.get("DeliveryNote", back["id"].as_str().unwrap().parse().unwrap()).await;
    t.k.update(
        &t.ctx,
        "DeliveryNote",
        uid(&back),
        obj(json!({"delivery_country": "FR", "items": [{"id": back["items"][0]["id"], "item": machine.to_string(), "qty": "1", "warehouse": stores}]})),
        None,
    )
    .await
    .expect("one back");
    t.submit("DeliveryNote", uid(&back)).await;
    let intrastat = |flow: &'static str| {
        let (k, ctx) = (t.k.clone(), t.ctx.clone());
        async move {
            let p: HashMap<String, String> =
                [("company", cid.to_string()), ("from_date", "2000-01-01".to_string()), ("to_date", "2100-12-31".to_string()), ("flow", flow.to_string())]
                    .into_iter()
                    .map(|(a, b)| (a.to_string(), b))
                    .collect();
            ferrum_domain::reports::run(&k, &ctx, "eu_intrastat", &p).await
        }
    };
    let all = intrastat("").await.expect("intrastat");
    let rows: Vec<(String, String, String, String, String, String, Decimal, Decimal, Option<Decimal>)> = all["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let txt = |k: &str| r[k].as_str().unwrap_or_default().to_string();
            (
                txt("flow"),
                txt("partner"),
                txt("cn_code"),
                txt("origin"),
                txt("nature"),
                txt("partner_vat"),
                dec(&r["value"]),
                dec(&r["net_mass"]),
                r["supplementary"].as_str().map(n),
            )
        })
        .collect();
    assert_eq!(
        rows,
        vec![
            ("Arrivals".into(), "FR".into(), "84713000".into(), "CN".into(), "21".into(), "".into(), n("100"), n("2.5"), None),
            ("Arrivals".into(), "PL".into(), "84713000".into(), "CN".into(), "11".into(), "".into(), n("200"), n("10"), None),
            ("Dispatches".into(), "FR".into(), "84713000".into(), "CN".into(), "11".into(), "FR12345678901".into(), n("300"), n("7.5"), None),
            ("Dispatches".into(), "IT".into(), "22042110".into(), "DE".into(), "11".into(), "IT09876543217".into(), n("20"), n("1.2"), Some(n("0.75"))),
        ],
        "{all}"
    );
    let incomplete = all["incomplete"].as_array().unwrap();
    assert_eq!(incomplete.len(), 1, "{incomplete:?}");
    assert!(incomplete[0]["missing"].as_str().unwrap().contains("commodity code"));
    assert_eq!(intrastat("Arrivals").await.unwrap()["rows"].as_array().unwrap().len(), 2, "one flow");
    // the_report_needs_read_on_delivery_notes
    let user = t.k.create_user(&t.ctx, "vat@mg.test", "VAT", Some("Correct-Horse-9"), &["Accounts User".into()], false).await.expect("user");
    let clerk = t.k.load_ctx(t.ctx.tenant, user, None, "it".into()).await.unwrap();
    let p: HashMap<String, String> = [("company", cid.to_string())].into_iter().map(|(a, b)| (a.to_string(), b)).collect();
    assert!(ferrum_domain::reports::run(&t.k, &clerk, "eu_intrastat", &p).await.is_err(), "no read on delivery notes");
    // a_company_outside_the_union_is_refused
    let chf = t.insert("Currency", json!({"code": "CHF", "currency_name": "Swiss Franc", "symbol": "CHF", "precision": 2})).await;
    let ch = t.insert("Company", json!({"company_name": "Swiss AG", "abbr": "SAG", "default_currency": chf["id"], "country": "Switzerland"})).await;
    let p: HashMap<String, String> = [("company", uid(&ch).to_string())].into_iter().map(|(a, b)| (a.to_string(), b)).collect();
    let e = format!("{:?}", ferrum_domain::reports::run(&t.k, &t.ctx, "eu_intrastat", &p).await.unwrap_err());
    assert!(e.contains("EU member state"), "{e}");
}
