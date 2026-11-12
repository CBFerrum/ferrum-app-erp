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
}
