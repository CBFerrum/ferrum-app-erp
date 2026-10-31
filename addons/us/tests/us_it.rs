//! The United States end to end against a real PostgreSQL (`FERRUM_US_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_us_test`; reset first) and a fake AvaTax: a Texas
//! company registered in Texas, California and New York selling across states, abroad, to a
//! reseller and through a marketplace; its rate tables, use tax, AvaTax records and voids, and the
//! sales tax, economic nexus and Form 1099 reports.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings, ListQuery};
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

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_US_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_us_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_us::packages().expect("the US's package"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_us::register(&mut hooks);
    for source in ferrum_us::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let plugins = ferrum_plugins::PluginHost::new(false).expect("plugin host");
    hooks.register("*", Arc::new(ferrum_plugins::PluginHook(plugins)));
    let settings = KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([7u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    k
}

type Calls = Arc<Mutex<Vec<(String, Value)>>>;

/// AvaTax as far as Ferrum uses it: transactions recorded (its own tax one dollar more for the
/// gadget) and voided, behind the company's credentials.
async fn fake_avatax(calls: Calls) -> String {
    async fn record(State(calls): State<Calls>, headers: HeaderMap, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
        if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Basic MTEwMDpMSUNFTlNF") {
            return (StatusCode::UNAUTHORIZED, Json(json!({"error": {"code": "AuthenticationException", "message": "Authentication failed."}})));
        }
        calls.lock().unwrap().push(("record".into(), body.clone()));
        let m = &body["createTransactionModel"];
        let ours = m["taxOverride"]["taxAmount"].as_f64().unwrap_or_default();
        let gadget = m["lines"].as_array().unwrap().iter().any(|l| l["itemCode"] == json!("GADGET"));
        let theirs = if gadget { ours + 1.0 } else { ours };
        (StatusCode::CREATED, Json(json!({"id": 1, "code": m["code"], "status": "Committed", "totalTax": ours, "totalTaxCalculated": theirs})))
    }
    async fn void(State(calls): State<Calls>, Path((company, code)): Path<(String, String)>, Json(body): Json<Value>) -> (StatusCode, Json<Value>) {
        calls.lock().unwrap().push(("void".into(), json!({"company": company, "code": code, "body": body})));
        (StatusCode::OK, Json(json!({"code": code, "status": "Cancelled"})))
    }
    let app = Router::new()
        .route("/api/v2/transactions/createoradjust", post(record))
        .route("/api/v2/companies/{company}/transactions/{code}/void", post(void))
        .with_state(calls);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[test]
fn united_states_end_to_end() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let calls: Calls = Arc::default();
    let avatax = fake_avatax(calls.clone()).await;
    // SAFETY: set before any connector runs, in this test's own process.
    unsafe { std::env::set_var("FERRUM_US_AVATAX_URL", &avatax) };
    let k = fresh_kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "us".into(),
            name: "Lone Star".into(),
            admin_email: "admin@lonestar.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying", "us"].iter().map(|s| s.to_string()).collect(),
            settings: Some(json!({"fiscal_year_start_month": 1})),
        })
        .await
        .expect("tenant");
    let session = k.login("us", "admin@lonestar.test", "Correct-Horse-9").await.expect("login");
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let first = async |entity: &str, filters: Value| -> Doc {
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() };
        k.list(&ctx, entity, &q).await.unwrap().rows.remove(0)
    };
    let insert = async |entity: &str, v: Value| -> Doc { k.insert(&ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}")) };
    let usd = first("Currency", json!({"code": "USD"})).await;
    let co = insert(
        "Company",
        json!({"company_name": "Lone Star Supply", "abbr": "LSS", "default_currency": usd["id"], "country": "United States", "us_state": "TX", "us_zip": "78701-1234",
               "us_registered_states": "TX, CA, ny", "us_avatax_mode": "Sandbox", "us_avatax_account_id": "1100", "us_avatax_license_key": "LICENSE",
               "us_avatax_company_code": "LSS"}),
    )
    .await;
    let cid = uid(&co);
    assert_eq!((co["us_state"].clone(), co["country_code"].clone()), (json!("TX"), json!("US")));
    let account = |number: &'static str| {
        let k = k.clone();
        let ctx = ctx.clone();
        async move {
            let q = ListQuery {
                filters: ferrum_kernel::query::parse_filters(&json!({"company": cid.to_string(), "account_number": number})).unwrap(),
                ..Default::default()
            };
            k.list(&ctx, "Account", &q).await.unwrap().rows.remove(0)
        }
    };
    assert_eq!(account("2171").await["account_name"], json!("Use Tax Payable"));
    assert_eq!(account("5290").await["account_name"], json!("Use Tax Expense"));

    // The states' rate tables, as Avalara publishes them.
    let header = "State,ZipCode,TaxRegionName,EstimatedCombinedRate,StateRate,EstimatedCountyRate,EstimatedCityRate,EstimatedSpecialRate,RiskLevel";
    for (state, lines) in [
        ("CA", "CA,90001,LOS ANGELES COUNTY,0.097500,0.060000,0.002500,0.000000,0.035000,1"),
        ("TX", "TX,78701,AUSTIN,0.082500,0.062500,0.000000,0.010000,0.010000,1\nTX,75201,DALLAS,0.080000,0.062500,0.000000,0.010000,0.007500,1"),
    ] {
        let t = insert("UsRateTable", json!({"state": state, "valid_from": "2026-01-01", "source": "test", "csv": format!("{header}\n{lines}\n")})).await;
        let t = k.submit(&ctx, "UsRateTable", uid(&t), None).await.expect("rate table");
        assert!(t["zip_count"].as_i64().unwrap() >= 1);
    }
    let la_rate = first("UsTaxRate", json!({"zip": "90001"})).await;
    assert_eq!(dec(&la_rate["combined_rate"]), n("9.75"));
    let bad = k
        .insert(&ctx, "UsRateTable", obj(json!({"state": "NV", "valid_from": "2026-01-01", "csv": format!("{header}\nCA,90001,X,0.07,0.07,0,0,0,1\n")})))
        .await
        .unwrap();
    let err = k.submit(&ctx, "UsRateTable", uid(&bad), None).await.expect_err("another state's rows are refused");
    assert!(format!("{err:?}").contains("line 2: the state is CA, not NV"), "{err:?}");

    let nos = first("Uom", json!({"uom_name": "Nos"})).await;
    let group = first("ItemGroup", json!({"group_name": "Products"})).await;
    let item = async |code: &str, more: Value| -> Uuid {
        let mut v = obj(
            json!({"item_code": code, "item_name": code, "item_group": group["id"], "stock_uom": nos["id"], "is_stock_item": false, "standard_rate": "100"}),
        );
        v.extend(more.as_object().cloned().unwrap_or_default());
        uid(&insert("Item", Value::Object(v)).await)
    };
    let widget = item("WIDGET", json!({})).await;
    let gadget = item("GADGET", json!({})).await;
    let bread = item("BREAD", json!({"us_tax_code": "pf050001"})).await;
    let app = item("APP", json!({"us_tax_code": "NT"})).await;
    insert("UsTaxability", json!({"state": "Texas", "tax_code": "PF050001", "taxable": false, "note": "Food products for home consumption"})).await;

    let customer = async |name: &str, more: Value, address: Value| -> Uuid {
        let mut v = obj(json!({"customer_name": name}));
        v.extend(more.as_object().cloned().unwrap_or_default());
        let c = uid(&insert("Customer", Value::Object(v)).await);
        let mut a = obj(json!({"address_type": "Billing", "address_line1": "1 Main St", "is_primary": true, "customer": c.to_string()}));
        a.extend(address.as_object().cloned().unwrap_or_default());
        insert("Address", Value::Object(a)).await;
        c
    };
    let sale = async |customer: Uuid, item: Uuid, qty: &str| -> Doc {
        insert("SalesInvoice", json!({"company": cid.to_string(), "customer": customer.to_string(), "items": [{"item": item.to_string(), "qty": qty}]})).await
    };
    let taxed = |doc: &Doc| -> (Decimal, Value, Value) {
        let b = &doc["tax_breakdown"][0];
        (dec(&doc["total_taxes"]), b["category"].clone(), b["exemption_reason"].clone())
    };

    let la = customer("Angel Co", json!({}), json!({"city": "Los Angeles", "state": "California", "postal_code": "90001", "country": "United States"})).await;
    let si = sale(la, widget, "1").await;
    assert_eq!(taxed(&si), (n("9.75"), json!("Standard"), Value::Null), "destination: the ZIP code's combined rate");
    assert_eq!((si["us_state"].clone(), si["us_jurisdiction"].clone(), dec(&si["us_rate"])), (json!("CA"), json!("LOS ANGELES COUNTY"), n("9.75")));
    let la_invoice = k.submit(&ctx, "SalesInvoice", uid(&si), None).await.expect("submit");
    assert_eq!(la_invoice["us_avatax_status"], json!("Pending"));

    let dallas = customer("Big D", json!({}), json!({"city": "Dallas", "state": "TX", "postal_code": "75201", "country": "United States"})).await;
    let si = sale(dallas, widget, "1").await;
    assert_eq!(taxed(&si).0, n("8.25"), "within Texas, origin: Austin's rate, not Dallas's 8%");
    k.submit(&ctx, "SalesInvoice", uid(&si), None).await.expect("submit");
    let si = sale(dallas, bread, "1").await;
    assert_eq!(taxed(&si), (n("0"), json!("Exempt"), json!("Not taxable in Texas (PF050001)")));
    k.submit(&ctx, "SalesInvoice", uid(&si), None).await.expect("submit");
    let si = sale(la, app, "1").await;
    assert_eq!(taxed(&si), (n("0"), json!("Exempt"), json!("Not taxable (NT)")));
    k.submit(&ctx, "SalesInvoice", uid(&si), None).await.expect("submit");

    let reno = customer("Reno Retail", json!({}), json!({"city": "Reno", "state": "NV", "postal_code": "89501", "country": "United States"})).await;
    let si = sale(reno, widget, "1000").await;
    assert_eq!(taxed(&si), (n("0"), json!("Out of Scope"), json!("Not registered for sales tax in Nevada")));
    k.submit(&ctx, "SalesInvoice", uid(&si), None).await.expect("submit");

    let reseller = customer("Resale Co", json!({}), json!({"city": "Los Angeles", "state": "CA", "postal_code": "90001", "country": "United States"})).await;
    insert(
        "UsExemptionCertificate",
        json!({"customer": reseller.to_string(), "states": "California, NV", "reason": "Resale", "certificate_number": "SR-1", "valid_from": "2026-01-01"}),
    )
    .await;
    let si = sale(reseller, widget, "1").await;
    assert_eq!(taxed(&si), (n("0"), json!("Exempt"), json!("Exemption certificate SR-1 (Resale)")));
    k.submit(&ctx, "SalesInvoice", uid(&si), None).await.expect("submit");

    let amazon = customer(
        "Marketplace",
        json!({"us_marketplace_facilitator": true}),
        json!({"city": "Seattle", "state": "CA", "postal_code": "90001", "country": "United States"}),
    )
    .await;
    assert_eq!(taxed(&sale(amazon, widget, "1").await).2, json!("Collected by the marketplace facilitator"));
    let toronto = customer("Maple", json!({}), json!({"city": "Toronto", "state": "ON", "postal_code": "M5V", "country": "Canada"})).await;
    assert_eq!(taxed(&sale(toronto, widget, "1").await).1, json!("Export"));

    // New York's local taxes need its ZIP code's rate before an invoice goes out.
    let nyc = customer("Gotham", json!({}), json!({"city": "New York", "state": "NY", "postal_code": "10001", "country": "United States"})).await;
    let si = sale(nyc, gadget, "1").await;
    let err = k.submit(&ctx, "SalesInvoice", uid(&si), None).await.expect_err("no rate for 10001");
    assert!(format!("{err:?}").contains("no rate for ZIP code 10001"), "{err:?}");
    insert("UsTaxRate", json!({"state": "NY", "zip": "10001", "region": "NEW YORK CITY", "state_rate": "4", "city_rate": "4.5", "special_rate": "0.375", "valid_from": "2026-01-01"})).await;
    let si = k.update(&ctx, "SalesInvoice", uid(&si), obj(json!({"remarks": "rate added"})), None).await.expect("recalculate");
    assert_eq!(taxed(&si).0, n("8.88"), "8.875% of 100");
    let ny_invoice = k.submit(&ctx, "SalesInvoice", uid(&si), None).await.expect("submit");

    // AvaTax records each invoice with the tax it charged; the gadget's differs there.
    while k.dispatch_outbox_once().await.expect("outbox") > 0 {}
    let la_invoice = k.get(&ctx, "SalesInvoice", uid(&la_invoice), false).await.unwrap();
    assert_eq!((la_invoice["us_avatax_status"].clone(), la_invoice["us_avatax_error"].clone()), (json!("Committed"), Value::Null));
    let ny_invoice = k.get(&ctx, "SalesInvoice", uid(&ny_invoice), false).await.unwrap();
    assert_eq!(ny_invoice["us_avatax_status"], json!("Differs"));
    assert_eq!(dec(&ny_invoice["us_avatax_tax"]), n("9.88"));
    let recorded = calls.lock().unwrap().clone();
    let la_call =
        recorded.iter().find(|(kind, b)| kind == "record" && b["createTransactionModel"]["code"] == la_invoice["number"]).expect("recorded").1.clone();
    let m = &la_call["createTransactionModel"];
    assert_eq!(
        (m["type"].clone(), m["commit"].clone(), m["taxOverride"]["type"].clone(), m["taxOverride"]["taxAmount"].clone(), m["lines"][0]["taxCode"].clone()),
        (json!("SalesInvoice"), json!(true), json!("TaxAmount"), json!(9.75), json!("P0000000"))
    );
    assert_eq!((m["addresses"]["shipTo"]["postalCode"].clone(), m["addresses"]["shipFrom"]["postalCode"].clone()), (json!("90001"), json!("78701")));
    let resale_call =
        recorded.iter().find(|(kind, b)| kind == "record" && b["createTransactionModel"]["exemptionNo"] == json!("SR-1")).expect("the reseller's");
    assert_eq!(resale_call.1["createTransactionModel"]["entityUseCode"], json!("G"));

    // Cancelling voids it there.
    k.cancel(&ctx, "SalesInvoice", uid(&la_invoice), None).await.expect("cancel");
    while k.dispatch_outbox_once().await.expect("outbox") > 0 {}
    assert_eq!(k.get(&ctx, "SalesInvoice", uid(&la_invoice), false).await.unwrap()["us_avatax_status"], json!("Voided"));
    let voided = calls.lock().unwrap().iter().find(|(kind, _)| kind == "void").cloned().expect("voided").1;
    assert_eq!(
        (voided["company"].clone(), voided["code"].clone(), voided["body"]["code"].clone()),
        (json!("LSS"), la_invoice["number"].clone(), json!("DocVoided"))
    );

    // Use tax on equipment bought from Ohio without sales tax: at Austin's rate, owed by the company.
    let supplier = uid(&insert("Supplier", json!({"supplier_name": "Buckeye Tools"})).await);
    let pi = insert(
        "PurchaseInvoice",
        json!({"company": cid.to_string(), "supplier": supplier.to_string(), "bill_no": "BT-1", "us_accrue_use_tax": true,
               "items": [{"item": widget.to_string(), "qty": "1", "price_list_rate": "100"}]}),
    )
    .await;
    let row = &pi["taxes"][0];
    assert_eq!(
        (row["description"].clone(), row["reverse_charge"].clone(), dec(&row["tax_amount"]), dec(&pi["grand_total"])),
        (json!("Use Tax"), json!(true), n("8.25"), n("100"))
    );
    k.submit(&ctx, "PurchaseInvoice", uid(&pi), None).await.expect("purchase");

    // The reports.
    let report = async |name: &str, params: &[(&str, &str)]| -> Value {
        let mut p: HashMap<String, String> = params.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        p.insert("company".into(), cid.to_string());
        ferrum_domain::reports::run(&k, &ctx, name, &p).await.unwrap_or_else(|e| panic!("{name}: {e:?}"))
    };
    let jurisdictions = report("us_sales_tax", &[("from_date", "2026-01-01"), ("to_date", "2026-12-31")]).await;
    let row = |st: &str, j: &str| {
        jurisdictions["rows"].as_array().unwrap().iter().find(|r| r["state"] == json!(st) && r["jurisdiction"] == json!(j)).cloned().unwrap()
    };
    let austin = row("TX", "AUSTIN");
    assert_eq!((dec(&austin["gross"]), dec(&austin["exempt"]), dec(&austin["taxable"]), dec(&austin["tax"])), (n("200"), n("100"), n("100"), n("8.25")));
    let la_row = row("CA", "LOS ANGELES COUNTY");
    assert_eq!((dec(&la_row["exempt"]), dec(&la_row["taxable"])), (n("200"), n("0")), "the reseller's and the app; the cancelled sale is gone");
    assert_eq!(dec(&row("NV", "Nevada")["not_collected"]), n("100000"));

    let nexus = report("us_economic_nexus", &[]).await;
    let state = |st: &str| nexus["rows"].as_array().unwrap().iter().find(|r| r["state"] == json!(st)).cloned().unwrap();
    assert_eq!((state("NV")["status"].clone(), dec(&state("NV")["sales"])), (json!("Threshold reached: register"), n("100000")));
    assert_eq!(state("TX")["status"], json!("Registered"));

    // Forms 1099: what was paid in the year, by bank, against the $2,000 threshold of 2026.
    let bank = account("1121").await["id"].clone();
    let contractor = uid(&insert(
        "Supplier",
        json!({"supplier_name": "Jane Doe Consulting", "us_1099_form": "NEC", "us_tin_type": "SSN", "us_tin": "123-45-6789",
                                                   "us_w9_received": "2026-01-15"}),
    )
    .await);
    let small = uid(&insert("Supplier", json!({"supplier_name": "Odd Jobs", "us_1099_form": "NEC"})).await);
    for (supplier, amount, card) in [(contractor, "2500", false), (contractor, "300", true), (small, "1000", false)] {
        let pay = insert(
            "PaymentEntry",
            json!({"company": cid.to_string(), "payment_type": "Pay", "supplier": supplier.to_string(), "bank_account": bank, "currency": usd["id"],
                   "paid_amount": amount, "us_card_payment": card}),
        )
        .await;
        k.submit(&ctx, "PaymentEntry", uid(&pay), None).await.expect("payment");
    }
    let forms = report("us_1099", &[("as_of", "2026-12-31")]).await;
    let line = |name: &str| forms["rows"].as_array().unwrap().iter().find(|r| r["supplier"] == json!(name)).cloned().unwrap();
    let jane = line("Jane Doe Consulting");
    assert_eq!(
        (dec(&jane["paid"]), jane["reportable"].clone(), jane["tin"].clone(), jane["box"].clone(), jane["notes"].clone()),
        (n("2500"), json!(true), json!("***-**-6789"), json!("1099-NEC box 1"), json!(""))
    );
    let odd = line("Odd Jobs");
    assert_eq!((odd["reportable"].clone(), odd["notes"].as_str().unwrap().starts_with("No valid TIN")), (json!(false), true));
}
