//! Which income or expense account a line books to, against a real PostgreSQL
//! (`FERRUM_ERP_ACCOUNTS_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_accounts_test`; reset first).
//!
//! The line's own account, then the item's, then its item group's or the nearest parent group's,
//! then the company default; a stock item bought still waits on stock received but not billed.

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
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let page =
            self.k.list(&self.ctx, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() }).await.unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
    /// What a document posted, by account name: debit minus credit.
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
        rows.into_iter().collect()
    }
}

#[test]
fn item_group_accounts() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_ACCOUNTS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_accounts_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    let k: Arc<Kernel> = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "ga".into(),
            name: "Group Accounts Co".into(),
            admin_email: "admin@ga.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying"].iter().map(|s| s.to_string()).collect(),
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("ga", "admin@ga.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), ctx };

    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let cid = uid(&t.insert("Company", json!({"company_name": "Group Accounts Co", "abbr": "GA", "default_currency": inr["id"]})).await);
    let c = cid.to_string();
    let account = |name: &'static str| {
        let t = &t;
        let c = c.clone();
        async move { t.find_one("Account", json!({"account_name": name, "company": c})).await["id"].as_str().unwrap().to_string() }
    };
    let (service_income, other_income, sales, rent, srbnb) = (
        account("Service Income").await,
        account("Other Income").await,
        account("Sales").await,
        account("Rent").await,
        account("Stock Received But Not Billed").await,
    );
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;

    // Advisory carries the accounts; Consulting Work, under it, has none; Training Courses, under it too, its own income.
    let services = t
        .insert("ItemGroup", json!({"group_name": "Advisory", "is_group": true, "default_income_account": service_income, "default_expense_account": rent}))
        .await["id"]
        .clone();
    let consulting = t.insert("ItemGroup", json!({"group_name": "Consulting Work", "parent_group": services})).await["id"].clone();
    let training =
        t.insert("ItemGroup", json!({"group_name": "Training Courses", "parent_group": services, "default_income_account": other_income})).await["id"].clone();
    let item = |code: &str, group: &Value, more: Value| {
        let mut v = json!({"item_code": code, "item_name": code, "item_group": group, "stock_uom": nos["id"], "standard_rate": "100", "is_stock_item": false});
        v.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap());
        v
    };
    let advice = uid(&t.insert("Item", item("Advice", &consulting, json!({}))).await).to_string();
    let course = uid(&t.insert("Item", item("Course", &training, json!({}))).await).to_string();
    let audit = uid(&t.insert("Item", item("Audit", &consulting, json!({"income_account": sales}))).await).to_string();
    let bolts = uid(&t.insert("Item", item("Bolts", &consulting, json!({"is_stock_item": true}))).await).to_string();
    let customer = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let supplier = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await).to_string();
    let accounts =
        |doc: &Doc, field: &str| doc["items"].as_array().unwrap().iter().map(|r| r[field].as_str().unwrap_or("none").to_string()).collect::<Vec<_>>();

    // An invoice: the parent group's (Consulting Work has none), the nearest group's, the item's own,
    // a line's own, a line without an item.
    let si = t
        .insert(
            "SalesInvoice",
            json!({"company": c, "customer": customer, "items": [
                {"item": advice, "qty": "1"},
                {"item": course, "qty": "1"},
                {"item": audit, "qty": "1"},
                {"item": advice, "qty": "1", "income_account": sales},
                {"description": "Late fee", "qty": "1", "price_list_rate": "100"}]}),
        )
        .await;
    assert_eq!(accounts(&si, "income_account"), vec![service_income.clone(), other_income.clone(), sales.clone(), sales.clone(), sales.clone()]);
    t.submit("SalesInvoice", uid(&si)).await;
    let posted = t.posted(uid(&si)).await;
    let n = |s: &str| Decimal::from_str(s).unwrap();
    assert_eq!((posted["Service Income"], posted["Other Income"], posted["Sales"]), (n("-100"), n("-100"), n("-300")));

    // Buying: a service from its group's expense account; a stock item still waits on SRBNB.
    let pi = t
        .insert(
            "PurchaseInvoice",
            json!({"company": c, "supplier": supplier, "bill_no": "ACME/1", "items": [{"item": advice, "qty": "1", "price_list_rate": "40"}, {"item": bolts, "qty": "1", "price_list_rate": "40"}]}),
        )
        .await;
    assert_eq!(accounts(&pi, "expense_account"), vec![rent.clone(), srbnb.clone()]);
}
