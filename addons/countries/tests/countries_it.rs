//! Every country of the engine end to end against a real PostgreSQL
//! (`FERRUM_COUNTRIES_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_countries_test`; reset first): one workspace per
//! country with its package, a company there selling at home and abroad, buying at home, buying
//! services from abroad, and its return.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings, ListQuery};
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
        Value::String(s) => Decimal::from_str(s).unwrap(),
        Value::Null => Decimal::ZERO,
        other => Decimal::from_str(&other.to_string()).unwrap(),
    }
}

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

macro_rules! each_app {
    ($f:ident) => {
        $f!(ferrum_countries::ae);
        $f!(ferrum_countries::au);
        $f!(ferrum_countries::cn);
        $f!(ferrum_countries::id);
        $f!(ferrum_countries::jp);
        $f!(ferrum_countries::mx);
        $f!(ferrum_countries::my);
        $f!(ferrum_countries::nz);
        $f!(ferrum_countries::ph);
        $f!(ferrum_countries::sa);
        $f!(ferrum_countries::sg);
        $f!(ferrum_countries::th);
        $f!(ferrum_countries::vn);
    };
}

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_COUNTRIES_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_countries_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    macro_rules! add {
        ($app:path) => {{
            use $app as app;
            packages.extend(app::packages().expect("a country's package"));
            let names: Vec<String> = app::packages().unwrap().iter().map(|p| p.name().to_string()).collect();
            hooks.scoped(&names, |h| app::register(h));
            for source in app::reports() {
                ferrum_domain::reports::add_source(source);
            }
        }};
    }
    each_app!(add);
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let plugins = ferrum_plugins::PluginHost::new(false).expect("plugin host");
    hooks.register("*", Arc::new(ferrum_plugins::PluginHook(plugins)));
    let settings = KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([9u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    k
}

/// A country's scenario: its name, currency, a registration number, the categories of the goods
/// and of the services sold and bought, a setting the company needs for the reverse charge, and
/// the return with the boxes to check given the tax on the sale (T), on the purchase (I) and
/// reverse-charged (R).
struct Case {
    code: &'static str,
    country: &'static str,
    currency: &'static str,
    number: &'static str,
    goods: Option<&'static str>,
    services: Option<&'static str>,
    company: Value,
    report: &'static str,
    t: i64,
    i: i64,
    r: i64,
    boxes: &'static [(&'static str, i64)],
}

fn cases() -> Vec<Case> {
    let c = |code, country, currency, number, goods, services, company: Value, report, t, i, r, boxes| Case {
        code,
        country,
        currency,
        number,
        goods,
        services,
        company,
        report,
        t,
        i,
        r,
        boxes,
    };
    vec![
        c(
            "sg",
            "Singapore",
            "SGD",
            "201912345K",
            None,
            None,
            json!({"sg_reverse_charge": true}),
            "sg_gst_f5",
            9,
            9,
            9,
            &[("1", 200), ("2", 100), ("6", 18), ("7", 18), ("8", 0), ("14", 100)],
        ),
        c(
            "my",
            "Malaysia",
            "MYR",
            "W10-1808-32000001",
            Some("Sales Tax 10%"),
            Some("Service Tax"),
            json!({}),
            "my_sst02",
            10,
            0,
            8,
            &[("ST", 10), ("ITST", 8), ("TOTAL", 18)],
        ),
        c(
            "id",
            "Indonesia",
            "IDR",
            "0012345678901000",
            None,
            None,
            json!({}),
            "id_spt_masa_ppn",
            11,
            11,
            11,
            &[("PK", 11), ("JLN", 11), ("PM", 22), ("KB", 0)],
        ),
        c(
            "th",
            "Thailand",
            "THB",
            "0105536112014",
            None,
            None,
            json!({}),
            "th_pp30",
            7,
            7,
            7,
            &[("1", 200), ("4", 100), ("5", 7), ("7", 14), ("8", 0), ("9", 7)],
        ),
        c("ph", "Philippines", "PHP", "123456789000", None, None, json!({}), "ph_2550q", 12, 12, 12, &[("VS", 100), ("OT", 12), ("IT", 24), ("NV", -12)]),
        c(
            "vn",
            "Vietnam",
            "VND",
            "0101234567",
            None,
            None,
            json!({}),
            "vn_01_gtgt",
            10,
            10,
            10,
            &[("[29]", 100), ("[32]", 100), ("[35]", 10), ("[24]", 20), ("[36]", -10)],
        ),
        c(
            "sa",
            "Saudi Arabia",
            "SAR",
            "310123456700003",
            None,
            None,
            json!({}),
            "sa_vat_return",
            15,
            15,
            15,
            &[("1 VAT", 15), ("4", 100), ("9 VAT", 15), ("13", 0)],
        ),
        c(
            "ae",
            "United Arab Emirates",
            "AED",
            "100123456789003",
            None,
            None,
            json!({"ae_emirate": "Dubai"}),
            "ae_vat201",
            5,
            5,
            5,
            &[("1a", 0), ("1b", 100), ("3", 100), ("4", 100), ("12", 10), ("13", 10), ("14", 0)],
        ),
        c(
            "au",
            "Australia",
            "AUD",
            "51824753556",
            None,
            None,
            json!({"au_reverse_charge": true}),
            "au_bas_gst",
            10,
            10,
            10,
            &[("G1", 210), ("G2", 100), ("1A", 20), ("1B", 20)],
        ),
        c(
            "nz",
            "New Zealand",
            "NZD",
            "49091850",
            None,
            None,
            json!({"nz_reverse_charge": true}),
            "nz_gst101a",
            15,
            15,
            15,
            &[("5", 215), ("6", 100), ("8", 15), ("15", 0)],
        ),
        c(
            "jp",
            "Japan",
            "JPY",
            "T7000012050002",
            None,
            None,
            json!({"jp_reverse_charge": true}),
            "jp_consumption_tax",
            10,
            10,
            10,
            &[("S10", 100), ("EXP", 100), ("T10", 20), ("I10", 20), ("NET", 0)],
        ),
        c("mx", "Mexico", "MXN", "ABC680524P76", None, None, json!({}), "mx_iva", 16, 16, 16, &[("G16", 100), ("IVA-T", 16), ("IVA", 16)]),
        c(
            "cn",
            "China",
            "CNY",
            "91350100M000100Y43",
            None,
            Some("6%"),
            json!({}),
            "cn_vat_return",
            13,
            13,
            6,
            &[("13", 100), ("EXP", 100), ("OUT", 13), ("IN", 19), ("DUE", -6)],
        ),
    ]
}

#[test]
fn countries_end_to_end() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let k = fresh_kernel().await;
    for case in cases() {
        run(&k, &case).await;
    }
}

async fn run(k: &Arc<Kernel>, case: &Case) {
    let code = case.code;
    let tenant = k
        .create_tenant(NewTenant {
            slug: code.into(),
            name: case.country.into(),
            admin_email: format!("admin@{code}.test"),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying", code].iter().map(|s| s.to_string()).collect(),
            settings: Some(json!({"fiscal_year_start_month": 1})),
        })
        .await
        .unwrap_or_else(|e| panic!("{code}: tenant: {e:?}"));
    let session = k.login(code, &format!("admin@{code}.test"), "Correct-Horse-9").await.expect("login");
    let ctx: Ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let list = async |entity: &str, filters: Value| -> Vec<Doc> {
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() };
        k.list(&ctx, entity, &q).await.unwrap().rows
    };
    let insert = async |entity: &str, v: Value| -> Doc { k.insert(&ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("{code}: insert {entity}: {e:?}")) };
    let currency = match list("Currency", json!({"code": case.currency})).await.into_iter().next() {
        Some(c) => c,
        None => insert("Currency", json!({"code": case.currency, "currency_name": case.currency, "symbol": case.currency, "precision": 2})).await,
    };
    let (_, country) = ferrum_countries::all().into_iter().find(|(c, _)| *c == code).unwrap();
    let mut company = obj(
        json!({"company_name": format!("{} Trading", case.country), "abbr": code.to_uppercase(), "default_currency": currency["id"], "country": case.country}),
    );
    company.insert(country.number_field.into(), json!(case.number));
    company.extend(case.company.as_object().cloned().unwrap_or_default());
    let co = insert("Company", Value::Object(company)).await;
    let cid = uid(&co);
    assert!(co[country.number_field].as_str().is_some_and(|n| !n.is_empty()), "{code}: the number is kept");
    let mut patch = Doc::new();
    patch.insert(country.number_field.into(), json!("X1"));
    let bad = k.update(&ctx, "Company", cid, patch, None).await;
    assert!(bad.is_err(), "{code}: a number that is not one is refused");

    let nos = list("Uom", json!({"uom_name": "Nos"})).await.remove(0);
    let group = list("ItemGroup", json!({"group_name": "Products"})).await.remove(0);
    let item = async |name: &str, kind: &str, category: Option<&str>| -> Uuid {
        let mut v = obj(
            json!({"item_code": name, "item_name": name, "item_group": group["id"], "stock_uom": nos["id"], "is_stock_item": false, "standard_rate": "100"}),
        );
        v.insert(country.kind_field.into(), json!(kind));
        if let Some(c) = category {
            v.insert(country.category_field.into(), json!(c));
        }
        uid(&insert("Item", Value::Object(v)).await)
    };
    let goods = item("GOODS", "Goods", case.goods).await;
    let services = item("CONSULTING", "Services", case.services).await;
    let party = async |entity: &str, name: &str, country: &str| -> Uuid {
        let mut v = Doc::new();
        v.insert(if entity == "Customer" { "customer_name" } else { "supplier_name" }.into(), json!(name));
        let p = uid(&insert(entity, Value::Object(v)).await);
        let mut a = obj(json!({"address_type": "Billing", "address_line1": "1 Main Road", "city": "City", "country": country, "is_primary": true}));
        a.insert(entity.to_lowercase(), json!(p.to_string()));
        insert("Address", Value::Object(a)).await;
        p
    };
    let home = party("Customer", "Local Buyer", case.country).await;
    let abroad = party("Customer", "Overseas Buyer", "United States").await;
    let sale = async |customer: Uuid| -> Doc {
        insert("SalesInvoice", json!({"company": cid.to_string(), "customer": customer.to_string(), "items": [{"item": goods.to_string(), "qty": "1"}]})).await
    };
    let si = sale(home).await;
    assert_eq!(dec(&si["total_taxes"]), Decimal::from(case.t), "{code}: the sale at home");
    let si = k.submit(&ctx, "SalesInvoice", uid(&si), None).await.unwrap_or_else(|e| panic!("{code}: submit: {e:?}"));
    let export = sale(abroad).await;
    assert_eq!((dec(&export["total_taxes"]), export["tax_breakdown"][0]["category"].clone()), (Decimal::ZERO, json!("Export")), "{code}: the export");
    k.submit(&ctx, "SalesInvoice", uid(&export), None).await.unwrap_or_else(|e| panic!("{code}: submit export: {e:?}"));

    let supplier = party("Supplier", "Local Supplier", case.country).await;
    let pi = insert(
        "PurchaseInvoice",
        json!({"company": cid.to_string(), "supplier": supplier.to_string(), "bill_no": "L-1", "items": [{"item": goods.to_string(), "qty": "1", "price_list_rate": "100"}]}),
    )
    .await;
    assert_eq!(dec(&pi["total_taxes"]), Decimal::from(case.i), "{code}: the purchase at home");
    k.submit(&ctx, "PurchaseInvoice", uid(&pi), None).await.unwrap_or_else(|e| panic!("{code}: submit purchase: {e:?}"));
    let foreign = party("Supplier", "Overseas Adviser", "United States").await;
    let rc = insert(
        "PurchaseInvoice",
        json!({"company": cid.to_string(), "supplier": foreign.to_string(), "bill_no": "F-1", "items": [{"item": services.to_string(), "qty": "1", "price_list_rate": "100"}]}),
    )
    .await;
    let row = rc["taxes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["reverse_charge"] == json!(true))
        .cloned()
        .unwrap_or_else(|| panic!("{code}: no reverse charge: {:?}", rc["taxes"]));
    assert_eq!(
        (dec(&row["tax_amount"]), dec(&rc["grand_total"])),
        (Decimal::from(case.r), Decimal::from(100)),
        "{code}: services from abroad, reverse-charged"
    );
    k.submit(&ctx, "PurchaseInvoice", uid(&rc), None).await.unwrap_or_else(|e| panic!("{code}: submit reverse charge: {e:?}"));

    if code == "mx" {
        // IVA is owed when collected: the local sale is paid.
        let bank = list("Account", json!({"company": cid.to_string(), "account_number": "1121"})).await.remove(0);
        let pay = insert(
            "PaymentEntry",
            json!({"company": cid.to_string(), "payment_type": "Receive", "customer": home.to_string(), "bank_account": bank["id"], "currency": currency["id"],
                   "paid_amount": si["grand_total"], "references": [{"invoice_type": "Sales Invoice", "invoice": si["id"], "allocated_amount": si["grand_total"]}]}),
        )
        .await;
        k.submit(&ctx, "PaymentEntry", uid(&pay), None).await.expect("payment");
    }

    let p: HashMap<String, String> = [("company", cid.to_string()), ("from_date", "2000-01-01".to_string()), ("to_date", "2100-12-31".to_string())]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let report = ferrum_domain::reports::run(k, &ctx, case.report, &p).await.unwrap_or_else(|e| panic!("{code}: {}: {e:?}", case.report));
    let got: HashMap<String, Decimal> =
        report["rows"].as_array().unwrap().iter().map(|r| (r["box"].as_str().unwrap().to_string(), dec(&r["amount"]))).collect();
    for (b, want) in case.boxes {
        assert_eq!(got.get(*b).copied(), Some(Decimal::from(*want)), "{code}: box {b} of {:?}", report["rows"]);
    }
}
