//! A company switching books, end to end against a real PostgreSQL
//! (`FERRUM_ERP_SWITCHING_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_switching_test`; reset first).
//!
//! What a company coming from Tally, Zoho Books, QuickBooks or ERPNext does in its first weeks:
//! invoices with lines that are no item (rent, an electricity bill, an installation), credited in
//! part and in full; an order with a service line billed in two parts; its open invoices carried in
//! from a spreadsheet in two currencies, aged by their own due dates, paid in part, settled with an
//! exchange difference, an opening credit refunded; a journal entry's party line with its own due
//! date and currency; money moved between its own bank and cash accounts. Each step checks the
//! ledger lines, what parties owe and the reports' figures.

use chrono::Duration;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Result};
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

fn dec(v: &Value) -> Decimal {
    match v {
        Value::String(s) => Decimal::from_str(s).unwrap_or_else(|_| panic!("not a decimal: {s}")),
        Value::Number(n) => Decimal::from_str(&n.to_string()).unwrap(),
        Value::Null => Decimal::ZERO,
        other => panic!("not a decimal: {other}"),
    }
}

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

fn vid(v: &Value) -> Uuid {
    v["id"].as_str().unwrap().parse().unwrap()
}

/// The refusal's words contain `words`.
fn refused<T: std::fmt::Debug>(r: Result<T>, words: &str) {
    let text = match &r {
        Err(Error::Invalid(m)) => m.clone(),
        Err(Error::Validation(e)) => e.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => panic!("expected a refusal with {words:?}, got {other:?}"),
    };
    assert!(text.contains(words), "{words:?} not in {text:?}");
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
    async fn update(&self, entity: &str, id: Uuid, v: Value) -> Doc {
        self.k.update(&self.ctx, entity, id, obj(v), None).await.unwrap_or_else(|e| panic!("update {entity}: {e:?}"))
    }
    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn make(&self, entity: &str, v: Value) -> Doc {
        let d = self.insert(entity, v).await;
        self.submit(entity, uid(&d)).await
    }
    async fn cancel(&self, entity: &str, id: Uuid) {
        self.k.cancel(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("cancel {entity}: {e:?}"));
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.ctx, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    async fn action(&self, entity: &str, id: Uuid, action: &str, params: Value) -> Uuid {
        let v = self.k.run_action(&self.ctx, entity, id, action, params).await.unwrap_or_else(|e| panic!("{entity}.{action}: {e:?}"));
        vid(&v)
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let page =
            self.k.list(&self.ctx, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() }).await.unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
    async fn scalar(&self, sql: &'static str, a: Uuid) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        let v: Option<Decimal> = sqlx::query_scalar(sql).bind(self.ctx.tenant).bind(a).fetch_one(&mut *tx).await.unwrap();
        v.unwrap_or_default()
    }
    /// Debit minus credit on an account (by name), all postings.
    async fn gl(&self, account: &str) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        sqlx::query_scalar(
            "select coalesce(sum(l.debit - l.credit), 0) from journal_line l join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
             where l.tenant_id = $1 and a.account_name = $2",
        )
        .bind(self.ctx.tenant)
        .bind(account)
        .fetch_one(&mut *tx)
        .await
        .unwrap()
    }
    /// What a document posted, by account name: debit minus credit, company currency, reversals included.
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
    /// What a customer owes (positive) or is owed (negative), in the open items' own currencies.
    async fn owes(&self, customer: Uuid) -> Decimal {
        self.scalar("select sum(outstanding) from open_item where tenant_id = $1 and customer_id = $2 and status <> 'cancelled'", customer).await
    }
    /// An open item's number, due date, currency, amount and company-currency amount.
    async fn open_item(&self, id: Uuid) -> (Option<String>, Option<chrono::NaiveDate>, String, Decimal, Decimal) {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        sqlx::query_as("select number, due_date, trim(currency), amount, base_amount from open_item where tenant_id = $1 and id = $2")
            .bind(self.ctx.tenant)
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .unwrap()
    }
    async fn report(&self, name: &str, params: &[(&str, String)]) -> Value {
        let p: HashMap<String, String> = params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        ferrum_domain::reports::run(&self.k, &self.ctx, name, &p).await.unwrap_or_else(|e| panic!("report {name}: {e:?}"))
    }
    fn line(&self, doc: &Doc, i: usize) -> Doc {
        doc["items"][i].as_object().cloned().unwrap()
    }
}

/// An ageing report's buckets: not due, 1-30, 31-60, 61-90, over 90, and the total.
fn buckets(r: &Value) -> [Decimal; 6] {
    let t = &r["totals"];
    ["not_due", "d1_30", "d31_60", "d61_90", "d90_plus", "outstanding_base"].map(|b| dec(&t[b]))
}

#[test]
fn switching_books() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_SWITCHING_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_switching_test".into());
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
            slug: "sw".into(),
            name: "Switching Co".into(),
            admin_email: "admin@sw.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying"].iter().map(|s| s.to_string()).collect(),
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("sw", "admin@sw.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), ctx };
    let today = t.ctx.today();
    let day = |offset: i64| (today + Duration::days(offset)).to_string();

    // ───────────── a company, its parties, items, accounts and taxes ─────────────
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let usd = t.find_one("Currency", json!({"code": "USD"})).await;
    let cid = uid(&t.insert("Company", json!({"company_name": "Switching Co", "abbr": "SC", "default_currency": inr["id"]})).await);
    let c = cid.to_string();
    let stores = uid(&t.find_one("Warehouse", json!({"warehouse_name": "Stores", "company": c})).await).to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |code: &str, more: Value| {
        let mut v = json!({"item_code": code, "item_name": code, "item_group": products["id"], "stock_uom": nos["id"]});
        v.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap());
        v
    };
    let widget = uid(&t.insert("Item", item("Widget", json!({"standard_rate": "150"}))).await).to_string();
    let advice = uid(&t.insert("Item", item("Advice", json!({"standard_rate": "100", "is_stock_item": false}))).await).to_string();
    let party = |entity: &'static str, name: &'static str, more: Value| {
        let t = &t;
        async move {
            let field = if entity == "Customer" { "customer_name" } else { "supplier_name" };
            let mut v = json!({field: name});
            v.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap());
            uid(&t.insert(entity, v).await).to_string()
        }
    };
    let globex = party("Customer", "Globex", json!({})).await;
    let umbrella = party("Customer", "Umbrella", json!({})).await;
    let hooli = party("Customer", "Hooli", json!({})).await;
    let initech = party("Customer", "Initech", json!({"default_currency": usd["id"]})).await;
    let acme = party("Supplier", "Acme", json!({})).await;
    let power = party("Supplier", "Power Co", json!({})).await;
    let wayne = party("Supplier", "Wayne", json!({})).await;
    let account = |name: &'static str| {
        let t = &t;
        let c = c.clone();
        async move { t.find_one("Account", json!({"account_name": name, "company": c})).await["id"].as_str().unwrap().to_string() }
    };
    let (debtors, obe, bank, cash) = (account("Debtors").await, account("Opening Balance Equity").await, account("Primary Bank").await, account("Cash").await);
    let (other_income, utilities, output) = (account("Other Income").await, account("Utilities").await, account("Output Tax").await);
    let factory = uid(&t.insert("CostCenter", json!({"cost_center_name": "Factory", "company": c})).await);
    let inclusive = uid(&t
        .insert(
            "TaxTemplate",
            json!({"title": "VAT 10% included", "company": c, "kind": "Sales",
                   "rows": [{"charge_type": "On Net Total", "account": output, "description": "VAT", "rate": "10", "included_in_price": true}]}),
        )
        .await)
    .to_string();
    // A rule that would discount anything Globex buys, unless the line says its own discount.
    t.insert("PricingRule", json!({"title": "Globex 5%", "apply_on": "All Items", "customer": globex, "discount_percent": "5"})).await;
    let se = t
        .insert(
            "StockEntry",
            json!({"company": c, "purpose": "Material Receipt", "items": [{"item": widget, "qty": "10", "basic_rate": "100", "target_warehouse": stores}]}),
        )
        .await;
    t.submit("StockEntry", uid(&se)).await;

    // ───────────── a sale with a line that is no item, beside one that is, tax included ─────────────
    let si = t
        .make(
            "SalesInvoice",
            json!({"company": c, "customer": globex, "tax_template": inclusive, "items": [
                {"item": advice, "qty": "2", "discount_percent": "0"},
                {"description": "Rent for May", "qty": "1", "price_list_rate": "1100", "income_account": other_income}]}),
        )
        .await;
    let si_id = uid(&si);
    let rent = t.line(&si, 1);
    assert_eq!(
        (rent["item_name"].clone(), dec(&rent["price_list_rate"]), dec(&rent["discount_percent"]), dec(&rent["net_amount"])),
        (json!("Rent for May"), n("1100"), Decimal::ZERO, n("1000")),
        "named by its description, its own price, no pricing rule, tax taken out of it"
    );
    assert_eq!(rent["cost_center"], t.get("Company", cid).await["default_cost_center"], "the company's cost centre");
    assert_eq!((dec(&si["grand_total"]), dec(&si["total_taxes"])), (n("1300"), n("118.18")));
    assert_eq!(
        t.posted(si_id).await,
        map(&[("Debtors", "1300"), ("Other Income", "-1000"), ("Sales", "-181.82"), ("Output Tax", "-118.18")]),
        "the rent to the account it names, the advice to its default"
    );
    assert_eq!(t.scalar("select count(*)::numeric from stock_move where tenant_id = $1 and source_id = $2", si_id).await, Decimal::ZERO);
    // Credited in part (the rent alone), then in full (what is left).
    let cn1 = t.action("SalesInvoice", si_id, "make_credit_note", json!({})).await;
    let cn1_doc = t.get("SalesInvoice", cn1).await;
    let rent_back = cn1_doc["items"].as_array().unwrap().iter().find(|l| l["item"].is_null()).unwrap().clone();
    assert_eq!((rent_back["description"].clone(), rent_back["return_line"].clone()), (json!("Rent for May"), rent["id"].clone()));
    t.update("SalesInvoice", cn1, json!({"items": [{"id": rent_back["id"]}]})).await;
    t.submit("SalesInvoice", cn1).await;
    assert_eq!(t.posted(cn1).await, map(&[("Debtors", "-1100"), ("Other Income", "1000"), ("Output Tax", "100")]));
    let si_now = t.get("SalesInvoice", si_id).await;
    assert_eq!((dec(&si_now["items"][1]["credited_amount"]), dec(&si_now["items"][1]["returned_qty"])), (n("1000"), n("1")));
    assert_eq!(dec(&si_now["outstanding_amount"]), n("200"));
    let cn2 = t.action("SalesInvoice", si_id, "make_credit_note", json!({})).await;
    let cn2_doc = t.get("SalesInvoice", cn2).await;
    assert_eq!(cn2_doc["items"].as_array().unwrap().len(), 1, "the rent is credited already: only the advice is left");
    t.submit("SalesInvoice", cn2).await;
    assert_eq!(t.posted(cn2).await, map(&[("Debtors", "-200"), ("Sales", "181.82"), ("Output Tax", "18.18")]));
    assert_eq!((dec(&t.get("SalesInvoice", si_id).await["outstanding_amount"]), t.owes(globex.parse().unwrap()).await), (Decimal::ZERO, Decimal::ZERO));
    refused(t.k.run_action(&t.ctx, "SalesInvoice", si_id, "make_credit_note", json!({})).await, "has already been returned");
    // A line that is no item, sold for a service period, waits on Deferred Revenue.
    let support = t
        .make(
            "SalesInvoice",
            json!({"company": c, "customer": umbrella, "items": [{"description": "Annual support", "qty": "1", "price_list_rate": "1200",
                   "enable_deferred_revenue": true, "service_start_date": day(0), "service_end_date": day(364)}]}),
        )
        .await;
    assert_eq!(t.posted(uid(&support)).await, map(&[("Debtors", "1200"), ("Deferred Revenue", "-1200")]));

    // ───────────── refusals: a line must say what it is and go to an account that can take it ─────────────
    let line = |more: Value| {
        let mut l = json!({"qty": "1", "price_list_rate": "10"});
        l.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap());
        json!({"company": c, "customer": umbrella, "items": [{"item": advice, "qty": "1"}, l]})
    };
    refused(t.k.insert(&t.ctx, "SalesInvoice", obj(line(json!({})))).await, "line 2 has no item: say what it is in its description");
    for (acc, words) in [
        ("Debtors", "line 2 (Fee) cannot be booked to Debtors: what a party owes goes there through the invoice itself"),
        ("Direct Income", "is_group must be false"),
        ("Primary Bank", "cannot be booked to Primary Bank: money goes there through a payment entry"),
        ("Stock In Hand", "cannot be booked to Stock In Hand: stock is valued there"),
    ] {
        let a = account(acc).await;
        refused(t.k.insert(&t.ctx, "SalesInvoice", obj(line(json!({"description": "Fee", "income_account": a})))).await, words);
    }
    let default_income = t.get("Company", cid).await["default_income_account"].clone();
    t.update("Company", cid, json!({"default_income_account": null})).await;
    refused(t.k.insert(&t.ctx, "SalesInvoice", obj(line(json!({"description": "Fee"})))).await, "line 2 (Fee) has neither an item nor an account");
    t.update("Company", cid, json!({"default_income_account": default_income})).await;

    // ───────────── an electricity bill booked straight to Utilities, with a cost centre ─────────────
    let bill = t
        .make(
            "PurchaseInvoice",
            json!({"company": c, "supplier": power, "bill_no": "EB-0425", "items": [
                {"description": "Electricity, April", "qty": "1", "price_list_rate": "5000", "expense_account": utilities, "cost_center": factory.to_string()},
                {"description": "Meter rent", "qty": "1", "price_list_rate": "100"}]}),
        )
        .await;
    let bill_id = uid(&bill);
    assert_eq!(t.posted(bill_id).await, map(&[("Utilities", "5000"), ("Administrative Expenses", "100"), ("Creditors", "-5100")]));
    {
        let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
        let at: Option<Uuid> = sqlx::query_scalar(
            "select l.cost_center_id from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
             join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id where l.tenant_id = $1 and e.source_id = $2 and a.account_name = 'Utilities'",
        )
        .bind(t.ctx.tenant)
        .bind(bill_id)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(at, Some(factory), "the electricity is the factory's");
    }
    // A rebate of a tenth of the price: a debit note of price alone on both lines.
    let rebate = t.action("PurchaseInvoice", bill_id, "make_debit_note", json!({"credit_percent": "10"})).await;
    t.update("PurchaseInvoice", rebate, json!({"bill_no": "EB-0425-R"})).await;
    t.submit("PurchaseInvoice", rebate).await;
    assert_eq!(t.posted(rebate).await, map(&[("Utilities", "-500"), ("Administrative Expenses", "-10"), ("Creditors", "510")]));
    let bill_now = t.get("PurchaseInvoice", bill_id).await;
    assert_eq!((dec(&bill_now["items"][0]["credited_amount"]), dec(&bill_now["items"][1]["credited_amount"])), (n("500"), n("10")));

    // ───────────── an order with a service line, billed in two parts ─────────────
    let so = t
        .make(
            "SalesOrder",
            json!({"company": c, "customer": umbrella, "set_warehouse": stores, "items": [
                {"item": widget, "qty": "2"}, {"description": "Installation", "qty": "4", "price_list_rate": "250"}]}),
        )
        .await;
    let so_id = uid(&so);
    assert_eq!((dec(&so["grand_total"]), so["status"].clone()), (n("1300"), json!("To Deliver and Bill")));
    let dn = t.action("SalesOrder", so_id, "make_delivery_note", json!({})).await;
    let dn_doc = t.get("DeliveryNote", dn).await;
    assert_eq!(dn_doc["items"].as_array().unwrap().len(), 1, "the installation is not delivered");
    t.submit("DeliveryNote", dn).await;
    let order = t.get("SalesOrder", so_id).await;
    assert_eq!((dec(&order["per_delivered"]), order["status"].clone()), (n("100"), json!("To Bill")), "the goods are all delivered");
    let si1 = t.action("SalesOrder", so_id, "make_sales_invoice", json!({})).await;
    let si1_doc = t.get("SalesInvoice", si1).await;
    let install = si1_doc["items"].as_array().unwrap().iter().find(|l| l["item"].is_null()).unwrap().clone();
    assert_eq!(
        (install["description"].clone(), dec(&install["qty"]), dec(&install["price_list_rate"]), install["income_account"].clone()),
        (json!("Installation"), n("4"), n("250"), json!(account("Sales").await))
    );
    t.update("SalesInvoice", si1, json!({"items": [{"id": si1_doc["items"][0]["id"]}, {"id": install["id"], "qty": "2"}]})).await;
    t.submit("SalesInvoice", si1).await;
    let order = t.get("SalesOrder", so_id).await;
    assert_eq!((dec(&order["per_billed"]), order["status"].clone()), (n("66.67"), json!("To Bill")));
    let si2 = t.action("SalesOrder", so_id, "make_sales_invoice", json!({})).await;
    let si2_doc = t.get("SalesInvoice", si2).await;
    assert_eq!((si2_doc["items"].as_array().unwrap().len(), dec(&si2_doc["items"][0]["qty"])), (1, n("2")), "the two hours left");
    t.submit("SalesInvoice", si2).await;
    assert_eq!(t.posted(si2).await, map(&[("Debtors", "500"), ("Sales", "-500")]));
    assert_eq!(t.get("SalesOrder", so_id).await["status"], json!("Completed"));
    // An order of a service alone has nothing to receive, and is billed.
    let po =
        t.make("PurchaseOrder", json!({"company": c, "supplier": acme, "items": [{"description": "Audit fee", "qty": "1", "price_list_rate": "20000"}]})).await;
    assert_eq!((po["status"].clone(), dec(&po["per_received"])), (json!("To Bill"), n("100")));
    refused(t.k.run_action(&t.ctx, "PurchaseOrder", uid(&po), "make_purchase_receipt", json!({})).await, "not available");
    let pi = t.action("PurchaseOrder", uid(&po), "make_purchase_invoice", json!({})).await;
    t.update("PurchaseInvoice", pi, json!({"bill_no": "AUD-1"})).await;
    t.submit("PurchaseInvoice", pi).await;
    assert_eq!(t.posted(pi).await, map(&[("Administrative Expenses", "20000"), ("Creditors", "-20000")]));
    assert_eq!(t.get("PurchaseOrder", uid(&po)).await["status"], json!("Completed"));

    // ───────────── a sale within the group: the line that is no item reaches the buyer's books ─────────────
    let sub = uid(&t.insert("Company", json!({"company_name": "Sub Co", "abbr": "SUB", "default_currency": inr["id"]})).await);
    let to_sub = party("Customer", "Sub Co", json!({"represents_company": sub.to_string()})).await;
    party("Supplier", "Switching Co", json!({"represents_company": c})).await;
    let fee = t
        .make("SalesInvoice", json!({"company": c, "customer": to_sub, "items": [{"description": "Management fee", "qty": "1", "price_list_rate": "1000"}]}))
        .await;
    let mirror = t.action("SalesInvoice", uid(&fee), "make_inter_company_invoice", json!({})).await;
    let mirror = t.get("PurchaseInvoice", mirror).await;
    let sub_admin = t.find_one("Account", json!({"account_name": "Administrative Expenses", "company": sub.to_string()})).await["id"].clone();
    let l = t.line(&mirror, 0);
    assert_eq!(
        (l["item"].clone(), l["description"].clone(), l["expense_account"].clone(), dec(&mirror["net_total"])),
        (Value::Null, json!("Management fee"), sub_admin, n("1000"))
    );

    // ───────────── open invoices from the old books, imported from a spreadsheet ─────────────
    // Hooli owes ten in rupees and has an advance of 200; Initech eight of 100 dollars carried at 83;
    // the company owes Wayne one. Due 10 days ahead, and 10, 45, 75 and 120 days ago.
    let offsets = [10i64, -10, -45, -75, -120];
    let mut rows = Vec::new();
    for i in 0..10 {
        let due = offsets[i % 5];
        rows.push(json!({"company": c, "party_type": "Customer", "customer": hooli, "invoice_no": format!("OB-H-{:02}", i + 1),
                         "invoice_date": day(due - 30), "due_date": day(due), "posting_date": day(0), "amount": "1000"}));
    }
    rows.push(json!({"company": c, "customer": hooli, "invoice_no": "OB-H-CR", "invoice_date": day(-5), "posting_date": day(0), "amount": "-200"}));
    for i in 0..8 {
        let due = offsets[i % 5];
        rows.push(json!({"company": c, "customer": initech, "invoice_no": format!("OB-I-{:02}", i + 1), "invoice_date": day(due - 30), "due_date": day(due),
                         "posting_date": day(0), "currency": usd["id"], "conversion_rate": "83", "amount": "100"}));
    }
    rows.push(json!({"company": c, "party_type": "Supplier", "supplier": wayne, "invoice_no": "W-9", "invoice_date": day(-20), "posting_date": day(0), "amount": "700"}));
    // What is refused, row by row, in words.
    rows.push(json!({"company": c, "customer": hooli, "invoice_no": "OB-H-99", "invoice_date": day(-5), "posting_date": day(0), "amount": "0"}));
    rows.push(json!({"company": c, "customer": initech, "invoice_no": "OB-I-99", "invoice_date": day(-5), "posting_date": day(0), "currency": usd["id"], "amount": "5"}));
    rows.push(json!({"company": c, "customer": hooli, "invoice_no": "OB-H-01", "invoice_date": day(-5), "posting_date": day(0), "amount": "5"}));
    rows.push(json!({"company": c, "customer": hooli, "invoice_no": "OB-H-98", "invoice_date": day(1), "posting_date": day(0), "amount": "5"}));
    rows.push(json!({"company": c, "customer": globex, "invoice_no": si["number"], "invoice_date": day(-5), "posting_date": day(0), "amount": "5"}));
    let (made, failed) = t.k.import_rows(&t.ctx, "OpeningInvoice", &rows).await.unwrap();
    assert_eq!(made.len(), 20, "{failed:?}");
    let said: Vec<(i64, String)> = failed.iter().map(|f| (f["row"].as_i64().unwrap(), f.to_string())).collect();
    let expect = [
        (20, "Opening invoice OB-H-99 of Hooli has nothing open"),
        (21, "Opening invoice OB-I-99 of Initech is in USD: give the rate it is carried at"),
        (22, "Opening invoice OB-H-01 of Hooli is already carried in"),
        (23, "after the opening date"),
        (24, "is already in these books as SINV-"),
    ];
    assert_eq!(said.len(), expect.len(), "{said:?}");
    for ((row, text), (want_row, words)) in said.iter().zip(expect) {
        assert!(*row == want_row && text.contains(words), "row {row}: {words:?} not in {text}");
    }
    let ob: Vec<Uuid> = made.iter().map(|v| v.as_str().unwrap().parse().unwrap()).collect();
    let (ob_h01, ob_h10, ob_credit, ob_i01) = (ob[0], ob[9], ob[10], ob[11]);
    // A draft that becomes a double entry before posting: all are refused together, none posted.
    let late = t.insert("OpeningInvoice", json!({"company": c, "party_type": "Supplier", "supplier": acme, "invoice_no": "ACME-77", "invoice_date": day(-3), "posting_date": day(0), "amount": "50"})).await;
    t.make(
        "PurchaseInvoice",
        json!({"company": c, "supplier": acme, "bill_no": "ACME-77", "items": [{"description": "Courier", "qty": "1", "price_list_rate": "50"}]}),
    )
    .await;
    refused(
        t.k.run_action(&t.ctx, "Company", cid, "post_opening_invoices", json!({})).await,
        "Opening invoice ACME-77 of Acme was refused, so none was posted: Opening invoice ACME-77 of Acme is already in these books as PINV-",
    );
    assert_eq!(t.gl("Opening Balance Equity").await, Decimal::ZERO, "nothing posted");
    t.k.delete(&t.ctx, "OpeningInvoice", uid(&late), None).await.unwrap();
    let posted = t.k.run_action(&t.ctx, "Company", cid, "post_opening_invoices", json!({})).await.unwrap();
    assert_eq!(posted["message"], json!("Posted 20 opening invoices"));
    // 10 000 less the advance from Hooli, 800 dollars at 83 from Initech, 700 owed to Wayne.
    assert_eq!(t.gl("Opening Balance Equity").await, n("-75500"));
    assert_eq!(t.posted(ob_i01).await, map(&[("Debtors", "8300"), ("Opening Balance Equity", "-8300")]));
    assert_eq!((t.owes(hooli.parse().unwrap()).await, t.owes(initech.parse().unwrap()).await), (n("9800"), n("800")), "Initech's in dollars");
    assert_eq!(t.open_item(ob_i01).await, (Some("OB-I-01".into()), Some(today + Duration::days(10)), "USD".into(), n("100"), n("8300")));
    let h01 = t.get("OpeningInvoice", ob_h01).await;
    assert_eq!((h01["status"].clone(), dec(&h01["outstanding_amount"]), h01["party_name"].clone()), (json!("Unpaid"), n("1000"), json!("Hooli")));
    assert_eq!(t.get("OpeningInvoice", ob_credit).await["status"], json!("Credit"));
    // Aged by their own due dates, today and forty days ago.
    let ageing = |who: &str, on: i64| {
        let t = &t;
        let p = vec![("company", c.clone()), ("customer", who.to_string()), ("as_of", day(on))];
        async move { buckets(&t.report("receivables", &p).await) }
    };
    assert_eq!(ageing(&hooli, 0).await, ["2000", "1800", "2000", "2000", "2000", "9800"].map(n), "the advance is in 1-30");
    assert_eq!(ageing(&hooli, -40).await, ["2000", "2000", "2000", "2000", "0", "8000"].map(n), "what was invoiced by then, aged then");
    assert_eq!(ageing(&initech, 0).await, ["16600", "16600", "16600", "8300", "8300", "66400"].map(n));
    let ar = t.report("receivables", &[("company", c.clone()), ("customer", hooli.clone())]).await;
    assert!(ar["rows"].as_array().unwrap().iter().any(|r| r["voucher_no"] == json!("OB-H-01") && r["voucher_type"] == json!("OpeningInvoice")));
    // Get Outstanding Invoices offers them as it offers invoices.
    let draft = t.insert("PaymentEntry", json!({"company": c, "payment_type": "Receive", "customer": hooli, "paid_amount": "0", "bank_account": bank})).await;
    let offered = t.k.run_action(&t.ctx, "PaymentEntry", uid(&draft), "get_outstanding_invoices", json!({})).await.unwrap();
    assert_eq!((offered["references"].as_array().unwrap().len(), dec(&offered["paid_amount"])), (10, n("10000")));
    assert_eq!(offered["references"][0]["invoice_type"], json!("Opening Invoice"));
    t.k.delete(&t.ctx, "PaymentEntry", uid(&draft), None).await.unwrap();
    // Paid in part.
    let part = t
        .make(
            "PaymentEntry",
            json!({"company": c, "payment_type": "Receive", "customer": hooli, "paid_amount": "600", "bank_account": bank,
                   "references": [{"invoice": ob_h01.to_string(), "allocated_amount": "600"}]}),
        )
        .await;
    assert_eq!(part["references"][0]["invoice_number"], json!("OB-H-01"), "the number Hooli knows");
    let h01 = t.get("OpeningInvoice", ob_h01).await;
    assert_eq!((h01["status"].clone(), dec(&h01["outstanding_amount"])), (json!("Partly Paid"), n("400")));
    refused(t.k.cancel(&t.ctx, "OpeningInvoice", ob_h01, None).await, "Opening invoice OB-H-01 of Hooli has payments allocated to it");
    // Settled in dollars at 85: 200 more than it was carried at, an exchange gain.
    let usd_pay = t
        .make(
            "PaymentEntry",
            json!({"company": c, "payment_type": "Receive", "customer": initech, "currency": usd["id"], "exchange_rate": "85", "paid_amount": "100",
                   "bank_account": bank, "references": [{"invoice": ob_i01.to_string(), "allocated_amount": "100"}]}),
        )
        .await;
    assert_eq!(t.posted(uid(&usd_pay)).await, map(&[("Primary Bank", "8500"), ("Debtors", "-8300"), ("Exchange Gain or Loss", "-200")]));
    assert_eq!(t.get("OpeningInvoice", ob_i01).await["status"], json!("Paid"));
    // The advance given back.
    let refund = t
        .make(
            "PaymentEntry",
            json!({"company": c, "payment_type": "Pay", "is_refund": true, "customer": hooli, "paid_amount": "200", "bank_account": bank,
                   "references": [{"invoice": ob_credit.to_string(), "allocated_amount": "200"}]}),
        )
        .await;
    assert_eq!(t.posted(uid(&refund)).await, map(&[("Debtors", "200"), ("Primary Bank", "-200")]));
    let credit = t.get("OpeningInvoice", ob_credit).await;
    assert_eq!((credit["status"].clone(), dec(&credit["outstanding_amount"])), (json!("Paid"), Decimal::ZERO));
    // One carried in by mistake is cancelled: its balance leaves the books.
    t.cancel("OpeningInvoice", ob_h10).await;
    assert_eq!(t.posted(ob_h10).await, BTreeMap::new());
    assert_eq!(t.get("OpeningInvoice", ob_h10).await["status"], json!("Cancelled"));
    assert_eq!(t.owes(hooli.parse().unwrap()).await, n("8400"), "9 800 less 600 paid, the 200 refunded back on, 1 000 cancelled");

    // ───────────── a journal entry's party line: its own number, due date and currency ─────────────
    let je = |accounts: Value| json!({"company": c, "entry_type": "Opening Entry", "accounts": accounts});
    let old_way = t
        .make(
            "JournalEntry",
            je(json!([{"account": debtors, "party_type": "Customer", "customer": globex, "debit": "300"}, {"account": obe, "credit": "300"}])),
        )
        .await;
    let line_id = |doc: &Doc| doc["accounts"][0]["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    assert_eq!(
        t.open_item(line_id(&old_way)).await,
        (old_way["number"].as_str().map(str::to_string), Some(today), "INR".into(), n("300"), n("300")),
        "made the old way: due on its day, under the entry's number"
    );
    let dated = t
        .make(
            "JournalEntry",
            je(json!([{"account": debtors, "party_type": "Customer", "customer": globex, "debit": "5000", "reference_no": "OLD-77", "due_date": day(-20)},
                      {"account": obe, "credit": "5000"}])),
        )
        .await;
    assert_eq!(t.open_item(line_id(&dated)).await, (Some("OLD-77".into()), Some(today - Duration::days(20)), "INR".into(), n("5000"), n("5000")));
    let in_usd = t
        .make(
            "JournalEntry",
            je(json!([{"account": debtors, "party_type": "Customer", "customer": initech, "currency": usd["id"], "exchange_rate": "84", "debit_in_currency": "50",
                       "reference_no": "OLD-USD-3", "due_date": day(5)}, {"account": obe, "credit": "4200"}])),
        )
        .await;
    assert_eq!((dec(&in_usd["accounts"][0]["debit"]), dec(&in_usd["total_debit"])), (n("4200"), n("4200")));
    assert_eq!(t.open_item(line_id(&in_usd)).await, (Some("OLD-USD-3".into()), Some(today + Duration::days(5)), "USD".into(), n("50"), n("4200")));
    let settle = t
        .make(
            "PaymentEntry",
            json!({"company": c, "payment_type": "Receive", "customer": initech, "currency": usd["id"], "exchange_rate": "84", "paid_amount": "50",
                   "bank_account": bank, "references": [{"invoice": line_id(&in_usd).to_string(), "allocated_amount": "50"}]}),
        )
        .await;
    assert_eq!(t.posted(uid(&settle)).await, map(&[("Primary Bank", "4200"), ("Debtors", "-4200")]), "at the rate it was carried at: no difference");
    refused(
        t.k.insert(
            &t.ctx,
            "JournalEntry",
            obj(je(json!([{"account": debtors, "party_type": "Customer", "customer": initech, "currency": usd["id"], "debit_in_currency": "50"},
                                                         {"account": obe, "credit": "4200"}]))),
        )
        .await,
        "the rate USD is carried at is needed",
    );
    refused(
        t.k.insert(
            &t.ctx,
            "JournalEntry",
            obj(je(json!([{"account": cash, "currency": usd["id"], "exchange_rate": "84", "debit_in_currency": "50"},
                                                         {"account": obe, "credit": "4200"}]))),
        )
        .await,
        "only a customer's or supplier's line is carried in another currency",
    );

    // ───────────── money moved between the company's own accounts ─────────────
    let transfer = |more: Value| {
        let mut v =
            json!({"company": c, "payment_type": "Internal Transfer", "bank_account": bank, "paid_to": cash, "paid_amount": "1000", "reference_no": "CHQ-1"});
        v.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap());
        v
    };
    let to_cash = t.make("PaymentEntry", transfer(json!({}))).await;
    assert_eq!(
        (to_cash["party_name"].clone(), dec(&to_cash["received_amount"]), to_cash["customer"].clone()),
        (json!("Primary Bank → Cash"), n("1000"), Value::Null)
    );
    assert_eq!(t.posted(uid(&to_cash)).await, map(&[("Primary Bank", "-1000"), ("Cash", "1000")]));
    t.cancel("PaymentEntry", uid(&to_cash)).await;
    assert_eq!(t.posted(uid(&to_cash)).await, BTreeMap::new(), "cancelled like any payment");
    let groups = account("Bank Accounts").await;
    let usd_bank = uid(&t
        .insert("Account", json!({"account_name": "USD Bank", "company": c, "parent_account": groups, "root_type": "Asset", "account_type": "Bank", "account_currency": usd["id"]}))
        .await)
    .to_string();
    // Rupees bought dollars at 84, booked at the day's 83: a loss of 100.
    let buy =
        t.make("PaymentEntry", transfer(json!({"paid_to": usd_bank, "paid_amount": "8400", "received_amount": "100", "target_exchange_rate": "83"}))).await;
    assert_eq!(
        (buy["received_currency"].clone(), dec(&buy["base_received_amount"]), dec(&buy["exchange_difference"])),
        (usd["id"].clone(), n("8300"), n("-100"))
    );
    assert_eq!(t.posted(uid(&buy)).await, map(&[("Primary Bank", "-8400"), ("USD Bank", "8300"), ("Exchange Gain or Loss", "100")]));
    {
        let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
        let (txn, cur): (Decimal, String) = sqlx::query_as(
            "select l.debit_txn, trim(l.currency) from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
             where l.tenant_id = $1 and e.source_id = $2 and l.account_id = $3",
        )
        .bind(t.ctx.tenant)
        .bind(uid(&buy))
        .bind(usd_bank.parse::<Uuid>().unwrap())
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!((txn, cur), (n("100"), "USD".to_string()), "the dollar account holds dollars");
    }
    // Without the rate, the two amounts say it: no difference.
    let plain = t.make("PaymentEntry", transfer(json!({"paid_to": usd_bank, "paid_amount": "830", "received_amount": "10"}))).await;
    assert_eq!(t.posted(uid(&plain)).await, map(&[("Primary Bank", "-830"), ("USD Bank", "830")]));
    // Dollars back at 84 (the dollar account's currency is the payment's), 4 250 rupees received.
    let back = t
        .make(
            "PaymentEntry",
            transfer(json!({"bank_account": usd_bank, "paid_to": bank, "exchange_rate": "84", "paid_amount": "50", "received_amount": "4250"})),
        )
        .await;
    assert_eq!(back["currency"], usd["id"]);
    assert_eq!(t.posted(uid(&back)).await, map(&[("USD Bank", "-4200"), ("Primary Bank", "4250"), ("Exchange Gain or Loss", "-50")]));
    refused(t.k.insert(&t.ctx, "PaymentEntry", obj(transfer(json!({"paid_to": bank})))).await, "choose another account to send it to");
    refused(t.k.insert(&t.ctx, "PaymentEntry", obj(transfer(json!({"paid_to": debtors})))).await, "Debtors is not a Bank or Cash account of this company");
    refused(
        t.k.insert(&t.ctx, "PaymentEntry", obj(transfer(json!({"references": [{"invoice": ob_h01.to_string(), "allocated_amount": "1"}]})))).await,
        "settles no invoices",
    );
}
