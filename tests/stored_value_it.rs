//! Stored value (gift cards, store credit, prepaid packages) against a real PostgreSQL
//! (`FERRUM_ERP_SV_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_sv_test`; reset first).
//!
//! Kinds made as an administrator would (none is seeded), value issued by hand, codes kept as hashes,
//! guessing refused, the ledger append-only and never below nothing, and the check that every
//! account is the sum of its entries and every liability account the sum of its accounts' values.

use ferrum_app_erp::stored_value;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, HookRegistry, Kernel, KernelSettings, ListQuery};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

fn vid(v: &Value) -> Uuid {
    v.as_str().unwrap().parse().unwrap()
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
    /// Inserted and submitted.
    async fn post(&self, entity: &str, v: Value) -> Doc {
        let d = self.insert(entity, v).await;
        self.submit(entity, uid(&d)).await
    }
    async fn cancel(&self, entity: &str, id: Uuid) -> ferrum_kernel::Result<Doc> {
        self.k.cancel(&self.ctx, entity, id, None).await
    }
    /// The account a document's stored value entries are on.
    async fn account_of(&self, source: Uuid) -> Uuid {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        sqlx::query_scalar("select account_id from stored_value_entry where tenant_id = $1 and source_id = $2 order by created_at limit 1")
            .bind(self.ctx.tenant)
            .bind(source)
            .fetch_one(&mut *tx)
            .await
            .unwrap()
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
    /// What an operation or document posted, by account name (debit minus credit, reversals included); it must balance.
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
    /// The check finds nothing wrong.
    async fn agrees(&self) {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        assert_eq!(stored_value::check(&self.ctx, &mut tx).await.unwrap(), Vec::<String>::new());
    }
    async fn account(&self, id: Uuid) -> (Decimal, Decimal, String) {
        let a = self.get("StoredValueAccount", id).await;
        (dec(&a["balance"]), dec(&a["value"]), a["state"].as_str().unwrap_or_default().to_string())
    }
    async fn issue(
        &self,
        kind: &str,
        amount: &str,
        holder: Option<Uuid>,
        code: Option<&str>,
        pin: Option<&str>,
        key: Option<&str>,
    ) -> ferrum_kernel::Result<Value> {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        // Through the capability gate, once per key.
        let key = key.map(str::to_string).unwrap_or_else(|| Uuid::new_v4().to_string());
        let out = stored_value::issue(&self.k, &self.ctx, &mut tx, kind.parse().unwrap(), n(amount), "goodwill", holder, code, pin, &key).await?;
        tx.commit().await.unwrap();
        Ok(out)
    }
    async fn find(&self, kind: &str, code: &str, pin: Option<&str>) -> ferrum_kernel::Result<Uuid> {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        stored_value::find(&self.k, &self.ctx, &mut tx, Some(kind.parse().unwrap()), code, pin).await
    }
}

#[test]
fn stored_value() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_SV_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_sv_test".into());
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
            slug: "sv".into(),
            name: "Stored Value Co".into(),
            admin_email: "admin@sv.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying"].iter().map(|s| s.to_string()).collect(),
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("sv", "admin@sv.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), ctx };

    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let co = t.insert("Company", json!({"company_name": "Stored Value Co", "abbr": "SV", "default_currency": inr["id"]})).await;
    let c = uid(&co).to_string();
    let account = |name: &'static str| {
        let (t, c) = (&t, c.clone());
        async move { t.find_one("Account", json!({"account_name": name, "company": c})).await["id"].clone() }
    };
    let current = account("Current Liabilities").await;
    let liability =
        t.insert("Account", json!({"account_name": "Gift Cards", "company": c, "parent_account": current, "root_type": "Liability"})).await["id"].clone();
    let packages =
        t.insert("Account", json!({"account_name": "Prepaid Packages", "company": c, "parent_account": current, "root_type": "Liability"})).await["id"].clone();
    let (income, goodwill) = (account("Other Income").await, account("Administrative Expenses").await);

    // ───────────── 1. kinds: what a business varies, with stated defaults ─────────────
    let gift =
        t.insert("StoredValueKind", json!({"kind_name": "Gift Card", "company": c, "liability_account": liability, "adjustment_account": goodwill})).await;
    assert_eq!(
        (gift["holds"].as_str(), gift["held_by"].as_str(), gift["tax_charged"].as_str(), gift["expiry"].as_str(), gift["on_expiry"].as_str()),
        (Some("Money"), Some("Bearer"), Some("When Sold"), Some("None"), Some("Stays Owed"))
    );
    assert_eq!((gift["can_top_up"].as_bool(), gift["partial_use"].as_bool(), gift["refund_to_account"].as_bool()), (Some(true), Some(true), Some(true)));
    assert_eq!(gift["currency"], inr["id"], "a kind that holds money holds the company's currency");
    let gift = gift["id"].as_str().unwrap().to_string();
    let refused =
        t.k.insert(
            &t.ctx,
            "StoredValueKind",
            obj(json!({"kind_name": "Sessions", "company": c, "holds": "Units", "unit_name": "session", "liability_account": packages})),
        )
        .await;
    assert!(format!("{:?}", refused.unwrap_err()).contains("Income Account"), "units need the account their price differences go to");
    let refused = t
        .k
        .insert(&t.ctx, "StoredValueKind", obj(json!({"kind_name": "Vouchers", "company": c, "liability_account": liability, "on_expiry": "Income at Expiry", "expiry": "From Issue", "expiry_days": 30})))
        .await;
    assert!(format!("{:?}", refused.unwrap_err()).contains("Income Account"), "breakage needs an income account");
    let pinned = t
        .insert("StoredValueKind", json!({"kind_name": "Store Credit", "company": c, "liability_account": liability, "adjustment_account": goodwill, "held_by": "Customer", "requires_pin": true, "income_account": income}))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();

    // ───────────── 2. accounts and their ledger ─────────────
    // Issued by hand with a card's own code: the code is not kept, only its hash and last four.
    let code = "4000-1234-5678-9ABC";
    let first = t.issue(&gift, "500", None, Some(code), None, None).await.unwrap();
    let a1 = vid(&first["account"]);
    assert_eq!(first["code"], Value::Null, "the card's own code is not handed back");
    assert_eq!(t.account(a1).await, (n("500"), n("500"), "Active".into()));
    let row = t.get("StoredValueAccount", a1).await;
    assert_eq!(row["code_hint"], json!("9ABC"));
    assert!(!serde_json::to_string(&row).unwrap().contains("56789ABC"), "no part of the code but its last four is stored");
    assert_eq!(t.posted(vid(&first["entry"])).await, map(&[("Administrative Expenses", "500"), ("Gift Cards", "-500")]));
    assert_eq!(t.find(&gift, "4000 1234 5678 9abc", None).await.unwrap(), a1, "typed with spaces, in small letters");
    // Without a code one is made, shown once, and it finds the account.
    let second = t.issue(&gift, "200", None, None, None, None).await.unwrap();
    let made = second["code"].as_str().unwrap().to_string();
    assert_eq!(made.len(), 19, "{made}");
    assert_eq!(t.find(&gift, &made, None).await.unwrap(), vid(&second["account"]));
    assert!(t.issue(&gift, "1", None, Some(code), None, None).await.unwrap_err().to_string().contains("already"), "one code, one account");

    // Never below nothing, and the refusal says what it holds.
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    let e = stored_value::adjust(&t.k, &t.ctx, &mut tx, a1, n("-600"), "count").await.unwrap_err();
    assert!(e.to_string().contains("holds 500"), "{e}");
    drop(tx);
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    let adj = stored_value::adjust(&t.k, &t.ctx, &mut tx, a1, n("-100"), "keyed twice").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(t.account(a1).await, (n("400"), n("400"), "Active".into()));
    assert_eq!(t.posted(vid(&adj["entry"])).await, map(&[("Administrative Expenses", "-100"), ("Gift Cards", "100")]));

    // The ledger is append-only.
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    assert!(sqlx::query("update stored_value_entry set amount = 1 where tenant_id = $1").bind(tenant).execute(&mut *tx).await.is_err());
    drop(tx);

    // Done once under a key; the same key asking something else is refused.
    let once = t.issue(&gift, "50", None, None, None, Some("till-7:sale-1")).await.unwrap();
    let again = t.issue(&gift, "50", None, None, None, Some("till-7:sale-1")).await.unwrap();
    assert_eq!((again["account"].clone(), again["repeated"].clone()), (once["account"].clone(), json!(true)));
    assert!(t.issue(&gift, "60", None, None, None, Some("till-7:sale-1")).await.unwrap_err().to_string().contains("other input"));

    // A PIN: five wrong ones lock the card for a while, even to the right one.
    let customer = uid(&t.insert("Customer", json!({"customer_name": "Asha"})).await);
    assert!(t.issue(&pinned, "100", Some(customer), Some("STORECREDIT-0001"), None, None).await.unwrap_err().to_string().contains("PIN"));
    let credit = vid(&t.issue(&pinned, "100", Some(customer), Some("STORECREDIT-0001"), Some("2468"), None).await.unwrap()["account"]);
    assert_eq!(t.find(&pinned, "STORECREDIT-0001", Some("2468")).await.unwrap(), credit);
    assert!(t.find(&pinned, "STORECREDIT-0001", None).await.unwrap_err().to_string().contains("needs its PIN"));
    assert!(t.find(&gift, "STORECREDIT-0001", Some("2468")).await.is_err(), "a code of another kind is not this kind's");
    for _ in 0..4 {
        assert!(t.find(&pinned, "STORECREDIT-0001", Some("0000")).await.unwrap_err().to_string().contains("wrong"));
    }
    assert!(matches!(t.find(&pinned, "STORECREDIT-0001", Some("2468")).await, Err(ferrum_kernel::Error::TooManyRequests(_))));

    t.agrees().await;

    // ───────────── 3. selling and loading on a sales invoice ─────────────
    let output = account("Output Tax").await;
    let vat = t
        .insert("TaxTemplate", json!({"title": "VAT", "company": c, "kind": "Sales", "rows": [{"charge_type": "On Net Total", "account": output, "rate": "18", "description": "VAT"}]}))
        .await["id"]
        .clone();
    let evoucher = t
        .insert("StoredValueKind", json!({"kind_name": "E-Voucher", "company": c, "liability_account": liability, "tax_charged": "When Redeemed"}))
        .await["id"]
        .clone();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await["id"].clone();
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await["id"].clone();
    let item = |code: &'static str, rate: &'static str, more: Value| {
        let (t, products, nos) = (&t, products.clone(), nos.clone());
        async move {
            let mut v =
                obj(json!({"item_code": code, "item_name": code, "item_group": products, "stock_uom": nos, "standard_rate": rate, "is_stock_item": false}));
            v.extend(obj(more));
            t.insert("Item", Value::Object(v)).await["id"].clone()
        }
    };
    let card = item("Gift Card", "500", json!({"stored_value_kind": gift})).await;
    let voucher = item("E-Voucher", "300", json!({"stored_value_kind": evoucher})).await;
    let work = item("Work", "100", json!({})).await;
    let walk_in = t.insert("Customer", json!({"customer_name": "Walk-in"})).await["id"].clone();
    // Taxed when sold: the card's line is taxed like any other and goes to the liability, not income.
    let draft = t
        .insert(
            "SalesInvoice",
            json!({"company": c, "customer": walk_in, "tax_template": vat, "items": [
                {"item": card, "qty": "1", "stored_value_code": "GIFT-0000-0000-0001"}, {"item": work, "qty": "1"}]}),
        )
        .await;
    let line = &draft["items"][0];
    assert_eq!(
        (line["stored_value_code"].as_str(), line["income_account"].clone()),
        (Some("••••0001"), liability.clone()),
        "the code is hidden before it is stored"
    );
    assert!(!serde_json::to_string(&draft).unwrap().contains("GIFT000000000001") && !serde_json::to_string(&draft).unwrap().contains("GIFT-0000"));
    let sale = t.submit("SalesInvoice", uid(&draft)).await;
    assert_eq!(t.posted(uid(&sale)).await, map(&[("Debtors", "708"), ("Gift Cards", "-500"), ("Sales", "-100"), ("Output Tax", "-108")]));
    let a3 = t.find(&gift, "GIFT-0000-0000-0001", None).await.unwrap();
    assert_eq!(t.account(a3).await, (n("500"), n("500"), "Active".into()));
    assert_eq!(t.get("StoredValueAccount", a3).await["holder"], Value::Null, "a bearer card names no one");
    // Taxed when redeemed: the voucher's line carries no tax.
    let sold = t.post("SalesInvoice", json!({"company": c, "customer": walk_in, "tax_template": vat, "items": [{"item": voucher, "qty": "1"}]})).await;
    assert_eq!(t.posted(uid(&sold)).await, map(&[("Debtors", "300"), ("Gift Cards", "-300")]));
    assert_eq!(dec(&sold["total_taxes"]), n("0"));
    // A top-up, cancelled while unused: the value comes off again.
    let top =
        t.post("SalesInvoice", json!({"company": c, "customer": walk_in, "items": [{"item": card, "qty": "1", "stored_value_account": a3.to_string()}]})).await;
    assert_eq!(t.account(a3).await, (n("1000"), n("1000"), "Active".into()));
    t.cancel("SalesInvoice", uid(&top)).await.unwrap();
    assert_eq!(t.account(a3).await, (n("500"), n("500"), "Active".into()));
    // Spent value cannot be taken back by cancelling the sale.
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    stored_value::adjust(&t.k, &t.ctx, &mut tx, a3, n("-50"), "used").await.unwrap();
    tx.commit().await.unwrap();
    let e = t.cancel("SalesInvoice", uid(&sale)).await.unwrap_err().to_string();
    assert!(e.contains("used since") && e.contains("holds 450"), "{e}");
    assert_eq!(t.get("SalesInvoice", uid(&sale)).await["docstatus"], json!(1), "still standing");
    // Not on a credit note.
    let e =
        t.k.insert(
            &t.ctx,
            "SalesInvoice",
            obj(json!({"company": c, "customer": walk_in, "is_return": true, "return_reason": "x", "items": [{"item": card, "qty": "-1"}]})),
        )
        .await;
    assert!(e.unwrap_err().to_string().contains("not credited"));
    // Units: a ten-class pack, held by the customer who buys it, valued at what was paid.
    let yoga = item("Yoga Class", "100", json!({})).await;
    let classes = t
        .insert(
            "StoredValueKind",
            json!({"kind_name": "Classes", "company": c, "holds": "Units", "unit_name": "class", "items": [{"item": yoga}],
            "held_by": "Customer", "liability_account": packages, "income_account": income, "tax_charged": "When Redeemed"}),
        )
        .await["id"]
        .clone();
    let pack = item("Ten Classes", "900", json!({"stored_value_kind": classes, "stored_value_units": "10"})).await;
    let bought =
        t.post("SalesInvoice", json!({"company": c, "customer": customer.to_string(), "tax_template": vat, "items": [{"item": pack, "qty": "1"}]})).await;
    assert_eq!(t.posted(uid(&bought)).await, map(&[("Debtors", "900"), ("Prepaid Packages", "-900")]));
    let a4 = t.account_of(uid(&bought)).await;
    assert_eq!(t.account(a4).await, (n("10"), n("900"), "Active".into()));
    assert_eq!(t.get("StoredValueAccount", a4).await["holder"], json!(customer.to_string()));
    t.agrees().await;

    // ───────────── 4. redeeming as a payment ─────────────
    let bill = |who: &Value, item: &Value, qty: &str| json!({"company": c, "customer": who, "items": [{"item": item, "qty": qty}]});
    let pay = |who: &Value, inv: &Doc, amount: &str, more: Value| {
        let mut v = obj(json!({"company": c, "payment_type": "Receive", "customer": who, "paid_amount": amount,
            "references": [{"invoice": inv["id"], "allocated_amount": amount}]}));
        v.extend(obj(more));
        Value::Object(v)
    };
    // In part, by code: what is left stays on the card, and the liability takes the bank's place.
    let i1 = t.post("SalesInvoice", bill(&walk_in, &work, "3")).await;
    let p1 = t.post("PaymentEntry", pay(&walk_in, &i1, "300", json!({"stored_value_code": "GIFT-0000-0000-0001"}))).await;
    assert_eq!((p1["stored_value_account"].clone(), p1["stored_value_code"].as_str()), (json!(a3.to_string()), Some("••••0001")));
    assert_eq!(t.posted(uid(&p1)).await, map(&[("Gift Cards", "300"), ("Debtors", "-300")]));
    assert_eq!(t.account(a3).await, (n("150"), n("150"), "Active".into()));
    assert_eq!(dec(&t.get("SalesInvoice", uid(&i1)).await["outstanding_amount"]), n("0"));
    // Two tills take from one card at once: one wins, the other is told what is left.
    let (i2, i3) = (t.post("SalesInvoice", bill(&walk_in, &work, "3")).await, t.post("SalesInvoice", bill(&walk_in, &work, "3")).await);
    let d2 = t.insert("PaymentEntry", pay(&walk_in, &i2, "300", json!({"stored_value_account": a1.to_string()}))).await;
    let d3 = t.insert("PaymentEntry", pay(&walk_in, &i3, "300", json!({"stored_value_account": a1.to_string()}))).await;
    let (r2, r3) = tokio::join!(t.k.submit(&t.ctx, "PaymentEntry", uid(&d2), None), t.k.submit(&t.ctx, "PaymentEntry", uid(&d3), None));
    let (won, lost) = match (r2, r3) {
        (Ok(w), Err(l)) | (Err(l), Ok(w)) => (w, l),
        other => panic!("exactly one wins: {other:?}"),
    };
    assert!(lost.to_string().contains("holds 100"), "{lost}");
    assert_eq!(t.account(a1).await, (n("100"), n("100"), "Active".into()));
    // Cancelling the payment puts the value back.
    t.cancel("PaymentEntry", uid(&won)).await.unwrap();
    assert_eq!(t.account(a1).await, (n("400"), n("400"), "Active".into()));
    // Units pay for their items' lines, at what they were bought for: 90 a class, priced at 100.
    let i4 = t.post("SalesInvoice", bill(&json!(customer.to_string()), &yoga, "2")).await;
    let p4 =
        t.post("PaymentEntry", pay(&json!(customer.to_string()), &i4, "200", json!({"stored_value_account": a4.to_string(), "stored_value_units": "2"}))).await;
    assert_eq!(t.posted(uid(&p4)).await, map(&[("Prepaid Packages", "180"), ("Other Income", "20"), ("Debtors", "-200")]));
    assert_eq!(t.account(a4).await, (n("8"), n("720"), "Active".into()));
    let i5 = t.post("SalesInvoice", bill(&json!(customer.to_string()), &yoga, "1")).await;
    let e =
        t.k.insert(
            &t.ctx,
            "PaymentEntry",
            obj(pay(&json!(customer.to_string()), &i5, "100", json!({"stored_value_account": a4.to_string(), "stored_value_units": "2"}))),
        )
        .await;
    assert!(e.unwrap_err().to_string().contains("1 of Classes"), "units pay only the lines of their items");
    // A refund of what a card paid goes back onto it.
    let cn = t
        .post("SalesInvoice", json!({"company": c, "customer": walk_in, "is_return": true, "return_against": i1["id"], "items": [{"item": work, "qty": "-3"}]}))
        .await;
    let refund = t.k.run_action(&t.ctx, "SalesInvoice", uid(&cn), "make_refund", json!({})).await.unwrap();
    let rf = t.get("PaymentEntry", vid(&refund["id"])).await;
    assert_eq!(rf["stored_value_account"], json!(a3.to_string()));
    let rf = t.submit("PaymentEntry", uid(&rf)).await;
    assert_eq!(t.posted(uid(&rf)).await, map(&[("Debtors", "300"), ("Gift Cards", "-300")]));
    assert_eq!(t.account(a3).await, (n("450"), n("450"), "Active".into()));
    // A return without its receipt, refunded onto new store credit, then spent on an exchange.
    let exchange =
        t.insert("StoredValueKind", json!({"kind_name": "Exchange Credit", "company": c, "held_by": "Customer", "liability_account": liability})).await["id"]
            .clone();
    let back = t
        .post(
            "SalesInvoice",
            json!({"company": c, "customer": walk_in, "is_return": true, "return_reason": "no receipt", "items": [{"item": work, "qty": "-1"}]}),
        )
        .await;
    let refund = t.k.run_action(&t.ctx, "SalesInvoice", uid(&back), "make_refund", json!({"store_credit": exchange})).await.unwrap();
    let rf = t.submit("PaymentEntry", vid(&refund["id"])).await;
    let a5 = t.account_of(uid(&rf)).await;
    assert_eq!(t.account(a5).await, (n("100"), n("100"), "Active".into()));
    assert_eq!(t.get("StoredValueAccount", a5).await["holder"], walk_in);
    let swap = t.post("SalesInvoice", bill(&walk_in, &work, "1.5")).await;
    let paid = t.post("PaymentEntry", pay(&walk_in, &swap, "100", json!({"stored_value_account": a5.to_string()}))).await;
    assert_eq!(t.posted(uid(&paid)).await, map(&[("Gift Cards", "100"), ("Debtors", "-100")]));
    assert_eq!(
        (t.account(a5).await.2, dec(&t.get("SalesInvoice", uid(&swap)).await["outstanding_amount"])),
        ("Used Up".into(), n("50")),
        "the difference is paid as usual"
    );
    // Unused value recognised over time: 10% expected unused, recognised with each use, taken back if used after all.
    let promo = t
        .insert(
            "StoredValueKind",
            json!({"kind_name": "Promo Card", "company": c, "liability_account": liability, "adjustment_account": goodwill,
            "income_account": income, "on_expiry": "Income Over Time", "breakage_percent": "10"}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let a6 = vid(&t.issue(&promo, "100", None, None, None, None).await.unwrap()["account"]);
    let i6 = t.post("SalesInvoice", bill(&walk_in, &work, "0.45")).await;
    let p6 = t.post("PaymentEntry", pay(&walk_in, &i6, "45", json!({"stored_value_account": a6.to_string()}))).await;
    assert_eq!(t.posted(uid(&p6)).await, map(&[("Gift Cards", "50"), ("Other Income", "-5"), ("Debtors", "-45")]));
    assert_eq!(t.account(a6).await, (n("55"), n("50"), "Active".into()));
    let i7 = t.post("SalesInvoice", bill(&walk_in, &work, "0.55")).await;
    let p7 = t.post("PaymentEntry", pay(&walk_in, &i7, "55", json!({"stored_value_account": a6.to_string()}))).await;
    assert_eq!(t.posted(uid(&p7)).await, map(&[("Gift Cards", "50"), ("Other Income", "5"), ("Debtors", "-55")]), "all of it used: the share comes back");
    assert_eq!(t.account(a6).await, (n("0"), n("0"), "Used Up".into()));
    t.agrees().await;
    // ───────────── 5. expiry and unused value, by the app's upkeep ─────────────
    let today = chrono::Utc::now().date_naive();
    let days = |d: i64| today + chrono::Duration::days(d);
    let seasonal = t
        .insert(
            "StoredValueKind",
            json!({"kind_name": "Seasonal", "company": c, "liability_account": liability, "adjustment_account": goodwill, "income_account": income,
            "expiry": "From Issue", "expiry_days": 30, "warn_days": 7, "on_expiry": "Income at Expiry"}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let kept = t
        .insert(
            "StoredValueKind",
            json!({"kind_name": "Kept", "company": c, "liability_account": liability, "adjustment_account": goodwill,
            "expiry": "From Last Use", "expiry_days": 10}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let a7 = vid(&t.issue(&seasonal, "80", None, None, None, None).await.unwrap()["account"]);
    assert_eq!(t.get("StoredValueAccount", a7).await["expires_on"], json!(days(30)));
    let a8 = vid(&t.issue(&kept, "40", None, None, None, None).await.unwrap()["account"]);
    assert_eq!(t.get("StoredValueAccount", a8).await["expires_on"], json!(days(10)));
    // From last use: a use moves the expiry on.
    let i8 = t.post("SalesInvoice", bill(&walk_in, &work, "0.1")).await;
    t.post("PaymentEntry", pay(&walk_in, &i8, "10", json!({"stored_value_account": a8.to_string(), "posting_date": days(5)}))).await;
    assert_eq!(t.get("StoredValueAccount", a8).await["expires_on"], json!(days(15)));
    let events = |t: &T| {
        let k = t.k.clone();
        async move {
            let mut tx = k.db.system_tx().await.unwrap();
            let n: i64 = sqlx::query_scalar("select count(*) from outbox where topic = 'accounting.stored_value_expiring' and record_id = $1")
                .bind(a7)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            n
        }
    };
    // Warned once, seven days before; the card kept from last use expired after day 15.
    assert_eq!(stored_value::upkeep(&t.k, &t.ctx, days(22)).await.unwrap(), (0, 1));
    assert_eq!(stored_value::upkeep(&t.k, &t.ctx, days(23)).await.unwrap(), (1, 0));
    assert_eq!(stored_value::upkeep(&t.k, &t.ctx, days(24)).await.unwrap(), (0, 0));
    assert_eq!(events(&t).await, 1);
    // Expired once, after its last day: what is left becomes income; what stays owed stays owed.
    assert_eq!(stored_value::upkeep(&t.k, &t.ctx, days(31)).await.unwrap(), (0, 1));
    assert_eq!(t.account(a7).await, (n("0"), n("0"), "Expired".into()));
    assert_eq!(t.account(a8).await, (n("30"), n("30"), "Expired".into()));
    assert_eq!(stored_value::upkeep(&t.k, &t.ctx, days(32)).await.unwrap().1, 0, "exactly once");
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    let income_at_expiry: Decimal = sqlx::query_scalar(
        "select coalesce(sum(l.credit - l.debit), 0) from journal_line l join stored_value_entry e on e.tenant_id = l.tenant_id and e.journal_entry_id = l.entry_id \
         where l.tenant_id = $1 and e.account_id = $2 and e.entry_type = 'expire' and l.account_id = $3",
    )
    .bind(tenant)
    .bind(a7)
    .bind(vid(&income))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    drop(tx);
    assert_eq!(income_at_expiry, n("80"));
    let i9 = t.post("SalesInvoice", bill(&walk_in, &work, "0.1")).await;
    let e = t.k.insert(&t.ctx, "PaymentEntry", obj(pay(&walk_in, &i9, "10", json!({"stored_value_account": a8.to_string()})))).await;
    assert!(e.unwrap_err().to_string().contains("expired"));
    t.agrees().await;
    // ───────────── 6. reports ─────────────
    let report = |name: &'static str, params: Vec<(&'static str, String)>| {
        let t = &t;
        async move {
            let params: std::collections::HashMap<String, String> = params.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
            ferrum_domain::reports::run(&t.k, &t.ctx, name, &params).await.unwrap_or_else(|e| panic!("{name}: {e:?}"))
        }
    };
    let row = |r: &Value, kind: &str| r["rows"].as_array().unwrap().iter().find(|x| x["kind"] == json!(kind)).cloned().unwrap_or(Value::Null);
    let owed = report("stored_value_liability", vec![("company", c.clone()), ("as_of", days(40).to_string())]).await;
    // Gift Card: a1 400, a3 450 and the 200 and 50 issued by hand, all issued forty days before.
    let g = row(&owed, "Gift Card");
    assert_eq!((dec(&g["value"]), dec(&g["age_31_90"]), g["accounts"].clone()), (n("1100"), n("1100"), json!(4)), "{owed}");
    assert_eq!(dec(&row(&owed, "Classes")["balance"]), n("8"));
    assert_eq!(row(&owed, "Seasonal"), Value::Null, "nothing owed once expired into income");
    let span = vec![("company", c.clone()), ("from_date", today.to_string()), ("to_date", days(40).to_string())];
    let moved = report("stored_value_movement", span.clone()).await;
    let s7 = row(&moved, "Seasonal");
    assert_eq!((dec(&s7["issued"]), dec(&s7["expired"]), dec(&s7["net"])), (n("80"), n("80"), n("0")));
    let g = row(&moved, "Gift Card");
    // Issued 500 + 200 + 50 by hand, 500 sold and 500 topped up (then cancelled: a reversal counts against it).
    assert_eq!((dec(&g["issued"]), dec(&g["redeemed"]), dec(&g["refunded"])), (n("1250"), n("300"), n("300")), "{g}");
    let mut sp = span.clone();
    sp.push(("stored_value_account", a3.to_string()));
    let st = report("stored_value_statement", sp).await;
    let last = st["rows"].as_array().unwrap().last().unwrap().clone();
    assert_eq!((dec(&last["balance"]), dec(&st["totals"]["balance"])), (n("450"), n("450")));
    assert_eq!(st["rows"][1]["entry_type"], json!("issue"));
    t.agrees().await;
    // ───────────── 7. for other apps and configuration: the contracted capabilities ─────────────
    let catalog = t.k.catalog(&t.ctx);
    let cap = |id: &str| catalog.iter().find(|c| c.id == id).cloned().unwrap_or_else(|| panic!("{id} in the catalog"));
    assert_eq!(cap("accounting.stored_value_kind.redeem").compensate_with.as_deref(), Some("accounting.stored_value_kind.reverse"));
    assert!(!cap("accounting.stored_value_kind.balance").effect.changes());
    let gid: Uuid = gift.parse().unwrap();
    let run = || {
        let k = t.k.clone();
        async move { k.db.tenant_tx(tenant).await.unwrap() }
    };
    let mut tx = run().await;
    let bal = stored_value::balance(&t.k, &t.ctx, &mut tx, gid, "GIFT-0000-0000-0001", None).await.unwrap();
    assert_eq!((bal["account"].clone(), dec(&bal["balance"]), bal["state"].clone()), (json!(a3.to_string()), n("450"), json!("Active")));
    drop(tx);
    // Redeemed once under a key, whatever the retries; then undone.
    let i10 = t.post("SalesInvoice", bill(&walk_in, &work, "1")).await;
    let mut tx = run().await;
    let r = stored_value::redeem(&t.k, &t.ctx, &mut tx, gid, "GIFT-0000-0000-0001", None, uid(&i10), None, None, "shop:order-10").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!((dec(&r["amount"]), dec(&r["balance"])), (n("100"), n("350")));
    let mut tx = run().await;
    let again = stored_value::redeem(&t.k, &t.ctx, &mut tx, gid, "GIFT-0000-0000-0001", None, uid(&i10), None, None, "shop:order-10").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!((again["repeated"].clone(), t.account(a3).await.0), (json!(true), n("350")));
    let mut tx = run().await;
    stored_value::reverse(&t.k, &t.ctx, &mut tx, gid, vid(&r["entry"]), "shop:order-10:undo").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(t.account(a3).await.0, n("450"));
    assert_eq!(t.get("PaymentEntry", vid(&r["payment"])).await["docstatus"], json!(2), "undone by cancelling its payment");
    // A return without its receipt onto new store credit: its code is handed back once.
    let note = t
        .post(
            "SalesInvoice",
            json!({"company": c, "customer": walk_in, "is_return": true, "return_reason": "no receipt", "items": [{"item": work, "qty": "-1"}]}),
        )
        .await;
    let mut tx = run().await;
    let back = stored_value::refund(&t.k, &t.ctx, &mut tx, vid(&exchange), uid(&note), None, None, None, "till-2:return-9").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(dec(&back["balance"]), n("100"));
    assert_eq!(t.find(exchange.as_str().unwrap(), back["code"].as_str().unwrap(), None).await.unwrap(), vid(&back["account"]));
    // Transferred where the kind allows it, in part, to a new card; then undone. Cashed out.
    let shared = t
        .insert(
            "StoredValueKind",
            json!({"kind_name": "Shared", "company": c, "liability_account": liability, "adjustment_account": goodwill,
            "can_transfer": true, "can_cash_out": true}),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let sid: Uuid = shared.parse().unwrap();
    t.issue(&shared, "100", None, Some("SHARED-CARD-0001"), None, None).await.unwrap();
    let mut tx = run().await;
    let e = stored_value::transfer(&t.k, &t.ctx, &mut tx, gid, "GIFT-0000-0000-0001", None, None, None, None, "x").await.unwrap_err();
    assert!(e.to_string().contains("cannot be transferred"), "{e}");
    drop(tx);
    let mut tx = run().await;
    let moved = stored_value::transfer(&t.k, &t.ctx, &mut tx, sid, "SHARED-CARD-0001", None, None, None, Some(n("40")), "gift-to-friend").await.unwrap();
    tx.commit().await.unwrap();
    let (s1, s2) = (vid(&moved["account"]), vid(&moved["to_account"]));
    assert_eq!((t.account(s1).await.0, t.account(s2).await.0), (n("60"), n("40")));
    assert_eq!(t.posted(vid(&moved["entry"])).await, map(&[]), "one liability on both sides: nothing posted");
    let mut tx = run().await;
    stored_value::reverse(&t.k, &t.ctx, &mut tx, sid, vid(&moved["entry"]), "gift-to-friend:undo").await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!((t.account(s1).await.0, t.account(s2).await.0), (n("100"), n("0")));
    let cash = account("Cash").await;
    let out = t.k.run_action(&t.ctx, "StoredValueAccount", s1, "cash_out", json!({"amount": "60", "paid_from": cash})).await.unwrap();
    assert_eq!(t.posted(vid(&out["entry"])).await, map(&[("Gift Cards", "60"), ("Cash", "-60")]));
    assert!(t.k.run_action(&t.ctx, "StoredValueAccount", a3, "cash_out", json!({"paid_from": cash})).await.unwrap_err().to_string().contains("not cashed out"));
    // A card held by one customer does not pay another's invoice.
    let other = t.insert("Customer", json!({"customer_name": "Ravi"})).await["id"].clone();
    let i11 = t.post("SalesInvoice", bill(&other, &work, "1")).await;
    let e = t.k.insert(&t.ctx, "PaymentEntry", obj(pay(&other, &i11, "50", json!({"stored_value_account": vid(&back["account"]).to_string()})))).await;
    assert!(e.unwrap_err().to_string().contains("another customer"));
    // A new code, shown once: the old one stops working.
    let fresh = t.k.run_action(&t.ctx, "StoredValueAccount", s1, "new_code", json!({})).await.unwrap();
    assert!(t.find(&shared, "SHARED-CARD-0001", None).await.is_err());
    assert_eq!(t.find(&shared, fresh["code"].as_str().unwrap(), None).await.unwrap(), s1);
    t.agrees().await;

    // ───────────── 8. a code or PIN comes to rest nowhere ─────────────
    // Every operation with a card's code and PIN, then every table searched for them: the audit log,
    // the capability receipts, record history, the outbox and events, and whatever else is stored.
    let pin = "Pq-2468-z";
    let pid: Uuid = pinned.parse().unwrap();
    let asha = json!(customer.to_string());
    t.issue(&pinned, "80", Some(customer), Some("STORECREDIT-0002"), Some(pin), None).await.unwrap();
    let mut tx = run().await;
    let bal = stored_value::balance(&t.k, &t.ctx, &mut tx, pid, "STORECREDIT-0002", Some(pin)).await.unwrap();
    assert_eq!(dec(&bal["balance"]), n("80"));
    drop(tx);
    let i12 = t.post("SalesInvoice", bill(&asha, &work, "1")).await;
    let mut tx = run().await;
    stored_value::redeem(&t.k, &t.ctx, &mut tx, pid, "STORECREDIT-0002", Some(pin), uid(&i12), Some(n("10")), None, "pin:redeem").await.unwrap();
    tx.commit().await.unwrap();
    let i13 = t.post("SalesInvoice", bill(&asha, &work, "1")).await;
    t.post("PaymentEntry", pay(&asha, &i13, "10", json!({"stored_value_code": "STORECREDIT-0002", "stored_value_pin": pin}))).await;
    let note2 = t
        .post("SalesInvoice", json!({"company": c, "customer": asha, "is_return": true, "return_reason": "no receipt", "items": [{"item": work, "qty": "-1"}]}))
        .await;
    let mut tx = run().await;
    stored_value::refund(&t.k, &t.ctx, &mut tx, pid, uid(&note2), Some("STORECREDIT-0003"), Some(pin), None, "pin:refund").await.unwrap();
    tx.commit().await.unwrap();
    t.issue(&shared, "30", None, Some("SHARED-CARD-0002"), Some(pin), None).await.unwrap();
    let mut tx = run().await;
    let to_new = stored_value::transfer(&t.k, &t.ctx, &mut tx, sid, "SHARED-CARD-0002", Some(pin), None, None, Some(n("5")), "pin:transfer").await.unwrap();
    stored_value::transfer(&t.k, &t.ctx, &mut tx, sid, "SHARED-CARD-0002", Some(pin), fresh["code"].as_str(), None, Some(n("5")), "pin:transfer-2")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // A logged action that is handed a code or PIN refuses it before anything runs; a proof is used once,
    // for its one operation; the account's New Code takes a proof, or makes its code and PIN itself.
    let mut tx = run().await;
    let e = stored_value::call(&t.k, &t.ctx, &mut tx, pid, "redeem", json!({"code": "STORECREDIT-0002", "pin": pin, "invoice": uid(&i12)}), Some("leak"));
    assert!(e.await.is_err());
    drop(tx);
    let e = t.k.run_action(&t.ctx, "StoredValueKind", pid, "issue", json!({"amount": "1", "reason": "x", "pin": pin})).await.unwrap_err();
    assert!(e.to_string().contains("never passed"), "{e}");
    let proven = t.k.run_action(&t.ctx, "StoredValueKind", pid, "prove", json!({"operation": "redeem", "code": "STORECREDIT-0002", "pin": pin})).await.unwrap();
    let proof = proven["proof"].as_str().unwrap();
    // The code and PIN of prove (and balance) are secret parameters: the platform shows them as redacted wherever
    // the action is kept (the audit trail of a recorded run, an approval request), and out of its errors.
    let kind = t.ctx.schema.entity("StoredValueKind").unwrap().clone();
    let asked = json!({"operation": "redeem", "code": "STORECREDIT-0002", "pin": pin, "key": "k1"});
    for action in ["prove", "balance"] {
        let def = kind.def.actions.iter().find(|a| a.name == action).unwrap();
        assert_eq!(def.secret_values(&asked), vec!["STORECREDIT-0002".to_string(), pin.to_string()], "{action}'s code and PIN are secret parameters");
        let kept = def.redacted(&asked);
        assert_eq!((kept["code"].clone(), kept["pin"].clone(), kept["operation"].clone()), (json!("[redacted]"), json!("[redacted]"), json!("redeem")));
        assert!(!kept.to_string().contains("STORECREDIT-0002") && !kept.to_string().contains(pin));
    }
    // Run as a recorded action would be (a configured action's audit row is `def.redacted`), the audit trail holds none.
    let mut tx = run().await;
    ferrum_kernel::audit::record(
        &mut tx,
        &t.ctx,
        "StoredValueKind",
        pid,
        "action:prove",
        None,
        kind.def.actions.iter().find(|a| a.name == "prove").unwrap().redacted(&asked),
    )
    .await
    .unwrap();
    let rows: Vec<String> = sqlx::query_scalar("select diff::text from audit_event where action = 'action:prove'").fetch_all(&mut *tx).await.unwrap();
    assert!(!rows.is_empty() && rows.iter().all(|d| !d.contains("STORECREDIT-0002") && !d.contains(pin) && d.contains("[redacted]")), "{rows:?}");
    tx.rollback().await.unwrap();
    let i14 = t.post("SalesInvoice", bill(&asha, &work, "1")).await;
    let redeem = |proof: &str| json!({"proof": proof, "invoice": uid(&i14), "amount": "5"});
    let e = t.k.run_action(&t.ctx, "StoredValueKind", sid, "redeem", redeem(proof)).await.unwrap_err();
    assert!(e.to_string().contains("prove the code again"), "another kind's: {e}");
    t.k.run_action(&t.ctx, "StoredValueKind", pid, "redeem", redeem(proof)).await.unwrap();
    let e = t.k.run_action(&t.ctx, "StoredValueKind", pid, "redeem", redeem(proof)).await.unwrap_err();
    assert!(e.to_string().contains("used"), "once: {e}");
    let mut tx = run().await;
    sqlx::query("update stored_value_proof set expires_at = now() - interval '1 second' where tenant_id = $1").bind(tenant).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let pinned_acc = t.find(&pinned, "STORECREDIT-0002", Some(pin)).await.unwrap();
    let made_pin = t.k.run_action(&t.ctx, "StoredValueAccount", pinned_acc, "new_code", json!({})).await.unwrap();
    let (new_code, new_pin) = (made_pin["code"].as_str().unwrap(), made_pin["pin"].as_str().unwrap());
    assert_eq!(t.find(&pinned, new_code, Some(new_pin)).await.unwrap(), pinned_acc, "a kind that needs a PIN gets one made with its code");
    assert!(t.k.run_action(&t.ctx, "StoredValueAccount", pinned_acc, "new_code", json!({"pin": pin})).await.unwrap_err().to_string().contains("never passed"));
    let chosen = t.k.run_action(&t.ctx, "StoredValueKind", pid, "prove", json!({"operation": "new_code", "pin": pin})).await.unwrap();
    let mine = t.k.run_action(&t.ctx, "StoredValueAccount", pinned_acc, "new_code", json!({"proof": chosen["proof"]})).await.unwrap();
    assert_eq!(mine["code"], Value::Null, "the code came from prove, not again from here");
    assert_eq!(t.find(&pinned, chosen["code"].as_str().unwrap(), Some(pin)).await.unwrap(), pinned_acc);
    let mut secrets: Vec<String> =
        ["4000-1234-5678-9ABC", "STORECREDIT-0001", "STORECREDIT-0002", "STORECREDIT-0003", "GIFT-0000-0000-0001", "SHARED-CARD-0001", "SHARED-CARD-0002"]
            .iter()
            .map(|c| c.to_string())
            .collect();
    secrets.extend(
        [&second["code"], &back["code"], &fresh["code"], &to_new["code"], &made_pin["code"], &chosen["code"]]
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string),
    );
    secrets.extend(secrets.clone().iter().map(|c| c.replace('-', "")));
    secrets.push(pin.into());
    let like: Vec<String> = secrets.iter().map(|s| format!("%{s}%")).collect();
    let mut tx = t.k.db.pool.begin().await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true)").execute(&mut *tx).await.unwrap();
    let tables: Vec<String> =
        sqlx::query_scalar("select table_name::text from information_schema.tables where table_schema = 'public' and table_type = 'BASE TABLE' order by 1")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    let mut rests = Vec::new();
    for table in tables {
        let hits: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("select count(*) from \"{table}\" x where x::text ilike any($1)")))
            .bind(&like)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        if hits > 0 {
            rests.push(format!("{table}: {hits}"));
        }
    }
    assert_eq!(rests, Vec::<String>::new(), "a card's code or PIN rests in these tables");
    drop(tx);

    // ───────────── 9. what 0.20.0 left behind is cleared, and nothing else ─────────────
    let mut tx = run().await;
    let issued = json!({"amount": "5", "reason": "x", "code": "OLD-LEAK-CODE-0001", "pin": "Pq-old-pin"});
    ferrum_kernel::audit::record(&mut tx, &t.ctx, "StoredValueKind", gid, "action:issue", None, issued).await.unwrap();
    ferrum_kernel::audit::record(
        &mut tx,
        &t.ctx,
        "StoredValueKind",
        gid,
        "action:transfer",
        None,
        json!({"code": "OLD-LEAK-CODE-0001", "to_code": "OLD-LEAK-CODE-0002", "amount": null}),
    )
    .await
    .unwrap();
    ferrum_kernel::audit::record(&mut tx, &t.ctx, "StoredValueKind", gid, "action:reverse", None, json!({"entry": "kept-entry"})).await.unwrap();
    ferrum_kernel::audit::record(&mut tx, &t.ctx, "Customer", customer, "action:other", None, json!({"pin": "Pq-another-app"})).await.unwrap();
    sqlx::query(
        "insert into logic_effect (tenant_id, environment, capability, effect_key, input_digest, version, capability_digest, result) \
         values ($1, 'production', 'accounting.stored_value_kind.issue', 'old-0.20.0', 'd', 'v', 'c', '{\"account\": \"kept-account\", \"code\": \"OLD-MADE-CODE-0003\"}')",
    )
    .bind(tenant)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let clean_up = include_str!("../packages/accounting/sql/006_stored_value_logs_cleared.sql");
    let mut tx = t.k.db.pool.begin().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(clean_up.to_string())).execute(&mut *tx).await.unwrap();
    sqlx::query("select set_config('ferrum.system', 'on', true)").execute(&mut *tx).await.unwrap();
    let left: Vec<String> = sqlx::query_scalar(
        "select diff::text from audit_event where tenant_id = $1 and diff::text ~ '(OLD-LEAK|Pq-old|OLD-MADE|kept-entry|Pq-another-app)' \
         union all select result::text from logic_effect where tenant_id = $1 and result::text ~ '(OLD-MADE|kept-account)' order by 1",
    )
    .bind(tenant)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(left.len(), 3, "the other parameters and other apps' events stay: {left:?}");
    assert!(left.iter().all(|l| !l.contains("OLD-") && !l.contains("Pq-old")), "{left:?}");
    let note: Value = sqlx::query_scalar(
        "select redacted from audit_event where tenant_id = $1 and action = 'action:transfer' and diff ? 'to_code' order by id desc limit 1",
    )
    .bind(tenant)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(note[0]["fields"], json!(["code", "to_code"]), "the redaction is recorded: {note}");
    assert_eq!(ferrum_kernel::audit::verify_record(&mut tx, tenant, "StoredValueKind", gid).await.unwrap(), None, "the chain still holds");
}
