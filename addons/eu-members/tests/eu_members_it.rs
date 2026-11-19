//! Belgium, Italy and Poland end to end against a real PostgreSQL
//! (`FERRUM_EU_MEMBERS_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_eu_members_test`; reset first), each in its own
//! workspace on the EU's add-on: Belgian business invoices queued for Peppol and the contractors'
//! reverse charge; Italian FatturaPA queued for SDI with Natura, stamp duty and SDI's receipts;
//! Polish FA(3) queued for KSeF, its number recorded and a correction citing it.

#[path = "../../../tests/common/chart.rs"]
mod chart;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::vault::{New, Rule, Scope, Vault, Zeroizing};
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, EventSink, HookRegistry, Kernel, KernelSettings, ListQuery};
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

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

/// Portugal's handle on the vault, as its registration gave it.
static PT_VAULT: std::sync::OnceLock<Vault> = std::sync::OnceLock::new();

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_EU_MEMBERS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_eu_members_test".into());
    let db = Db::connect(DbConfig { url: url.clone(), max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists vault cascade; drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    ferrum_kernel::vault::open(&db, opener(&url), false).await.expect("the vault");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_eu::packages().expect("the EU's package"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    let erp_packages: Vec<String> = ferrum_app_erp::packages().unwrap().iter().map(|p| p.name().to_string()).collect();
    hooks.app("erp", &[], ferrum_app_erp::register);
    hooks.own("erp", &erp_packages);
    ferrum_eu::register(&mut hooks);
    macro_rules! add {
        ($app:path) => {{
            use $app as app;
            packages.extend(app::packages().expect("a member state's package"));
            // As the server registers it: as itself (its id is its package's), owning its package.
            hooks.app(app::PACKAGE, &[app::PACKAGE.to_string()], |h| app::register(h));
            hooks.own(app::PACKAGE, &[app::PACKAGE.to_string()]);
            for source in app::reports() {
                ferrum_domain::reports::add_source(source);
            }
        }};
    }
    add!(ferrum_eu_members::be);
    add!(ferrum_eu_members::es);
    add!(ferrum_eu_members::fr);
    add!(ferrum_eu_members::it);
    add!(ferrum_eu_members::pl);
    // Portugal as the others, its handle kept too, to look at what it keeps in its vault.
    packages.extend(ferrum_eu_members::pt::packages().expect("Portugal's package"));
    hooks.app("pt", &["pt".to_string()], |h| {
        ferrum_eu_members::pt::register(h);
        let _ = PT_VAULT.set(h.vault());
    });
    hooks.own("pt", &["pt".to_string()]);
    add!(ferrum_eu_members::ro);
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let plugins = ferrum_plugins::PluginHost::new(false).expect("plugin host");
    hooks.register("*", Arc::new(ferrum_plugins::PluginHook(plugins)));
    // The fake services listen on a local address, which only a development server calls.
    let settings = KernelSettings { webhooks_allow_private: true, ..Default::default() };
    Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot")
}

struct T {
    k: Arc<Kernel>,
    ctx: Ctx,
    eur: Value,
    pln: Value,
    nos: Value,
    group: Value,
}

impl T {
    async fn workspace(k: &Arc<Kernel>, code: &str) -> T {
        let tenant = k
            .create_tenant(NewTenant {
                slug: code.into(),
                name: code.into(),
                admin_email: format!("admin@{code}.test"),
                admin_password: "Correct-Horse-9".into(),
                admin_name: None,
                packages: ["core", "accounting", "inventory", "selling", "buying", "eu", code].iter().map(|s| s.to_string()).collect(),
                settings: Some(json!({"fiscal_year_start_month": 1})),
            })
            .await
            .unwrap_or_else(|e| panic!("{code}: {e:?}"));
        let session = k.login(code, &format!("admin@{code}.test"), "Correct-Horse-9").await.expect("login");
        let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
        let mut t = T { k: k.clone(), ctx, eur: Value::Null, pln: Value::Null, nos: Value::Null, group: Value::Null };
        t.eur = t.first("Currency", json!({"code": "EUR"})).await["id"].clone();
        t.pln = match t.list("Currency", json!({"code": "PLN"})).await.into_iter().next() {
            Some(c) => c["id"].clone(),
            None => t.insert("Currency", json!({"code": "PLN", "currency_name": "Polish Zloty", "symbol": "zł", "precision": 2})).await["id"].clone(),
        };
        t.nos = t.first("Uom", json!({"uom_name": "Nos"})).await["id"].clone();
        t.group = t.first("ItemGroup", json!({"group_name": "Products"})).await["id"].clone();
        t
    }

    async fn list(&self, entity: &str, filters: Value) -> Vec<Doc> {
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() };
        self.k.list(&self.ctx, entity, &q).await.unwrap().rows
    }

    async fn first(&self, entity: &str, filters: Value) -> Doc {
        self.list(entity, filters).await.remove(0)
    }

    async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.k.insert(&self.ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }

    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }

    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.ctx, entity, id, false).await.unwrap()
    }

    async fn customer(&self, name: &str, more: Value, address: Value) -> Uuid {
        let mut v = obj(json!({"customer_name": name}));
        v.extend(more.as_object().cloned().unwrap_or_default());
        let c = uid(&self.insert("Customer", Value::Object(v)).await);
        let mut a = obj(json!({"address_type": "Billing", "is_primary": true, "customer": c.to_string()}));
        a.extend(address.as_object().cloned().unwrap_or_default());
        self.insert("Address", Value::Object(a)).await;
        c
    }

    async fn item(&self, code: &str, more: Value) -> Uuid {
        let mut v = obj(
            json!({"item_code": code, "item_name": code, "item_group": self.group, "stock_uom": self.nos, "is_stock_item": false, "standard_rate": "100", "eu_kind": "Services"}),
        );
        v.extend(more.as_object().cloned().unwrap_or_default());
        uid(&self.insert("Item", Value::Object(v)).await)
    }

    async fn invoice(&self, company: Uuid, customer: Uuid, item: Uuid) -> Doc {
        self.insert(
            "SalesInvoice",
            json!({"company": company.to_string(), "customer": customer.to_string(), "items": [{"item": item.to_string(), "qty": "1"}]}),
        )
        .await
    }

    async fn action(&self, id: Uuid, name: &str, params: Value) -> Value {
        self.k.run_action(&self.ctx, "SalesInvoice", id, name, params).await.unwrap_or_else(|e| panic!("{name}: {e:?}"))
    }
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
fn member_states_end_to_end() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

type Uploads = Arc<Mutex<Vec<(HashMap<String, String>, String)>>>;

/// Set: the fake ANAF takes the next upload, then never answers (the replica uploading dies first).
static HANG: Mutex<Option<Arc<tokio::sync::Notify>>> = Mutex::new(None);
/// Uploads the fake ANAF refuses with 429 before taking any.
static THROTTLE: Mutex<usize> = Mutex::new(0);

/// ANAF as far as Ferrum uses it: the token for the sign-in's code, uploads with the right token,
/// and the verdict on them.
async fn fake_anaf(uploads: Uploads) -> String {
    async fn token(Form(form): Form<HashMap<String, String>>) -> (StatusCode, Json<Value>) {
        let ok = match form.get("grant_type").map(String::as_str) {
            Some("authorization_code") => form.get("code").map(String::as_str) == Some("CODE-RO"),
            Some("refresh_token") => form.get("refresh_token").map(String::as_str) == Some("ro-refresh"),
            _ => false,
        };
        if !ok || form.get("client_id").map(String::as_str) != Some("ro-app") {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_grant"})));
        }
        (StatusCode::OK, Json(json!({"access_token": "ro-token", "refresh_token": "ro-refresh", "expires_in": 7776000, "token_type": "Bearer"})))
    }
    async fn upload(State(uploads): State<Uploads>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>, body: String) -> (StatusCode, String) {
        if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer ro-token") {
            return (StatusCode::UNAUTHORIZED, String::new());
        }
        {
            let mut throttle = THROTTLE.lock().unwrap();
            if *throttle > 0 {
                *throttle -= 1;
                return (StatusCode::TOO_MANY_REQUESTS, String::new());
            }
        }
        uploads.lock().unwrap().push((q, body));
        let died = HANG.lock().unwrap().take();
        if let Some(died) = died {
            died.notify_one();
            std::future::pending::<()>().await;
        }
        (StatusCode::OK, r#"<?xml version="1.0" encoding="UTF-8"?><header xmlns="mfp:anaf:dgti:spv:respUploadFisier:v1" dateResponse="202611011200" ExecutionStatus="0" index_incarcare="5004321"/>"#.into())
    }
    async fn status(Query(q): Query<HashMap<String, String>>) -> (StatusCode, String) {
        let ok = q.get("id_incarcare").map(String::as_str) == Some("5004321");
        (
            StatusCode::OK,
            if ok {
                r#"<header xmlns="mfp:anaf:dgti:efactura:stareMesajFactura:v1" stare="ok" id_descarcare="3001234"/>"#.into()
            } else {
                r#"<header stare="nok"/>"#.into()
            },
        )
    }
    let app = Router::new()
        .route("/anaf-oauth2/v1/token", post(token))
        .route("/FCTEL/rest/upload", post(upload))
        .route("/FCTEL/rest/stareMesaj", get(status))
        .with_state(uploads);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn scenario() {
    let uploads: Uploads = Arc::default();
    let anaf = fake_anaf(uploads.clone()).await;
    // SAFETY: set before any connector runs, in this test's own process.
    unsafe { std::env::set_var("FERRUM_RO_ANAF_URL", &anaf) };
    // SAFETY: as above; the certificate number of a throwaway certification.
    unsafe { std::env::set_var("FERRUM_PT_CERTIFICATE", "9999") };
    let k = fresh_kernel().await;
    // The producer's key, a throwaway one, in Portugal's vault as an operator sets it: revealed to Portugal alone.
    let pem = Zeroizing::new(include_str!("data/pt_test_key.pem").as_bytes().to_vec());
    let reveal = Rule { reveal: true, ..Default::default() };
    PT_VAULT.get().expect("Portugal's handle").set(&k, Scope::Installation, "signing.key", New::Value(pem), reveal).await.expect("the key");
    belgium(&k).await;
    italy(&k).await;
    poland(&k).await;
    romania(&k, &uploads).await;
    spain(&k).await;
    france(&k).await;
    portugal(&k).await;
}

async fn portugal(k: &Arc<Kernel>) {
    use rsa::pkcs8::DecodePublicKey;
    use rsa::signature::Verifier;
    let public = rsa::RsaPublicKey::from_public_key_pem(include_str!("data/pt_test_key.pub")).unwrap();
    let verify = |text: &str, hash: &str| {
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, hash).unwrap();
        let sig = rsa::pkcs1v15::Signature::try_from(bytes.as_slice()).unwrap();
        rsa::pkcs1v15::VerifyingKey::<sha1::Sha1>::new(public.clone()).verify(text.as_bytes(), &sig).is_ok()
    };
    // The Despacho's first record, signed by openssl with the same key: the same 172 characters.
    let (key, _, _) = ferrum_eu_members::pt::signing(k).await.unwrap();
    let text = ferrum_eu_members::pt::message("2010-05-18", "2010-05-18T11:22:19", "FAC 001/14", Decimal::new(312, 2), "");
    let ours = ferrum_eu_members::pt::sign(&key, &text);
    assert_eq!(ours.len(), 172);
    assert_eq!(
        ours,
        "qZ01CPPdn6Mt+hH4A7DfpWRjD94oWE72GCQPFNeWmmOn9eOYPOtxY9FBsdcebTYpjayvLyRvz2HoJB2Cmdk4b3jwzCZ4J+wNFquHY5IkflJ1mxIqBuD9OX8uUlnZcgFK+jDDWZ3ZqTk0lSuTwXNduA48EOFlW6hvM1cpY5ZxHf8="
    );

    let t = T::workspace(k, "pt").await;
    let co = t
        .insert(
            "Company",
            json!({"company_name": "Lusitana Lda", "abbr": "LL", "default_currency": t.eur, "country": "Portugal", "eu_vat_number": "PT501964843",
                   "eu_street": "Rua Augusta 1", "eu_city": "Lisboa", "eu_postal_code": "1100-048"}),
        )
        .await;
    let cid = uid(&co);
    assert!(
        t.k.insert(&t.ctx, "Customer", obj(json!({"customer_name": "NIF errado", "eu_vat_number": "PT501964844"}))).await.is_err(),
        "a NIF whose check digit does not hold"
    );
    let firm = t
        .customer(
            "Cliente SA",
            json!({"eu_vat_number": "PT509442013", "customer_type": "Company"}),
            json!({"address_line1": "Avenida da Boavista 1", "city": "Porto", "postal_code": "4100-111", "country": "Portugal"}),
        )
        .await;
    let goods = t.item("MESA", json!({"eu_kind": "Goods"})).await;
    let none = t.k.submit(&t.ctx, "SalesInvoice", uid(&t.invoice(cid, firm, goods).await), None).await;
    assert!(format!("{none:?}").contains("PT Series"), "no series communicated yet: {none:?}");
    t.insert("PtSeries", json!({"company": cid.to_string(), "document_type": "FT", "series": "2026", "validation_code": "CSDF7T5H"})).await;
    t.insert("PtSeries", json!({"company": cid.to_string(), "document_type": "NC", "series": "2026", "validation_code": "NCDF7T5H"})).await;
    let first = t.submit("SalesInvoice", uid(&t.invoice(cid, firm, goods).await)).await;
    assert_eq!(dec(&first["total_taxes"]), Decimal::from(23), "Portugal's 23%");
    assert_eq!((first["pt_document_no"].clone(), first["pt_atcud"].clone(), first["pt_status"].clone()), (json!("FT 2026/1"), json!("CSDF7T5H-1"), json!("N")));
    let h1 = first["pt_hash"].as_str().unwrap().to_string();
    let entry = first["pt_system_entry"].as_str().unwrap();
    let date = first["posting_date"].as_str().unwrap();
    assert!(verify(&format!("{date};{entry};FT 2026/1;123.00;"), &h1), "signed over the AT's fields, the first of its series");
    let second = t.submit("SalesInvoice", uid(&t.invoice(cid, firm, goods).await)).await;
    let entry2 = second["pt_system_entry"].as_str().unwrap();
    assert!(verify(&format!("{date};{entry2};FT 2026/2;123.00;{h1}"), second["pt_hash"].as_str().unwrap()), "chained to the series' previous document");
    let four = ferrum_eu_members::pt::four(&h1);
    assert_eq!(first["pt_hash_mention"], json!(format!("{four}-Processado por programa certificado n.º 9999/AT")));
    assert_eq!(
        first["pt_qr"],
        json!(format!(
            "A:501964843*B:509442013*C:PT*D:FT*E:N*F:{}*G:FT 2026/1*H:CSDF7T5H-1*I1:PT*I7:100.00*I8:23.00*N:23.00*O:123.00*Q:{four}*R:9999",
            date.replace('-', "")
        ))
    );
    // A consumer is 999999990; a credit note takes the NC series; a cancelled invoice says A.
    let person = t
        .customer(
            "Joana Silva",
            json!({"customer_type": "Individual"}),
            json!({"address_line1": "Rua do Carmo 2", "city": "Lisboa", "postal_code": "1200-093", "country": "Portugal"}),
        )
        .await;
    let si = t.submit("SalesInvoice", uid(&t.invoice(cid, person, goods).await)).await;
    assert!(si["pt_qr"].as_str().unwrap().starts_with("A:501964843*B:999999990*C:PT*D:FT"), "{:?}", si["pt_qr"]);
    let nc = t
        .insert("SalesInvoice", json!({"company": cid.to_string(), "customer": firm.to_string(), "is_return": true, "return_against": first["id"], "items": [{"item": goods.to_string(), "qty": "-1"}]}))
        .await;
    let nc = t.submit("SalesInvoice", uid(&nc)).await;
    assert_eq!((nc["pt_document_no"].clone(), nc["pt_atcud"].clone()), (json!("NC 2026/1"), json!("NCDF7T5H-1")));
    t.k.cancel(&t.ctx, "SalesInvoice", uid(&si), None).await.expect("cancel");
    let cancelled = t.get("SalesInvoice", uid(&si)).await;
    assert_eq!(cancelled["pt_status"], json!("A"));
    assert!(cancelled["pt_qr"].as_str().unwrap().contains("*E:A*"));

    // ───────────── the producer's key in Portugal's own vault ─────────────
    // Revealed to Portugal alone and sent nowhere.
    let vault = PT_VAULT.get().expect("Portugal's handle");
    let kept = vault.describe(&t.k, Scope::Installation, "signing.key").await.unwrap().expect("kept");
    assert_eq!((kept.version, kept.rule.reveal, kept.rule.hosts.len()), (1, true, 0));
    let pem = include_str!("data/pt_test_key.pem");
    let marker = pem.lines().nth(1).unwrap();
    assert_eq!(plain_anywhere(&t.k, marker).await, Vec::<String>::new(), "the key is plain in a table");
    ferrum_kernel::env::set([("FERRUM_PT_CERTIFICATE".to_string(), Some("8888".to_string())), ("FERRUM_PT_KEY_VERSION".to_string(), None)].into());
    let tt = &t;
    let next = move || async move { tt.k.submit(&tt.ctx, "SalesInvoice", uid(&tt.invoice(cid, firm, goods).await), None).await };
    let signed = next().await.expect("the vault's key signs");
    assert!(signed["pt_hash_mention"].as_str().unwrap().ends_with("n.º 8888/AT"), "the stored certificate number");
    // The vault's key, whatever it holds, is the one: a value that is not a key is refused without being quoted.
    vault.rotate(&t.k, Scope::Installation, "signing.key", New::Value(Zeroizing::new(b"not a key".to_vec()))).await.unwrap();
    let refused = format!("{:?}", next().await);
    assert!(refused.contains("not an RSA private key") && !refused.contains("not a key\""), "{refused}");
    vault.rotate(&t.k, Scope::Installation, "signing.key", New::Value(Zeroizing::new(pem.as_bytes().to_vec()))).await.unwrap();
    let from_vault = next().await.expect("the vault's key signs again");
    let previous = signed["pt_hash"].as_str().unwrap();
    let (date, entry, no) =
        (from_vault["posting_date"].as_str().unwrap(), from_vault["pt_system_entry"].as_str().unwrap(), from_vault["pt_document_no"].as_str().unwrap());
    assert!(verify(&format!("{date};{entry};{no};123.00;{previous}"), from_vault["pt_hash"].as_str().unwrap()), "signed with the vault's key, chained");
    vault.remove(&t.k, Scope::Installation, "signing.key").await.unwrap();
    let none = format!("{:?}", next().await);
    assert!(none.contains("no certification"), "without the key, plainly not set up: {none}");
    ferrum_kernel::env::set(HashMap::new());
}

/// The tables, the vault's included, any of whose rows holds `text` as it is.
async fn plain_anywhere(k: &Kernel, text: &str) -> Vec<String> {
    let mut tx = k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.vault', 'on', true)").execute(&mut *tx).await.unwrap();
    let tables: Vec<(String, String)> =
        sqlx::query_as("select schemaname::text, tablename::text from pg_tables where schemaname not in ('pg_catalog', 'information_schema')")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    assert!(tables.iter().any(|(s, _)| s == "vault"), "the vault's tables are among them");
    let mut found = vec![];
    for (schema, table) in tables {
        let sql = format!("select count(*) from \"{schema}\".\"{table}\" t where t::text like '%' || $1 || '%'");
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql)).bind(text).fetch_one(&mut *tx).await.unwrap();
        if n > 0 {
            found.push(format!("{schema}.{table}"));
        }
    }
    tx.rollback().await.unwrap();
    found
}

/// A company's account by its number.
async fn account_of(t: &T, company: Uuid, number: &str) -> Doc {
    t.first("Account", json!({"company": company.to_string(), "account_number": number})).await
}

async fn france(k: &Arc<Kernel>) {
    let t = T::workspace(k, "fr").await;
    let co = t
        .insert(
            "Company",
            json!({"company_name": "Société Exemple SA", "abbr": "SE", "default_currency": t.eur, "country": "France", "eu_vat_number": "FR44732829320",
                   "eu_street": "1 rue de Rivoli", "eu_city": "Paris", "eu_postal_code": "75001", "fr_vat_on_debits": true}),
        )
        .await;
    assert_eq!(co["fr_siren"], json!("732829320"), "from the VAT number");
    let cid = uid(&co);
    assert!(
        t.k.insert(&t.ctx, "Customer", obj(json!({"customer_name": "Mauvais", "fr_siren": "732829321"}))).await.is_err(),
        "a SIREN whose check does not hold"
    );
    let client = t
        .customer(
            "Client SARL",
            json!({"eu_vat_number": "FR13542107651", "customer_type": "Company"}),
            json!({"address_line1": "2 avenue Foch", "city": "Lyon", "postal_code": "69006", "country": "France"}),
        )
        .await;
    let goods = t.item("CHAISE", json!({"eu_kind": "Goods"})).await;
    let si = t.invoice(cid, client, goods).await;
    assert_eq!(dec(&si["total_taxes"]), Decimal::from(20), "France's 20%");
    let text = si["eu_legal_text"].as_str().unwrap().to_string();
    for part in [
        "Catégorie de l'opération : livraison de biens.",
        "Option pour le paiement de la taxe d'après les débits.",
        "indemnité forfaitaire pour frais de recouvrement de 40 €",
    ] {
        assert!(text.contains(part), "{part} in {text}");
    }
    let si = t.submit("SalesInvoice", uid(&si)).await;
    assert_eq!(t.get("SalesInvoice", uid(&si)).await["fr_pa_status"], json!("Pending"), "an e-invoice through the PA");
    let xml = t.action(uid(&si), "fr_ubl", json!({})).await["file"]["content"].as_str().unwrap().to_string();
    for part in ["<cbc:CompanyID schemeID=\"0002\">732829320</cbc:CompanyID>", "<cbc:CompanyID schemeID=\"0002\">542107651</cbc:CompanyID>"] {
        assert!(xml.contains(part), "{part} in {xml}");
    }
    t.action(uid(&si), "fr_record_pa", json!({"status": "Deposited", "pa_id": "PA-1"})).await;
    assert_eq!(t.get("SalesInvoice", uid(&si)).await["fr_pa_status"], json!("Deposited"));

    // A consumer's purchase is e-reported, not e-invoiced, and carries no business mentions.
    let person = t
        .customer(
            "Marie Martin",
            json!({"customer_type": "Individual"}),
            json!({"address_line1": "3 rue Victor Hugo", "city": "Nantes", "postal_code": "44000", "country": "France"}),
        )
        .await;
    let si = t.invoice(cid, person, goods).await;
    assert!(!si["eu_legal_text"].as_str().unwrap().contains("40 €"), "the indemnity is between businesses");
    let si = t.submit("SalesInvoice", uid(&si)).await;
    assert_eq!(t.get("SalesInvoice", uid(&si)).await["fr_pa_status"], Value::Null);
    let mut p = std::collections::HashMap::new();
    p.insert("company".to_string(), cid.to_string());
    p.insert("from_date".to_string(), "2000-01-01".to_string());
    p.insert("to_date".to_string(), "2100-12-31".to_string());
    let report = ferrum_domain::reports::run(&t.k, &t.ctx, "fr_e_reporting", &p).await.expect("e-reporting");
    let rows = report["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!((dec(&rows[0]["taxable"]), dec(&rows[0]["tax"])), (Decimal::from(100), Decimal::from(20)));

    // a_french_company_is_made_from_the_plan_comptable_and_charges_tva_on_44571: its chart is the plan's, its defaults
    // are the accounts standing for the roles, and an invoice's TVA is booked on 44571, not on an account of the standard chart.
    let plan = uid(&t.first("ChartTemplate", json!({"template_name": "France: Plan comptable général"})).await);
    let co = t
        .insert(
            "Company",
            json!({"company_name": "Société PCG SA", "abbr": "SP", "default_currency": t.eur, "country": "France", "eu_vat_number": "FR13542107651",
                   "eu_street": "1 rue de Rivoli", "eu_city": "Paris", "eu_postal_code": "75001", "chart_template": plan.to_string()}),
        )
        .await;
    let pcg = uid(&co);
    let group = account_of(&t, pcg, "4456").await;
    assert_eq!(group["is_group"], json!(true), "the plan's TVA déductible group");
    assert_eq!(account_of(&t, pcg, "44566").await["parent_account"], group["id"], "its accounts under it");
    let co = t.get("Company", pcg).await;
    assert_eq!(co["default_receivable_account"], account_of(&t, pcg, "411").await["id"], "Clients");
    assert_eq!(co["default_payable_account"], account_of(&t, pcg, "401").await["id"], "Fournisseurs");
    let output = account_of(&t, pcg, "44571").await["id"].clone();
    let client = t
        .customer(
            "Client PCG SARL",
            json!({"eu_vat_number": "FR13542107651", "customer_type": "Company"}),
            json!({"address_line1": "2 avenue Foch", "city": "Lyon", "postal_code": "69006", "country": "France"}),
        )
        .await;
    let goods = t.item("TABLE", json!({"eu_kind": "Goods"})).await;
    let si = t.invoice(pcg, client, goods).await;
    assert_eq!(dec(&si["total_taxes"]), Decimal::from(20), "France's 20% on a company made from the plan");
    let taxes = si["taxes"].as_array().expect("tax rows");
    assert!(!taxes.is_empty() && taxes.iter().all(|r| r["account"] == output), "TVA collectée on 44571: {taxes:?}");
}

async fn spain(k: &Arc<Kernel>) {
    let t = T::workspace(k, "es").await;
    let co = t
        .insert(
            "Company",
            json!({"company_name": "Iberia Comercial SL", "abbr": "IC", "default_currency": t.eur, "country": "Spain", "eu_vat_number": "ESB12345674",
                   "eu_street": "Calle Mayor 1", "eu_city": "Madrid", "eu_postal_code": "28013", "es_verifactu": true}),
        )
        .await;
    let cid = uid(&co);
    assert!(
        t.k.insert(&t.ctx, "Customer", obj(json!({"customer_name": "NIF malo", "eu_vat_number": "ES12345678A"}))).await.is_err(),
        "a NIF whose letter does not hold"
    );
    let madrid = t
        .customer(
            "Cliente SA",
            json!({"eu_vat_number": "ES12345678Z", "customer_type": "Company"}),
            json!({"address_line1": "Gran Vía 1", "city": "Madrid", "postal_code": "28013", "country": "Spain"}),
        )
        .await;
    let goods = t.item("MESA", json!({"eu_kind": "Goods"})).await;
    let first = t.submit("SalesInvoice", uid(&t.invoice(cid, madrid, goods).await)).await;
    let first = t.get("SalesInvoice", uid(&first)).await;
    assert_eq!(dec(&first["total_taxes"]), Decimal::from(21), "Spain's 21%");
    let h1 = first["es_huella"].as_str().unwrap().to_string();
    assert_eq!(
        (h1.len(), first["es_huella_anterior"].clone(), first["es_tipo_factura"].clone()),
        (64, Value::Null, json!("F1")),
        "the first record of the chain"
    );
    // The fingerprint is AEAT's, from the record's own fields.
    let expect = ferrum_eu_members::es::huella_alta(
        "B12345674",
        first["number"].as_str().unwrap(),
        &ferrum_eu_members::es::day(first["posting_date"].as_str().unwrap()),
        "F1",
        "21.00",
        "121.00",
        "",
        first["es_fecha_gen"].as_str().unwrap(),
    );
    assert_eq!(h1, expect);
    assert!(
        first["es_qr_url"].as_str().unwrap().starts_with("https://prewww2.aeat.es/wlpl/TIKE-CONT/ValidarQR?nif=B12345674&numserie="),
        "{:?}",
        first["es_qr_url"]
    );
    assert_eq!((first["es_qr_text"].clone(), first["es_verifactu_status"].clone()), (json!("VERI*FACTU"), json!("Pending")));
    let second = t.submit("SalesInvoice", uid(&t.invoice(cid, madrid, goods).await)).await;
    let second = t.get("SalesInvoice", uid(&second)).await;
    assert_eq!(second["es_huella_anterior"], json!(h1), "chained to the one before");
    t.k.cancel(&t.ctx, "SalesInvoice", uid(&second), None).await.expect("cancel");
    let cancelled = t.get("SalesInvoice", uid(&second)).await;
    let annul = cancelled["es_huella_anulacion"].as_str().unwrap().to_string();
    assert_eq!(t.get("Company", cid).await["es_last_huella"], json!(annul), "the cancellation record is the chain's last");
    t.action(uid(&first), "es_record_aeat", json!({"state": "Correcto", "csv": "A-XYZ123"})).await;
    assert_eq!(t.get("SalesInvoice", uid(&first)).await["es_verifactu_status"], json!("Accepted"));

    // A retailer under the recargo de equivalencia; goods to the Canary Islands are exports.
    let tienda = t
        .customer(
            "Tienda Pepe",
            json!({"eu_vat_number": "ESX1234567L", "es_recargo_equivalencia": true}),
            json!({"address_line1": "Calle Sol 2", "city": "Sevilla", "postal_code": "41001", "country": "Spain"}),
        )
        .await;
    let si = t.invoice(cid, tienda, goods).await;
    let recargo = si["taxes"].as_array().unwrap().iter().find(|r| r["description"] == json!("Recargo de equivalencia")).cloned().expect("the surcharge row");
    assert_eq!((dec(&recargo["tax_amount"]), dec(&si["grand_total"])), (Decimal::new(52, 1), Decimal::new(12620, 2)));
    let canarias = t
        .customer(
            "Islas SL",
            json!({"eu_vat_number": "ESB12345674", "customer_type": "Company"}),
            json!({"address_line1": "Calle Triana 3", "city": "Las Palmas", "postal_code": "35002", "country": "Spain"}),
        )
        .await;
    let si = t.invoice(cid, canarias, goods).await;
    assert_eq!((dec(&si["total_taxes"]), si["tax_breakdown"][0]["category"].clone()), (Decimal::ZERO, json!("Export")), "outside the EU's VAT area");
}

async fn romania(k: &Arc<Kernel>, uploads: &Uploads) {
    let t = T::workspace(k, "ro").await;
    let ron = match t.list("Currency", json!({"code": "RON"})).await.into_iter().next() {
        Some(c) => c["id"].clone(),
        None => t.insert("Currency", json!({"code": "RON", "currency_name": "Romanian Leu", "symbol": "lei", "precision": 2})).await["id"].clone(),
    };
    let co = t
        .insert(
            "Company",
            json!({"company_name": "Transilvania SRL", "abbr": "TS", "default_currency": ron, "country": "Romania", "eu_vat_number": "RO18547290",
                   "eu_street": "Str. Memorandumului 1", "eu_city": "Cluj-Napoca", "eu_postal_code": "400001", "ro_county": "Cluj"}),
        )
        .await;
    assert_eq!(co["ro_county"], json!("RO-CJ"));
    let cid = uid(&co);
    let bad = t.k.insert(&t.ctx, "Customer", obj(json!({"customer_name": "CIF greșit", "eu_vat_number": "RO18547291"}))).await;
    assert!(bad.is_err(), "a CIF whose check digit does not hold");
    let buyer = t
        .customer(
            "Capitala SA",
            json!({"eu_vat_number": "RO14399840", "customer_type": "Company"}),
            json!({"address_line1": "Bd. Unirii 1", "city": "Sector 3", "state": "București", "postal_code": "030119", "country": "Romania"}),
        )
        .await;
    let goods = t.item("MARFA", json!({"eu_kind": "Goods"})).await;
    let si = t.invoice(cid, buyer, goods).await;
    assert_eq!(dec(&si["total_taxes"]), Decimal::from(21), "Romania's 21% since August 2025");
    let si = t.submit("SalesInvoice", uid(&si)).await;
    assert_eq!(t.get("SalesInvoice", uid(&si)).await["ro_efactura_status"], json!("Pending"), "queued for e-Factura on submit");
    let xml = t.action(uid(&si), "ro_efactura", json!({})).await["file"]["content"].as_str().unwrap().to_string();
    for part in [
        "<cbc:CustomizationID>urn:cen.eu:en16931:2017#compliant#urn:efactura.mfinante.ro:CIUS-RO:1.0.1</cbc:CustomizationID>",
        "<cbc:CityName>Cluj-Napoca</cbc:CityName><cbc:PostalZone>400001</cbc:PostalZone><cbc:CountrySubentity>RO-CJ</cbc:CountrySubentity>",
        "<cbc:CityName>SECTOR3</cbc:CityName><cbc:PostalZone>030119</cbc:PostalZone><cbc:CountrySubentity>RO-B</cbc:CountrySubentity>",
        "<cbc:CompanyID>RO14399840</cbc:CompanyID>",
    ] {
        assert!(xml.contains(part), "{part} in {xml}");
    }
    t.action(uid(&si), "ro_record_anaf", json!({"upload_index": "5004321"})).await;
    assert_eq!(t.get("SalesInvoice", uid(&si)).await["ro_efactura_status"], json!("Uploaded"));
    t.action(uid(&si), "ro_record_anaf", json!({"state": "ok", "download_id": "3001234"})).await;
    let done = t.get("SalesInvoice", uid(&si)).await;
    assert_eq!(
        (done["ro_efactura_status"].clone(), done["ro_upload_index"].clone(), done["ro_download_id"].clone()),
        (json!("Validated"), json!("5004321"), json!("3001234"))
    );
    assert!(t.k.cancel(&t.ctx, "SalesInvoice", uid(&si), None).await.is_err(), "ANAF has it: a storno, not a cancellation");

    // Connected to ANAF: the sign-in's callback takes the tokens; uploads and verdicts follow on their own.
    let linked = t
        .insert(
            "Company",
            json!({"company_name": "Conectat SRL", "abbr": "CS", "default_currency": ron, "country": "Romania", "eu_vat_number": "RO16341004",
                   "eu_street": "Str. Lunga 2", "eu_city": "Brașov", "eu_postal_code": "500001", "ro_county": "Brașov", "ro_anaf_mode": "Test",
                   "ro_anaf_client_id": "ro-app", "ro_anaf_client_secret": "ro-secret", "ro_anaf_redirect_uri": "https://ferrum.test/api/app/ro/anaf/callback"}),
        )
        .await;
    let lid = uid(&linked);
    let started = t.k.run_action(&t.ctx, "Company", lid, "ro_connect_anaf", json!({})).await.expect("connect");
    let url = started["url"].as_str().unwrap().to_string();
    assert!(url.contains("/anaf-oauth2/v1/authorize?response_type=code&client_id=ro-app") && url.contains("token_content_type=jwt"), "{url}");
    let state = url.split("state=").nth(1).unwrap().split('&').next().unwrap().replace("%2E", ".").to_string();
    let site = ferrum_eu_members::anaf::AnafSite;
    let request = |query: Value| ferrum_kernel::SiteRequest {
        tenant: t.ctx.tenant,
        user: Some(t.ctx.clone()),
        method: "GET".into(),
        path: "/anaf/callback".into(),
        query: query.as_object().cloned().unwrap_or_default(),
        headers: Default::default(),
        body: Vec::new(),
        client_ip: "198.51.100.7".into(),
    };
    use ferrum_kernel::Site;
    // ANAF's error comes back in the address: a crafted link's text is shown as text, never run.
    let script = "<script>alert(1)</script>";
    let refused = site.handle(&t.k, request(json!({"error": script, "error_description": script}))).await.unwrap();
    let page = String::from_utf8_lossy(&refused.body);
    assert!(refused.status == 400 && !page.contains("<script") && page.contains("&lt;script&gt;alert(1)&lt;/script&gt;"), "{page}");
    let forged = site.handle(&t.k, request(json!({"code": "CODE-RO", "state": format!("{lid}.forged")}))).await.unwrap();
    assert_eq!(forged.status, 400, "a state Ferrum did not issue is refused");
    let back = site.handle(&t.k, request(json!({"code": "CODE-RO", "state": state}))).await.unwrap();
    assert_eq!(back.status, 200, "{}", String::from_utf8_lossy(&back.body));
    let si = t.submit("SalesInvoice", uid(&t.invoice(lid, buyer, goods).await)).await;
    while t.k.dispatch_outbox_once().await.expect("outbox") > 0 {}
    let done = t.get("SalesInvoice", uid(&si)).await;
    assert_eq!(
        (done["ro_efactura_status"].clone(), done["ro_upload_index"].clone(), done["ro_download_id"].clone()),
        (json!("Validated"), json!("5004321"), json!("3001234"))
    );
    let (query, xml) = uploads.lock().unwrap().first().cloned().expect("uploaded");
    assert_eq!((query.get("standard").cloned(), query.get("cif").cloned()), (Some("UBL".into()), Some("16341004".into())));
    assert!(xml.contains("CIUS-RO:1.0.1") && xml.contains("<cbc:CountrySubentity>RO-BV</cbc:CountrySubentity>"), "{xml}");

    // ───────────── an upload delivered again: ANAF gets the invoice once ─────────────
    // The upload's event as the outbox holds it; `take` marks it handled, as a replica's dispatcher takes it.
    let event = async |invoice: Uuid, take: bool| {
        let mut tx = t.k.db.system_tx().await.unwrap();
        let sql = if take {
            "update outbox set processed_at = now() where topic = $1 and record_id = $2 and processed_at is null returning id, tenant_id, payload"
        } else {
            "select id, tenant_id, payload from outbox where topic = $1 and record_id = $2"
        };
        let (id, tenant_id, payload): (i64, Uuid, Value) =
            sqlx::query_as(sql).bind(ferrum_eu_members::ro::UPLOAD_TOPIC).bind(invoice).fetch_one(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        ferrum_kernel::outbox::Event {
            id,
            tenant_id,
            topic: ferrum_eu_members::ro::UPLOAD_TOPIC.into(),
            record_id: Some(invoice),
            payload,
            ..Default::default()
        }
    };
    let count = || uploads.lock().unwrap().len();
    // Validated, then its upload again.
    let twice = uid(&t.submit("SalesInvoice", uid(&t.invoice(lid, buyer, goods).await)).await);
    while t.k.dispatch_outbox_once().await.expect("outbox") > 0 {}
    let before = count();
    ferrum_eu_members::anaf::Anaf.on_event(&t.k, &event(twice, false).await).await.expect("again");
    assert_eq!((count() - before, t.get("SalesInvoice", twice).await["ro_efactura_status"].clone()), (0, json!("Validated")));
    // ANAF takes the upload and the replica uploading dies before it hears back: not uploaded again, left to a person.
    let died = uid(&t.submit("SalesInvoice", uid(&t.invoice(lid, buyer, goods).await)).await);
    let ev = event(died, true).await;
    let before = count();
    let gone = Arc::new(tokio::sync::Notify::new());
    *HANG.lock().unwrap() = Some(gone.clone());
    tokio::select! {
        r = ferrum_eu_members::anaf::Anaf.on_event(&t.k, &ev) => panic!("ANAF never answers this one: {r:?}"),
        () = gone.notified() => {}
    }
    ferrum_eu_members::anaf::Anaf.on_event(&t.k, &ev).await.expect("the next replica");
    let doc = t.get("SalesInvoice", died).await;
    let error = doc["ro_efactura_error"].as_str().unwrap_or_default();
    assert_eq!((count() - before, doc["ro_efactura_status"].clone()), (1, json!("Pending")), "{error}");
    assert!(error.contains("may have reached ANAF"), "{error}");

    // ───────────── a person resolves an upload that may have reached ANAF ─────────────
    let act = async |inv: Uuid, action: &str, params: Value| t.k.run_action(&t.ctx, "SalesInvoice", inv, action, params).await;
    // Neither is open to an invoice ANAF has given its verdict on.
    for (action, params) in
        [("ro_record_anaf", json!({"upload_index": "5009999"})), ("ro_record_anaf", json!({"state": "nok"})), ("ro_upload_again", Value::Null)]
    {
        let refused = act(twice, action, params).await;
        assert!(refused.as_ref().is_err_and(|e| e.to_string().contains("not available")), "{action}: {refused:?}");
    }
    assert_eq!(t.get("SalesInvoice", twice).await["ro_efactura_status"], json!("Validated"));
    // ANAF's messages have it: its index, then its verdict, recorded; nothing is uploaded.
    let before = count();
    act(died, "ro_record_anaf", json!({"upload_index": "5004321"})).await.expect("its index");
    act(died, "ro_record_anaf", json!({"state": "ok", "download_id": "3001234"})).await.expect("its verdict");
    while t.k.dispatch_outbox_once().await.expect("outbox") > 0 {}
    let doc = t.get("SalesInvoice", died).await;
    assert_eq!((count() - before, doc["ro_efactura_status"].clone(), doc["ro_efactura_error"].clone()), (0, json!("Validated"), Value::Null));
    assert!(act(died, "ro_record_anaf", json!({"state": "nok"})).await.is_err(), "a verdict is not written over");
    // ANAF's messages do not have it: uploaded again, once, as a new request, and validated.
    let lost = uid(&t.submit("SalesInvoice", uid(&t.invoice(lid, buyer, goods).await)).await);
    let ev = event(lost, true).await;
    let gone = Arc::new(tokio::sync::Notify::new());
    *HANG.lock().unwrap() = Some(gone.clone());
    tokio::select! {
        r = ferrum_eu_members::anaf::Anaf.on_event(&t.k, &ev) => panic!("ANAF never answers this one: {r:?}"),
        () = gone.notified() => {}
    }
    ferrum_eu_members::anaf::Anaf.on_event(&t.k, &ev).await.expect("the next replica");
    let before = count();
    act(lost, "ro_upload_again", Value::Null).await.expect("uploaded again");
    while t.k.dispatch_outbox_once().await.expect("outbox") > 0 {}
    assert_eq!((count() - before, t.get("SalesInvoice", lost).await["ro_efactura_status"].clone()), (1, json!("Validated")));

    // ───────────── a failure that certainly took nothing is retried by itself ─────────────
    // ANAF's 429, then a connection never made: each upload is retried and validated, once, with no person.
    let anaf = std::env::var("FERRUM_RO_ANAF_URL").expect("the stand-in's address");
    for refuse in ["throttled", "refused"] {
        let inv = uid(&t.submit("SalesInvoice", uid(&t.invoice(lid, buyer, goods).await)).await);
        let ev = event(inv, true).await;
        let before = count();
        if refuse == "throttled" {
            *THROTTLE.lock().unwrap() = 1;
        } else {
            let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap();
            // SAFETY: this test's own process; set back below.
            unsafe { std::env::set_var("FERRUM_RO_ANAF_URL", format!("http://{closed}")) };
        }
        let first = ferrum_eu_members::anaf::Anaf.on_event(&t.k, &ev).await;
        unsafe { std::env::set_var("FERRUM_RO_ANAF_URL", &anaf) };
        assert!(first.is_err(), "{refuse}: tried again later");
        ferrum_eu_members::anaf::Anaf.on_event(&t.k, &ev).await.expect("the retry");
        while t.k.dispatch_outbox_once().await.expect("outbox") > 0 {}
        let doc = t.get("SalesInvoice", inv).await;
        assert_eq!((count() - before, doc["ro_efactura_status"].clone(), doc["ro_efactura_error"].clone()), (1, json!("Validated"), Value::Null), "{refuse}");
    }
}

async fn belgium(k: &Arc<Kernel>) {
    let t = T::workspace(k, "be").await;
    let co = t
        .insert(
            "Company",
            json!({"company_name": "Brussels Works SA", "abbr": "BW", "default_currency": t.eur, "country": "Belgium", "eu_vat_number": "BE0308357159",
                   "eu_street": "Rue de la Loi 1", "eu_city": "Bruxelles", "eu_postal_code": "1000", "eu_iban": "BE68539007547034"}),
        )
        .await;
    assert_eq!((co["be_enterprise_number"].clone(), co["eu_peppol_id"].clone()), (json!("0308.357.159"), json!("0208:0308357159")), "from the VAT number");
    let cid = uid(&co);
    let bad = t.k.update(&t.ctx, "Company", cid, obj(json!({"be_enterprise_number": "0308357158"})), None).await;
    assert!(matches!(bad, Err(Error::Validation(ref e)) if e.iter().any(|f| f.field == "be_enterprise_number")), "{bad:?}");
    let firm = t
        .customer(
            "Antwerp Trading NV",
            json!({"eu_vat_number": "BE0417497106", "customer_type": "Company"}),
            json!({"address_line1": "Meir 1", "city": "Antwerpen", "postal_code": "2000", "country": "Belgium"}),
        )
        .await;
    assert_eq!(t.get("Customer", firm).await["eu_peppol_id"], json!("0208:0417497106"));
    let advice = t.item("ADVICE", json!({})).await;
    let si = t.invoice(cid, firm, advice).await;
    assert_eq!(dec(&si["total_taxes"]), Decimal::from(21), "Belgium's 21%");
    let si = t.submit("SalesInvoice", uid(&si)).await;
    assert_eq!(t.get("SalesInvoice", uid(&si)).await["eu_peppol_status"], json!("Pending"), "a Belgian business's invoice goes through Peppol on submit");

    let works = t.item("RENOVATION", json!({"be_reverse_charge": true})).await;
    let si = t.invoice(cid, firm, works).await;
    assert_eq!((dec(&si["total_taxes"]), si["tax_breakdown"][0]["category"].clone()), (Decimal::ZERO, json!("Reverse Charge")));
    assert!(si["eu_legal_text"].as_str().unwrap().contains("Autoliquidation"), "{:?}", si["eu_legal_text"]);
    let person = t
        .customer(
            "Jean Dupont",
            json!({"customer_type": "Individual"}),
            json!({"address_line1": "Rue Neuve 2", "city": "Liège", "postal_code": "4000", "country": "Belgium"}),
        )
        .await;
    let si = t.invoice(cid, person, works).await;
    assert_eq!(dec(&si["total_taxes"]), Decimal::from(21), "a consumer pays the VAT");
    let si = t.submit("SalesInvoice", uid(&si)).await;
    assert_eq!(t.get("SalesInvoice", uid(&si)).await["eu_peppol_status"], Value::Null, "nothing goes through Peppol to a consumer");
}

async fn italy(k: &Arc<Kernel>) {
    let t = T::workspace(k, "it").await;
    let co = t
        .insert(
            "Company",
            json!({"company_name": "Rossi & Figli S.r.l.", "abbr": "RF", "default_currency": t.eur, "country": "Italy", "eu_vat_number": "IT01234567897",
                   "eu_street": "Via Roma 1", "eu_city": "Roma", "eu_postal_code": "00100", "eu_iban": "IT60X0542811101000000123456", "it_provincia": "rm",
                   "it_rea_ufficio": "RM", "it_rea_numero": "123456", "it_bollo_virtuale": true}),
        )
        .await;
    let cid = uid(&co);
    let milano = t
        .customer(
            "Bianchi SpA",
            json!({"eu_vat_number": "IT09876543217", "it_codice_destinatario": "abc1234", "customer_type": "Company"}),
            json!({"address_line1": "Corso Buenos Aires 1", "city": "Milano", "postal_code": "20124", "state": "MI", "country": "Italy"}),
        )
        .await;
    let goods = t.item("SEDIA", json!({"eu_kind": "Goods"})).await;
    let si = t.invoice(cid, milano, goods).await;
    assert_eq!(dec(&si["total_taxes"]), Decimal::from(22), "Italy's 22%");
    let si = t.submit("SalesInvoice", uid(&si)).await;
    let sent = t.get("SalesInvoice", uid(&si)).await;
    assert_eq!((sent["it_sdi_status"].clone(), sent["it_sdi_file"].clone()), (json!("Pending"), json!("IT01234567897_00001.xml")), "queued for SDI on submit");
    let x = t.action(uid(&si), "it_fatturapa", json!({})).await;
    let xml = x["file"]["content"].as_str().unwrap();
    for part in [
        "<IdTrasmittente><IdPaese>IT</IdPaese><IdCodice>01234567897</IdCodice></IdTrasmittente><ProgressivoInvio>00001</ProgressivoInvio>",
        "<CodiceDestinatario>ABC1234</CodiceDestinatario>",
        "<Provincia>RM</Provincia><Nazione>IT</Nazione></Sede><IscrizioneREA>",
        "<IdCodice>09876543217</IdCodice>",
        "<CAP>20124</CAP><Comune>Milano</Comune><Provincia>MI</Provincia>",
        "<AliquotaIVA>22.00</AliquotaIVA>",
        "<ImportoTotaleDocumento>122.00</ImportoTotaleDocumento>",
        "<IBAN>IT60X0542811101000000123456</IBAN>",
    ] {
        assert!(xml.contains(part), "{part} in {xml}");
    }
    let cancel = t.k.cancel(&t.ctx, "SalesInvoice", uid(&si), None).await;
    assert!(cancel.is_err(), "SDI has it: a credit note, not a cancellation");
    t.action(uid(&si), "it_record_sdi", json!({"receipt": "RC", "sdi_id": "12345678"})).await;
    assert_eq!(t.get("SalesInvoice", uid(&si)).await["it_sdi_status"], json!("Delivered"));

    // An exempt sale over €77.47 bears the stamp duty; services to a German business are N2.1.
    let course = t.item("CORSO", json!({"eu_vat_category": "Exempt", "eu_exemption_reason": "Art. 10, n. 20, D.P.R. 633/1972"})).await;
    let si = t.submit("SalesInvoice", uid(&t.invoice(cid, milano, course).await)).await;
    let xml = t.action(uid(&si), "it_fatturapa", json!({})).await["file"]["content"].as_str().unwrap().to_string();
    assert!(xml.contains("<Natura>N4</Natura>") && xml.contains("<BolloVirtuale>SI</BolloVirtuale><ImportoBollo>2.00</ImportoBollo>"), "{xml}");
    let berlin = t
        .customer(
            "Kunde GmbH",
            json!({"eu_vat_number": "DE123456789", "customer_type": "Company"}),
            json!({"address_line1": "Hauptstr. 1", "city": "Berlin", "postal_code": "10115", "country": "Germany"}),
        )
        .await;
    // VIES confirmed the number (the EU's own test asks a fake VIES).
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    t.k.set_values_tx(
        &ferrum_domain::util::sys(&t.ctx),
        &mut tx,
        "Customer",
        berlin,
        obj(json!({"eu_vies_valid": true, "eu_vies_consultation": "WAPIAAAAW1"})),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let advice = t.item("CONSULENZA", json!({})).await;
    let si = t.submit("SalesInvoice", uid(&t.invoice(cid, berlin, advice).await)).await;
    let xml = t.action(uid(&si), "it_fatturapa", json!({})).await["file"]["content"].as_str().unwrap().to_string();
    for part in
        ["<CodiceDestinatario>XXXXXXX</CodiceDestinatario>", "<IdPaese>DE</IdPaese><IdCodice>123456789</IdCodice>", "<CAP>00000</CAP>", "<Natura>N2.1</Natura>"]
    {
        assert!(xml.contains(part), "{part} in {xml}");
    }

    // a_template_company_is_still_taxed (erp-4): a company made from a national chart books VAT on the account standing
    // for output tax, and its FatturaPA finds the VAT by that role.
    let template = chart::national_template(&t.k, &t.ctx, &cid.to_string(), "Piano dei conti", &[]).await;
    let nat = t
        .insert(
            "Company",
            json!({"company_name": "Rossi Nazionale S.r.l.", "abbr": "RN", "default_currency": t.eur, "country": "Italy", "eu_vat_number": "IT01234567897",
                   "eu_street": "Via Roma 1", "eu_city": "Roma", "eu_postal_code": "00100", "eu_iban": "IT60X0542811101000000123456", "it_provincia": "rm",
                   "it_rea_ufficio": "RM", "it_rea_numero": "123456", "chart_template": template}),
        )
        .await;
    let nid = uid(&nat);
    let si = t.submit("SalesInvoice", uid(&t.invoice(nid, milano, goods).await)).await;
    let output = chart::account(&t.k, &t.ctx, &nid.to_string(), "2131").await;
    assert!(chart::posted(&t.k, &t.ctx, si["id"].as_str().unwrap()).await.contains(&(output, Decimal::from(-22))), "VAT on the account standing for 2131");
    let xml = t.action(uid(&si), "it_fatturapa", json!({})).await["file"]["content"].as_str().unwrap().to_string();
    assert!(xml.contains("<AliquotaIVA>22.00</AliquotaIVA><ImponibileImporto>100.00</ImponibileImporto><Imposta>22.00</Imposta>"), "{xml}");
}

async fn poland(k: &Arc<Kernel>) {
    let t = T::workspace(k, "pl").await;
    let co = t
        .insert(
            "Company",
            json!({"company_name": "Firma Sp. z o.o.", "abbr": "FS", "default_currency": t.pln, "country": "Poland", "eu_vat_number": "PL5265877635",
                   "eu_street": "ul. Prosta 1", "eu_city": "Warszawa", "eu_postal_code": "00-001", "eu_iban": "PL61109010140000071219812874"}),
        )
        .await;
    let cid = uid(&co);
    let bad = t.k.insert(&t.ctx, "Customer", obj(json!({"customer_name": "Zły NIP", "eu_vat_number": "PL5265877636"}))).await;
    assert!(bad.is_err(), "a NIP whose check digit does not hold");
    let buyer = t
        .customer(
            "Kupiec S.A.",
            json!({"eu_vat_number": "PL5260250995", "customer_type": "Company"}),
            json!({"address_line1": "ul. Długa 5", "city": "Kraków", "postal_code": "30-001", "country": "Poland"}),
        )
        .await;
    let goods = t.item("TOWAR", json!({"eu_kind": "Goods"})).await;
    let si = t.invoice(cid, buyer, goods).await;
    assert_eq!(dec(&si["total_taxes"]), Decimal::from(23), "Poland's 23%");
    let si = t.submit("SalesInvoice", uid(&si)).await;
    assert_eq!(t.get("SalesInvoice", uid(&si)).await["pl_ksef_status"], json!("Pending"), "queued for KSeF on submit");
    let xml = t.action(uid(&si), "pl_fa3", json!({})).await["file"]["content"].as_str().unwrap().to_string();
    for part in [
        "<NIP>5265877635</NIP>",
        "<NIP>5260250995</NIP>",
        "<KodWaluty>PLN</KodWaluty>",
        "<P_13_1>100.00</P_13_1><P_14_1>23.00</P_14_1>",
        "<P_15>123.00</P_15>",
        "<P_12>23</P_12>",
        "<RodzajFaktury>VAT</RodzajFaktury>",
    ] {
        assert!(xml.contains(part), "{part} in {xml}");
    }
    let wrong = t.k.run_action(&t.ctx, "SalesInvoice", uid(&si), "pl_record_ksef", json!({"ksef_number": "5265877635-20250826-0100001AF629-AE"})).await;
    assert!(wrong.is_err(), "a KSeF number whose check does not hold");
    t.action(uid(&si), "pl_record_ksef", json!({"ksef_number": "5265877635-20250826-0100001AF629-AF"})).await;
    let accepted = t.get("SalesInvoice", uid(&si)).await;
    assert_eq!((accepted["pl_ksef_status"].clone(), accepted["pl_ksef_number"].clone()), (json!("Accepted"), json!("5265877635-20250826-0100001AF629-AF")));

    // A correction cites the corrected invoice's KSeF number.
    let kor = t
        .insert(
            "SalesInvoice",
            json!({"company": cid.to_string(), "customer": buyer.to_string(), "is_return": true, "return_against": si["id"], "items": [{"item": goods.to_string(), "qty": "-1"}]}),
        )
        .await;
    let kor = t.submit("SalesInvoice", uid(&kor)).await;
    let xml = t.action(uid(&kor), "pl_fa3", json!({})).await["file"]["content"].as_str().unwrap().to_string();
    for part in [
        "<RodzajFaktury>KOR</RodzajFaktury>",
        "<NrKSeF>1</NrKSeF><NrKSeFFaKorygowanej>5265877635-20250826-0100001AF629-AF</NrKSeFFaKorygowanej>",
        "<P_15>-123.00</P_15>",
    ] {
        assert!(xml.contains(part), "{part} in {xml}");
    }

    // a_template_company_is_still_taxed (erp-4): a company made from a national chart books VAT on the account standing
    // for output tax, and its FA(3) finds the VAT by that role.
    let template = chart::national_template(&t.k, &t.ctx, &cid.to_string(), "Plan kont", &[]).await;
    let nat = t
        .insert(
            "Company",
            json!({"company_name": "Firma Krajowa Sp. z o.o.", "abbr": "FK", "default_currency": t.pln, "country": "Poland", "eu_vat_number": "PL5265877635",
                   "eu_street": "ul. Prosta 1", "eu_city": "Warszawa", "eu_postal_code": "00-001", "eu_iban": "PL61109010140000071219812874",
                   "chart_template": template}),
        )
        .await;
    let nid = uid(&nat);
    let si = t.submit("SalesInvoice", uid(&t.invoice(nid, buyer, goods).await)).await;
    let output = chart::account(&t.k, &t.ctx, &nid.to_string(), "2131").await;
    assert!(chart::posted(&t.k, &t.ctx, si["id"].as_str().unwrap()).await.contains(&(output, Decimal::from(-23))), "VAT on the account standing for 2131");
    let xml = t.action(uid(&si), "pl_fa3", json!({})).await["file"]["content"].as_str().unwrap().to_string();
    assert!(xml.contains("<P_13_1>100.00</P_13_1><P_14_1>23.00</P_14_1>"), "{xml}");
}
