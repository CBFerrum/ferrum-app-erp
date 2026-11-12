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
        let out = stored_value::issue(&self.k, &self.ctx, &mut tx, kind.parse().unwrap(), n(amount), "goodwill", holder, code, pin, key).await?;
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
    assert!(t.issue(&gift, "60", None, None, None, Some("till-7:sale-1")).await.unwrap_err().to_string().contains("another"));

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
}
