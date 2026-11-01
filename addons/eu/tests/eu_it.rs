//! The EU's rules end to end against a real PostgreSQL (`FERRUM_EU_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_eu_test`; the database is reset first), with
//! fake VIES and TEDB services: a German company selling at home, to a business and to consumers
//! in France, outside the EU and buying from France; VAT numbers checked in VIES; rates from TEDB;
//! the EC sales list and the OSS return; EN 16931 e-invoices and their Peppol hand-over.

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

/// VIES's REST API: FR12345678901 is valid (with a consultation number when asked by a requester),
/// FR99999999999 is not, anything else is refused as bad input.
async fn fake_vies() -> String {
    async fn check(Json(body): Json<Value>) -> (axum::http::StatusCode, Json<Value>) {
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
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_eu::packages().expect("the EU's package"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_eu::register(&mut hooks);
    for source in ferrum_app_erp::reports().into_iter().chain(ferrum_eu::reports()) {
        ferrum_domain::reports::add_source(source);
    }
    let plugins = ferrum_plugins::PluginHost::new(false).expect("plugin host");
    hooks.register("*", Arc::new(ferrum_plugins::PluginHook(plugins)));
    let settings = KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([5u8; 32])), webhooks_allow_private: true, ..Default::default() };
    let k = Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    k
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
    for number in ["2150", "2151", "2131", "1151"] {
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
    assert_eq!(t.gl(cid, "2150").await, n("-33"), "OSS VAT is owed apart from the home VAT");

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
}
