//! Germany end to end against a real PostgreSQL (`FERRUM_DE_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_de_test`; reset first): XRechnung for a public
//! buyer, KoSIT's rules refusing an incomplete one, the domestic reverse charge (§ 13b UStG) and a
//! small business (§ 19 UStG) invoicing with its tax number.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
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

async fn fresh_kernel() -> Arc<Kernel> {
    let url = std::env::var("FERRUM_DE_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_de_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_eu::packages().expect("the EU's package"));
    packages.extend(ferrum_de::packages().expect("Germany's package"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    // The EU's rules first: Germany's adjust them.
    ferrum_eu::register(&mut hooks);
    ferrum_de::register(&mut hooks);
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
    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn first(&self, entity: &str, filters: Value) -> Doc {
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() };
        self.k.list(&self.ctx, entity, &q).await.unwrap().rows.remove(0)
    }
}

#[test]
fn germany_end_to_end() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let k = fresh_kernel().await;
    let tenant = k
        .create_tenant(NewTenant {
            slug: "de".into(),
            name: "Muster".into(),
            admin_email: "admin@muster.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying", "eu", "de"].iter().map(|s| s.to_string()).collect(),
            settings: Some(json!({"fiscal_year_start_month": 1})),
        })
        .await
        .expect("tenant");
    let session = k.login("de", "admin@muster.test", "Correct-Horse-9").await.expect("login");
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let t = T { k: k.clone(), ctx };
    let eur = t.first("Currency", json!({"code": "EUR"})).await;
    let nos = t.first("Uom", json!({"uom_name": "Nos"})).await;
    let group = t.first("ItemGroup", json!({"group_name": "Products"})).await;
    let company = |name: &'static str, abbr: &'static str, more: Value| {
        let (k, ctx, eur) = (t.k.clone(), t.ctx.clone(), eur["id"].clone());
        async move {
            let mut v = json!({"company_name": name, "abbr": abbr, "default_currency": eur, "country": "Germany"});
            v.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap_or_default());
            uid(&k.insert(&ctx, "Company", obj(v)).await.unwrap_or_else(|e| panic!("company {name}: {e:?}")))
        }
    };
    let customer = |name: &'static str, more: Value, city: &'static str, postal: &'static str| {
        let (k, ctx) = (t.k.clone(), t.ctx.clone());
        async move {
            let mut v = json!({"customer_name": name});
            v.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap_or_default());
            let c = uid(&k.insert(&ctx, "Customer", obj(v)).await.unwrap_or_else(|e| panic!("customer {name}: {e:?}")));
            let a = json!({"address_type": "Billing", "address_line1": "Amtsweg 1", "city": city, "postal_code": postal, "country": "Deutschland", "is_primary": true, "customer": c.to_string()});
            k.insert(&ctx, "Address", obj(a)).await.unwrap_or_else(|e| panic!("address of {name}: {e:?}"));
            c
        }
    };
    let item = |code: &'static str, more: Value| {
        let (k, ctx, g, u) = (t.k.clone(), t.ctx.clone(), group["id"].clone(), nos["id"].clone());
        async move {
            let mut v = json!({"item_code": code, "item_name": code, "item_group": g, "stock_uom": u, "is_stock_item": false, "standard_rate": "100", "eu_kind": "Services"});
            v.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap_or_default());
            uid(&k.insert(&ctx, "Item", obj(v)).await.unwrap_or_else(|e| panic!("item {code}: {e:?}")))
        }
    };
    let invoice = |company: Uuid, customer: Uuid, item: Uuid| {
        let (k, ctx) = (t.k.clone(), t.ctx.clone());
        async move {
            let v = json!({"company": company.to_string(), "customer": customer.to_string(), "items": [{"item": item.to_string(), "qty": "1"}]});
            k.insert(&ctx, "SalesInvoice", obj(v)).await.unwrap_or_else(|e| panic!("invoice: {e:?}"))
        }
    };

    // ───────────── XRechnung for a public buyer ─────────────
    let bad_iban =
        t.k.insert(
            &t.ctx,
            "Company",
            obj(json!({"company_name": "X", "abbr": "XX", "default_currency": eur["id"], "country": "Germany", "eu_iban": "DE89370400440532013001"})),
        )
        .await;
    assert!(matches!(bad_iban, Err(Error::Validation(ref e)) if e.iter().any(|f| f.field == "eu_iban")), "{bad_iban:?}");
    let muster = company(
        "Muster GmbH",
        "MG",
        json!({"eu_vat_number": "DE123456789", "eu_street": "Hauptstr. 1", "eu_city": "Berlin", "eu_postal_code": "10115", "eu_contact_name": "Erika Muster",
               "eu_contact_phone": "+49 30 1234567", "eu_contact_email": "rechnung@muster.de", "eu_iban": "DE89 3704 0044 0532 0130 00"}),
    )
    .await;
    let amt = customer("Bundesamt für Beispiele", json!({"de_leitweg_id": "991-12345-67", "customer_type": "Company"}), "Bonn", "53113").await;
    let advice = item("ADVICE", json!({})).await;
    let si = invoice(muster, amt, advice).await;
    assert_eq!((dec(&si["total_taxes"]), dec(&si["grand_total"])), (Decimal::new(19, 0), Decimal::new(119, 0)));
    let si = t.submit("SalesInvoice", uid(&si)).await;
    let x = t.k.run_action(&t.ctx, "SalesInvoice", uid(&si), "de_xrechnung", json!({})).await.expect("XRechnung");
    let xml = x["file"]["content"].as_str().unwrap();
    for part in [
        "<cbc:CustomizationID>urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_3.0</cbc:CustomizationID>",
        "<cbc:BuyerReference>991-12345-67</cbc:BuyerReference>",
        "<cbc:EndpointID schemeID=\"0204\">991-12345-67</cbc:EndpointID>",
        "<cbc:EndpointID schemeID=\"EM\">rechnung@muster.de</cbc:EndpointID>",
        "<cac:Contact><cbc:Name>Erika Muster</cbc:Name><cbc:Telephone>+49 30 1234567</cbc:Telephone><cbc:ElectronicMail>rechnung@muster.de</cbc:ElectronicMail></cac:Contact>",
        "<cbc:PaymentMeansCode>58</cbc:PaymentMeansCode><cac:PayeeFinancialAccount><cbc:ID>DE89370400440532013000</cbc:ID>",
        "<cbc:CityName>Berlin</cbc:CityName><cbc:PostalZone>10115</cbc:PostalZone>",
        "<cbc:CityName>Bonn</cbc:CityName><cbc:PostalZone>53113</cbc:PostalZone>",
    ] {
        assert!(xml.contains(part), "{part} in {xml}");
    }

    // KoSIT's rules refuse what is missing, and say which.
    let bare = company("Ohne GmbH", "OG", json!({"eu_vat_number": "DE111111111"})).await;
    let si = invoice(bare, amt, advice).await;
    let si = t.submit("SalesInvoice", uid(&si)).await;
    let refused = t.k.run_action(&t.ctx, "SalesInvoice", uid(&si), "de_xrechnung", json!({})).await;
    let Err(Error::Invalid(m)) = refused else { panic!("{refused:?}") };
    for rule in ["BR-DE-1", "BR-DE-2", "BR-DE-3", "BR-DE-4"] {
        assert!(m.contains(rule), "{rule} in {m}");
    }

    // ───────────── § 13b UStG: a builder in Germany owes the VAT ─────────────
    let bau = customer("Bau GmbH", json!({"eu_vat_number": "DE987654321"}), "Köln", "50667").await;
    let work = item("BAU", json!({"de_reverse_charge": true})).await;
    let si = invoice(muster, bau, work).await;
    assert_eq!((dec(&si["total_taxes"]), si["eu_supply"].clone()), (Decimal::ZERO, json!("Domestic")));
    assert_eq!(si["tax_breakdown"][0]["category"], json!("Reverse Charge"));
    assert!(si["eu_legal_text"].as_str().unwrap().contains("§ 13b UStG"), "{si:?}");
    let consumer = customer("Max Mustermann", json!({"customer_type": "Individual"}), "Köln", "50667").await;
    assert_eq!(dec(&invoice(muster, consumer, work).await["total_taxes"]), Decimal::new(19, 0), "a consumer pays the VAT");

    // ───────────── § 19 UStG: a small business, with its tax number ─────────────
    let klein =
        company("Klein Werkstatt", "KW", json!({"de_small_business": true, "eu_tax_number": "21/815/08150", "eu_city": "Leipzig", "eu_postal_code": "04109"}))
            .await;
    let si = invoice(klein, consumer, advice).await;
    assert_eq!((dec(&si["total_taxes"]), dec(&si["grand_total"])), (Decimal::ZERO, Decimal::new(100, 0)));
    assert_eq!(
        (si["tax_breakdown"][0]["category"].clone(), si["tax_breakdown"][0]["exemption_reason"].clone()),
        (json!("Exempt"), json!("Kleinunternehmerregelung (§ 19 UStG)"))
    );
    assert!(si["eu_legal_text"].as_str().unwrap().contains("§ 19 UStG"));
    let si = t.submit("SalesInvoice", uid(&si)).await;
    let ubl = t.k.run_action(&t.ctx, "SalesInvoice", uid(&si), "eu_ubl", json!({})).await.expect("EN 16931 with a tax number");
    let xml = ubl["file"]["content"].as_str().unwrap();
    assert!(xml.contains("<cbc:CompanyID>21/815/08150</cbc:CompanyID><cac:TaxScheme><cbc:ID>FC</cbc:ID>"), "{xml}");
    assert!(xml.contains("<cbc:TaxExemptionReason>Kleinunternehmerregelung (§ 19 UStG)</cbc:TaxExemptionReason>"), "{xml}");
}
