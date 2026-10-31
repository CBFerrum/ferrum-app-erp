//! Belgium, Italy and Poland end to end against a real PostgreSQL
//! (`FERRUM_EU_MEMBERS_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_eu_members_test`; reset first), each in its own
//! workspace on the EU's add-on: Belgian business invoices queued for Peppol and the contractors'
//! reverse charge; Italian FatturaPA queued for SDI with Natura, stamp duty and SDI's receipts;
//! Polish FA(3) queued for KSeF, its number recorded and a correction citing it.

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
    let url = std::env::var("FERRUM_EU_MEMBERS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_eu_members_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_eu::packages().expect("the EU's package"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    ferrum_eu::register(&mut hooks);
    macro_rules! add {
        ($app:path) => {{
            use $app as app;
            packages.extend(app::packages().expect("a member state's package"));
            hooks.scoped(&[app::PACKAGE.to_string()], |h| app::register(h));
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
    add!(ferrum_eu_members::ro);
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let plugins = ferrum_plugins::PluginHost::new(false).expect("plugin host");
    hooks.register("*", Arc::new(ferrum_plugins::PluginHook(plugins)));
    let settings = KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([3u8; 32])), ..Default::default() };
    let k = Kernel::boot(db, catalog, hooks, settings, None).await.expect("boot");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    k
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

#[test]
fn member_states_end_to_end() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let k = fresh_kernel().await;
    belgium(&k).await;
    italy(&k).await;
    poland(&k).await;
    romania(&k).await;
    spain(&k).await;
    france(&k).await;
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

async fn romania(k: &Arc<Kernel>) {
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
    assert_eq!(t.get("SalesInvoice", uid(&si)).await["it_sdi_file"], json!("IT01234567897_00003.xml"), "each transmission its own progressive");
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
}
