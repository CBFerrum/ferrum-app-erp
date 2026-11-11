//! Withholding by category (FN-28) against a real PostgreSQL (`FERRUM_ERP_WHT_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_wht_test`; reset first).
//!
//! Categories made here as an administrator would (the core seeds none): a yearly threshold not
//! reached and then crossed, once withholding on the whole year's base and once on the part above
//! it; a lower-rate certificate up to its limit; deduction on payment, part by part and in
//! proportion to the bill's net; an advance and then its bill, withheld once; a credit note; a
//! cancellation; collection on a sale; and the report. Every document's postings balance.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
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

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, Decimal> {
    pairs.iter().map(|(a, v)| (a.to_string(), n(v))).collect()
}

struct T {
    k: Arc<Kernel>,
    ctx: Ctx,
}

impl T {
    async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.k.insert(&self.ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.ctx, entity, id, false).await.unwrap()
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let page =
            self.k.list(&self.ctx, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() }).await.unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
    /// Inserted and submitted.
    async fn post(&self, entity: &str, v: Value) -> Doc {
        let d = self.insert(entity, v).await;
        self.submit(entity, uid(&d)).await
    }
    /// What a document posted, by account name (debit minus credit, reversals included); it must balance.
    async fn posted(&self, source: Uuid) -> BTreeMap<String, Decimal> {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        let rows: Vec<(String, Decimal)> = sqlx::query_as(
            "select a.account_name, sum(l.debit - l.credit) from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
             join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id where l.tenant_id = $1 and e.source_id = $2 group by 1 having sum(l.debit - l.credit) <> 0",
        )
        .bind(self.ctx.tenant)
        .bind(source)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        let out: BTreeMap<String, Decimal> = rows.into_iter().collect();
        assert_eq!(out.values().copied().sum::<Decimal>(), Decimal::ZERO, "balanced: {out:?}");
        out
    }
    fn withheld(doc: &Doc) -> Vec<(Decimal, Decimal, Decimal, Decimal)> {
        doc["withholdings"].as_array().unwrap().iter().map(|r| (dec(&r["base"]), dec(&r["taxable"]), dec(&r["rate"]), dec(&r["amount"]))).collect()
    }
}

#[test]
fn withholding_by_category() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_WHT_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_wht_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    for source in ferrum_app_erp::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let k: Arc<Kernel> = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "wh".into(),
            name: "Withholding Co".into(),
            admin_email: "admin@wh.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying"].iter().map(|s| s.to_string()).collect(),
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("wh", "admin@wh.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), ctx };

    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let co = t.insert("Company", json!({"company_name": "Withholding Co", "abbr": "WC", "default_currency": inr["id"]})).await;
    let c = uid(&co).to_string();
    let bank = t.get("Company", uid(&co)).await["default_bank_account"].clone();
    let account = |name: &'static str| {
        let (t, c) = (&t, c.clone());
        async move { t.find_one("Account", json!({"account_name": name, "company": c})).await["id"].clone() }
    };
    let (payable, collected, input) = (account("TDS Payable").await, account("Output Tax").await, account("Input Tax").await);
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let work = uid(&t
        .insert("Item", json!({"item_code": "Work", "item_name": "Work", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "100", "is_stock_item": false}))
        .await)
    .to_string();
    let category = |name: &'static str, applies: &'static str, more: Value, rate: Value| {
        let (t, c, payable, collected) = (&t, c.clone(), payable.clone(), collected.clone());
        async move {
            let mut v = obj(json!({"category_name": name, "applies": applies, "rates": [rate],
                "accounts": [{"company": c, "account": if applies == "On Sale" { collected } else { payable }}]}));
            v.extend(obj(more));
            uid(&t.insert("WithholdingCategory", Value::Object(v)).await).to_string()
        }
    };
    let from = "2000-01-01";
    let whole_year = category(
        "Contracts",
        "On Bill",
        json!({"round_to_unit": true}),
        json!({"valid_from": from, "rate": "10", "single_threshold": "3000", "yearly_threshold": "5000"}),
    )
    .await;
    let above =
        category("Goods", "On Bill", json!({"cumulative_base": "Above Threshold"}), json!({"valid_from": from, "rate": "10", "yearly_threshold": "5000"}))
            .await;
    let fees = category("Fees", "On Bill", json!({}), json!({"valid_from": from, "rate": "10"})).await;
    let on_payment = category("Services", "On Payment", json!({}), json!({"valid_from": from, "rate": "10"})).await;
    let on_sale = category("Collected", "On Sale", json!({}), json!({"valid_from": from, "rate": "1"})).await;
    let supplier = |name: &'static str, cat: &String| {
        let (t, cat) = (&t, cat.clone());
        async move { uid(&t.insert("Supplier", json!({"supplier_name": name, "withholding_category": cat})).await).to_string() }
    };
    let bill = |supplier: &String, amount: &str| json!({"company": c, "supplier": supplier, "bill_no": Uuid::new_v4().to_string(), "items": [{"item": work, "qty": "1", "price_list_rate": amount}]});

    // ───────────── a yearly threshold not reached, then crossed: the whole year's base ─────────────
    let s1 = supplier("Builder", &whole_year).await;
    let b1 = t.post("PurchaseInvoice", bill(&s1, "800")).await;
    assert_eq!(T::withheld(&b1), vec![(n("800"), n("0"), n("10"), n("0"))], "under both thresholds: nothing withheld, the base counted");
    assert_eq!(t.posted(uid(&b1)).await, map(&[("Administrative Expenses", "800"), ("Creditors", "-800")]));
    t.post("PurchaseInvoice", bill(&s1, "900")).await;
    t.post("PurchaseInvoice", bill(&s1, "2900")).await;
    let b4 = t.post("PurchaseInvoice", bill(&s1, "1000")).await;
    // 800 + 900 + 2900 + 1000 = 5600 > 5000: withheld on this 1000 and the 4600 before it.
    assert_eq!(T::withheld(&b4), vec![(n("1000"), n("5600"), n("10"), n("560"))]);
    assert_eq!((dec(&b4["withholding_amount"]), dec(&b4["outstanding_amount"])), (n("560"), n("440")));
    assert_eq!(t.posted(uid(&b4)).await, map(&[("Administrative Expenses", "1000"), ("Creditors", "-440"), ("TDS Payable", "-560")]));

    // ───────────── crossed, withholding on the part above it only ─────────────
    let s2 = supplier("Trader", &above).await;
    let g1 = t.post("PurchaseInvoice", bill(&s2, "4000")).await;
    assert_eq!(T::withheld(&g1), vec![(n("4000"), n("0"), n("10"), n("0"))]);
    let g2 = t.post("PurchaseInvoice", bill(&s2, "1500")).await;
    assert_eq!(T::withheld(&g2), vec![(n("1500"), n("500"), n("10"), n("50"))], "5500 is 500 above the threshold");
    assert_eq!(t.posted(uid(&g2)).await, map(&[("Administrative Expenses", "1500"), ("Creditors", "-1450"), ("TDS Payable", "-50")]));

    // ───────────── a certificate: 2% up to 1000 of base, then the category's 10% ─────────────
    let s3 = supplier("Consultant", &fees).await;
    t.insert(
        "WithholdingCertificate",
        json!({"certificate_no": "LDC-1", "category": fees, "party_type": "Supplier", "supplier": s3, "valid_from": from, "rate": "2", "limit": "1000"}),
    )
    .await;
    let f1 = t.post("PurchaseInvoice", bill(&s3, "600")).await;
    assert_eq!(T::withheld(&f1), vec![(n("600"), n("600"), n("2"), n("12"))]);
    let f2 = t.post("PurchaseInvoice", bill(&s3, "800")).await;
    assert_eq!(T::withheld(&f2), vec![(n("800"), n("400"), n("2"), n("8")), (n("0"), n("400"), n("10"), n("40"))], "400 left under the certificate");
    assert!(f2["withholdings"][0]["certificate"].is_string() && f2["withholdings"][1]["certificate"].is_null());
    assert_eq!(t.posted(uid(&f2)).await, map(&[("Administrative Expenses", "800"), ("Creditors", "-752"), ("TDS Payable", "-48")]));

    // ───────────── on payment, part by part, on the bill's net ─────────────
    let s4 = supplier("Agency", &on_payment).await;
    let vat = uid(&t
        .insert("TaxTemplate", json!({"title": "Input VAT 10%", "company": c, "kind": "Purchase", "rows": [{"charge_type": "On Net Total", "account": input, "description": "VAT", "rate": "10"}]}))
        .await)
    .to_string();
    let mut taxed = bill(&s4, "1000");
    taxed["tax_template"] = json!(vat);
    let p_bill = t.post("PurchaseInvoice", taxed).await;
    assert_eq!((T::withheld(&p_bill), dec(&p_bill["outstanding_amount"])), (vec![], n("1100")), "an On Payment category waits for the payment");
    let pay = |amount: &str, allocated: &str| {
        json!({"company": c, "payment_type": "Pay", "supplier": s4, "bank_account": bank, "currency": inr["id"], "paid_amount": amount,
               "references": [{"invoice": p_bill["id"], "allocated_amount": allocated}]})
    };
    // 550 of 1100 settled: half the net, 500, withheld on; 500 leaves the bank.
    let pay1 = t.post("PaymentEntry", pay("500", "550")).await;
    assert_eq!(T::withheld(&pay1), vec![(n("500"), n("500"), n("10"), n("50"))]);
    assert_eq!((dec(&pay1["withholding_amount"]), dec(&pay1["unallocated_amount"])), (n("50"), n("0")));
    assert_eq!(t.posted(uid(&pay1)).await, map(&[("Creditors", "550"), ("Primary Bank", "-500"), ("TDS Payable", "-50")]));
    assert_eq!(dec(&t.get("PurchaseInvoice", uid(&p_bill)).await["outstanding_amount"]), n("550"));
    let pay2 = t.post("PaymentEntry", pay("500", "550")).await;
    assert_eq!(dec(&t.get("PurchaseInvoice", uid(&p_bill)).await["outstanding_amount"]), n("0"));
    // A payment cancelled gives its withholding back.
    t.k.cancel(&t.ctx, "PaymentEntry", uid(&pay2), None).await.expect("cancel");
    assert_eq!(t.posted(uid(&pay2)).await, map(&[]));
    assert_eq!(dec(&t.get("PurchaseInvoice", uid(&p_bill)).await["outstanding_amount"]), n("550"));

    // ───────────── an advance, then its bill: withheld once ─────────────
    let s5 = supplier("Landlord", &fees).await;
    let advance = t
        .post("PaymentEntry", json!({"company": c, "payment_type": "Pay", "supplier": s5, "bank_account": bank, "currency": inr["id"], "paid_amount": "900"}))
        .await;
    assert_eq!(T::withheld(&advance), vec![(n("900"), n("900"), n("10"), n("90"))], "withheld on what is advanced");
    assert_eq!(dec(&advance["unallocated_amount"]), n("990"), "the landlord's credit: the money and what was withheld from it");
    assert_eq!(t.posted(uid(&advance)).await, map(&[("Creditors", "990"), ("Primary Bank", "-900"), ("TDS Payable", "-90")]));
    let rent = t.post("PurchaseInvoice", bill(&s5, "1000")).await;
    assert_eq!(T::withheld(&rent), vec![(n("100"), n("100"), n("10"), n("10"))], "900 of it was withheld on with the advance");
    assert_eq!(dec(&rent["withholding_amount"]), n("10"), "100 in all, not 190");

    // ───────────── a credit note reverses its share ─────────────
    let mut credit = bill(&s1, "500");
    credit["is_return"] = json!(true);
    credit["return_against"] = b4["id"].clone();
    credit["items"][0]["qty"] = json!("-1");
    let note = t.post("PurchaseInvoice", credit).await;
    assert_eq!(T::withheld(&note), vec![(n("-500"), n("-2800"), n("10"), n("-280"))], "half of the bill: half of what it withheld");
    assert_eq!(t.posted(uid(&note)).await, map(&[("Administrative Expenses", "-500"), ("Creditors", "220"), ("TDS Payable", "280")]));

    // ───────────── a cancelled bill gives back what it withheld and leaves the year ─────────────
    t.k.cancel(&t.ctx, "PurchaseInvoice", uid(&g2), None).await.expect("cancel");
    assert_eq!(t.posted(uid(&g2)).await, map(&[]));
    let g3 = t.post("PurchaseInvoice", bill(&s2, "1500")).await;
    assert_eq!(T::withheld(&g3), vec![(n("1500"), n("500"), n("10"), n("50"))], "the cancelled bill is out of the year");

    // ───────────── collected on a sale ─────────────
    let customer = uid(&t.insert("Customer", json!({"customer_name": "Retail", "withholding_category": on_sale})).await).to_string();
    let sale = t.post("SalesInvoice", json!({"company": c, "customer": customer, "items": [{"item": work, "qty": "10"}]})).await;
    assert_eq!(T::withheld(&sale), vec![(n("1000"), n("1000"), n("1"), n("10"))]);
    assert_eq!(dec(&sale["outstanding_amount"]), n("1010"));
    assert_eq!(t.posted(uid(&sale)).await, map(&[("Debtors", "1010"), ("Output Tax", "-10"), ("Sales", "-1000")]));

    // ───────────── the report ─────────────
    let params: HashMap<String, String> = [("company", c.clone())].into_iter().map(|(a, b)| (a.to_string(), b)).collect();
    let report = ferrum_domain::reports::run(&t.k, &t.ctx, "withholding", &params).await.expect("withholding report");
    let rows = report["rows"].as_array().unwrap();
    let total: Decimal = rows.iter().map(|r| dec(&r["amount"])).sum();
    // 560 − 280, 50 (g3; g2 cancelled), 12 + 48, 50 (pay1; pay2 cancelled), 90 + 10, 10 collected.
    assert_eq!(total, n("550"));
    assert_eq!(dec(&report["totals"]["amount"]), n("550"));
    assert!(rows.iter().all(|r| r["voucher_id"].is_string() && r["category"].is_string() && r["party"].is_string()), "{rows:?}");
}
