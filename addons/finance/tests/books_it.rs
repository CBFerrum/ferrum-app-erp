//! The accountant's screens, against a real PostgreSQL (`FERRUM_FINANCE_BOOKS_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_finance_books_test`; the database is
//! reset first): what the bank reconciliation workbench shows and does, applying payments to open
//! invoices, comparative statements and the cash flow, foreign exchange revaluation and the
//! finance overview.
//!
//! Each section opens a company of its own with a handful of postings small enough to add up by
//! hand; the expected figures in the assertions are those sums, worked out on paper (the working is
//! in the comments), not read back from the engine.

use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery};
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

fn invalid(e: &Error, needle: &str) -> bool {
    match e {
        Error::Invalid(m) => m.contains(needle),
        Error::Validation(fs) => fs.iter().any(|f| f.message.contains(needle)),
        _ => false,
    }
}

struct T {
    k: Arc<Kernel>,
    tenant: Uuid,
    admin: Ctx,
    /// An accounts manager and an accounts user, who do most of what the sections do.
    am: Ctx,
    au: Ctx,
    /// The company the section in hand works in.
    company: Uuid,
    inr: Uuid,
    eur: Uuid,
}

impl T {
    async fn insert(&self, ctx: &Ctx, entity: &str, v: Value) -> Doc {
        self.k.insert(ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn submit(&self, ctx: &Ctx, entity: &str, id: Uuid) -> Doc {
        self.k.submit(ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn make(&self, ctx: &Ctx, entity: &str, v: Value) -> Doc {
        let d = self.insert(ctx, entity, v).await;
        self.submit(ctx, entity, uid(&d)).await
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.admin, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    async fn action(&self, ctx: &Ctx, entity: &str, id: Uuid, name: &str, params: Value) -> Value {
        self.k.run_action(ctx, entity, id, name, params).await.unwrap_or_else(|e| panic!("{entity}.{name}: {e:?}"))
    }
    async fn list(&self, entity: &str, filters: Value) -> Vec<Doc> {
        let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), limit: Some(500), ..Default::default() };
        self.k.list(&self.admin, entity, &q).await.unwrap().rows
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let rows = self.list(entity, filters.clone()).await;
        assert_eq!(rows.len(), 1, "{entity} {filters}");
        rows[0].clone()
    }
    async fn count(&self, sql: &str, id: Option<Uuid>) -> i64 {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).bind(id).fetch_one(&mut *tx).await.unwrap()
    }
    async fn amount(&self, sql: &str, id: Option<Uuid>) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).bind(id).fetch_one(&mut *tx).await.unwrap()
    }
    async fn uuid(&self, sql: &str, id: Option<Uuid>) -> Uuid {
        let mut tx = self.k.db.tenant_tx(self.tenant).await.unwrap();
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string())).bind(self.tenant).bind(id).fetch_one(&mut *tx).await.unwrap()
    }
    /// What is left on an open item (a receivable, a payable, money not yet applied).
    async fn outstanding(&self, item: Uuid) -> Decimal {
        self.amount("select outstanding from open_item where tenant_id = $1 and id = $2", Some(item)).await
    }
    /// Debit minus credit on an account (by name) in the working company.
    async fn gl(&self, account: &str) -> Decimal {
        self.amount(
            &format!(
                "select coalesce(sum(l.debit - l.credit), 0) from journal_line l join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
                 where l.tenant_id = $1 and l.company_id = '{}' and a.account_name = '{account}' and $2::uuid is null",
                self.company
            ),
            None,
        )
        .await
    }
    async fn report(&self, ctx: &Ctx, name: &str, params: &[(&str, String)]) -> Value {
        let p: HashMap<String, String> = params.iter().map(|(k, v)| (k.to_string(), v.clone())).collect();
        ferrum_domain::reports::run(&self.k, ctx, name, &p).await.unwrap_or_else(|e| panic!("report {name}: {e:?}"))
    }
    async fn user(&self, email: &str, role: &str) -> Ctx {
        let id = self.k.create_user(&self.admin, email, email, Some("Correct-Horse-9"), &[role.into()], false).await.expect("user");
        self.k.load_ctx(self.tenant, id, None, "it".into()).await.expect("ctx")
    }
    /// An account of the working company, by name.
    async fn account(&self, name: &str) -> Uuid {
        uid(&self.find_one("Account", json!({"account_name": name, "company": self.company.to_string()})).await)
    }
    /// A new company in rupees with the standard chart and the fiscal year 2026; later sections
    /// work in it.
    async fn open_company(&mut self, name: &str, abbr: &str) -> String {
        let company = self.insert(&self.admin, "Company", json!({"company_name": name, "abbr": abbr, "default_currency": self.inr.to_string()})).await;
        self.company = uid(&company);
        let cid = self.company.to_string();
        self.insert(
            &self.admin,
            "FiscalYear",
            json!({"year_name": format!("{abbr} 2026"), "company": cid, "start_date": "2026-01-01", "end_date": "2026-12-31"}),
        )
        .await;
        cid
    }
    fn journal(&self, date: &str, lines: Value) -> Value {
        json!({"company": self.company.to_string(), "posting_date": date, "accounts": lines})
    }
    /// Every journal entry of the tenant balances, and so does the working company's trial balance.
    async fn books_balance(&self) {
        let tb = self
            .report(
                &self.admin,
                "trial_balance",
                &[("company", self.company.to_string()), ("from_date", "2024-01-01".into()), ("to_date", "2029-12-31".into())],
            )
            .await;
        assert_eq!(tb["meta"]["balanced"], json!(true), "{}", tb["totals"]);
        let unbalanced = self
            .count("select count(*) from (select entry_id from journal_line where tenant_id = $1 and $2::uuid is null group by entry_id having sum(debit) <> sum(credit)) x", None)
            .await;
        assert_eq!(unbalanced, 0);
    }
}

fn dr(account: Uuid, amount: &str) -> Value {
    json!({"account": account.to_string(), "debit": amount})
}

fn cr(account: Uuid, amount: &str) -> Value {
    json!({"account": account.to_string(), "credit": amount})
}

#[test]
fn the_accountants_screens() {
    // Same runtime shape as the server: unoptimized async hook chains need more than 2 MiB.
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_FINANCE_BOOKS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_finance_books_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("platform packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    packages.extend(ferrum_finance::packages().expect("finance packages"));
    let catalog = Catalog::from_packages(packages).expect("catalog");
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    for source in ferrum_app_erp::reports() {
        ferrum_domain::reports::add_source(source);
    }
    ferrum_finance::register(&mut hooks);
    for source in ferrum_finance::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let k = Kernel::boot(db, catalog, hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("partitions");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "books".into(),
            name: "Books Co".into(),
            admin_email: "admin@books.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: vec!["finance".into()],
            settings: Some(json!({"fiscal_year_start_month": 1})),
        })
        .await
        .expect("tenant");
    let session = k.login("books", "admin@books.test", "Correct-Horse-9").await.expect("login");
    let admin = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.expect("ctx");
    let mut t = T { k: k.clone(), tenant, am: admin.clone(), au: admin.clone(), admin, company: Uuid::nil(), inr: Uuid::nil(), eur: Uuid::nil() };
    t.am = t.user("am@books.test", "Accounts Manager").await;
    t.au = t.user("au@books.test", "Accounts User").await;
    t.inr = uid(&t.find_one("Currency", json!({"code": "INR"})).await);
    t.eur = uid(&t.find_one("Currency", json!({"code": "EUR"})).await);

    bank_workbench(&mut t).await;
    payment_allocation(&mut t).await;
}

// ───────────────────────────── the bank reconciliation workbench ─────────────────────────────

async fn bank_workbench(t: &mut T) {
    let (a, am, au) = (t.admin.clone(), t.am.clone(), t.au.clone());
    let cid = t.open_company("Bank Co", "BC").await;
    let bank_gl = t.account("Primary Bank").await;
    let bank_charges = t.account("Bank Charges").await;
    let inr = t.inr.to_string();
    let acme = uid(&t.insert(&a, "Customer", json!({"customer_name": "Acme Corp"})).await);
    let globex = uid(&t.insert(&a, "Customer", json!({"customer_name": "Globex"})).await);
    let landlord = uid(&t.insert(&a, "Supplier", json!({"supplier_name": "Landlord"})).await);
    let supplies = uid(&t.insert(&a, "Supplier", json!({"supplier_name": "Supplies Co"})).await);
    let cafe = uid(&t.insert(&a, "Supplier", json!({"supplier_name": "Cafe"})).await);

    // The books: five payments that clear a line each or will, two small ones to a cafe, and two the
    // bank has not seen yet (250 received on the 30th, 300 paid on the 20th).
    let pay = |party: Uuid, kind: &str, date: &str, amount: &str, reference: &str| {
        let field = if kind == "Pay" { "supplier" } else { "customer" };
        json!({"company": cid, "payment_type": kind, field: party.to_string(), "posting_date": date, "bank_account": bank_gl.to_string(),
               "currency": inr, "paid_amount": amount, "reference_no": reference})
    };
    let pe_a = uid(&t.make(&am, "PaymentEntry", pay(acme, "Receive", "2026-03-02", "1500", "INV-1")).await);
    let pe_b = uid(&t.make(&am, "PaymentEntry", pay(globex, "Receive", "2026-03-04", "800", "G-1")).await);
    let pe_c = uid(&t.make(&am, "PaymentEntry", pay(landlord, "Pay", "2026-03-04", "500", "R-3")).await);
    let pe_d = uid(&t.make(&am, "PaymentEntry", pay(acme, "Receive", "2026-03-30", "250", "ACME-9")).await);
    t.make(&am, "PaymentEntry", pay(supplies, "Pay", "2026-03-20", "300", "CHQ-7")).await;
    let pe_x = uid(&t.make(&am, "PaymentEntry", pay(cafe, "Pay", "2026-03-06", "3.50", "")).await);
    let pe_y = uid(&t.make(&am, "PaymentEntry", pay(cafe, "Pay", "2026-03-07", "3.50", "")).await);

    let ba = t
        .insert(
            &am,
            "BankAccount",
            json!({"account_name": "Main", "company": cid, "account": bank_gl.to_string(), "opening_balance": "0", "opening_date": "2026-02-28"}),
        )
        .await;
    let ba_id = uid(&ba);

    // The statement, six lines. Importing it for a trial run writes nothing and says what would happen.
    let csv = "Date,Description,Reference,Amount\n02/03/2026,Payment from Acme Corp,INV-1,1500.00\n03/03/2026,Transfer Globex,G-1,800.00\n\
               04/03/2026,Rent March,R-3,-500.00\n05/03/2026,Bank charges,,-25.00\n06/03/2026,Coffee,,-3.50\n06/03/2026,Coffee,,-3.50\n";
    let mapping = json!({"date": "Date", "description": "Description", "reference": "Reference", "amount": "Amount"});
    let trial = t.action(&am, "BankAccount", ba_id, "import_transactions", json!({"csv": csv, "mapping": mapping, "dry_run": true})).await;
    assert_eq!((trial["dry_run"].clone(), trial["imported"].clone(), trial["duplicates"].clone()), (json!(true), json!(6), json!(0)), "{trial}");
    assert_eq!(
        (trial["from_date"].clone(), trial["to_date"].clone(), trial["preview"].as_array().unwrap().len()),
        (json!("2026-03-02"), json!("2026-03-06"), 6)
    );
    assert_eq!(dec(&trial["preview"][0]["deposit"]), n("1500"));
    assert!(t.list("BankTransaction", json!({"bank_account": ba_id.to_string()})).await.is_empty(), "a trial run writes nothing");
    let bad = t
        .action(
            &am,
            "BankAccount",
            ba_id,
            "import_transactions",
            json!({"csv": "Date,Amount\nsoon,5\n", "mapping": {"date": "Date", "amount": "Amount"}, "dry_run": true}),
        )
        .await;
    assert_eq!(bad["imported"], json!(0));
    assert!(bad["errors"][0].as_str().unwrap().starts_with("line 2"), "a trial run lists the bad lines instead of refusing: {bad}");
    let real = t.action(&am, "BankAccount", ba_id, "import_transactions", json!({"csv": csv, "mapping": mapping})).await;
    assert_eq!(real["imported"], json!(6));
    let again = t.action(&am, "BankAccount", ba_id, "import_transactions", json!({"csv": csv, "mapping": mapping, "dry_run": true})).await;
    assert_eq!((again["imported"].clone(), again["duplicates"].clone()), (json!(0), json!(6)), "the same statement again is all duplicates: {again}");

    // An accounts user reads the workbench (the account is theirs to read), as of the 31st.
    let w = t.action(&au, "BankAccount", ba_id, "workbench", json!({"as_of": "2026-03-31"})).await;
    let lines = w["lines"].as_array().unwrap();
    let described: Vec<(String, Decimal, Decimal)> =
        lines.iter().map(|l| (l["description"].as_str().unwrap().to_string(), dec(&l["amount"]), dec(&l["balance"]))).collect();
    // The bank's balance after each line: 1500, +800, -500, -25, -3.50, -3.50.
    assert_eq!(
        described,
        [
            ("Payment from Acme Corp".to_string(), n("1500"), n("1500")),
            ("Transfer Globex".to_string(), n("800"), n("2300")),
            ("Rent March".to_string(), n("-500"), n("1800")),
            ("Bank charges".to_string(), n("-25"), n("1775")),
            ("Coffee".to_string(), n("-3.5"), n("1771.5")),
            ("Coffee".to_string(), n("-3.5"), n("1768")),
        ]
    );
    // Exact amounts pair off, each payment and each line once: the first three lines with their
    // payments, and the two coffees (both 3.50, both on the 6th) with the two 3.50 payments (the
    // 6th and the 7th), the earlier line taking the payment of its own day.
    let matched = |i: usize| lines[i]["match"].clone();
    assert_eq!(matched(0)["payment_entry"], json!(pe_a.to_string()));
    assert_eq!(matched(1)["payment_entry"], json!(pe_b.to_string()));
    assert_eq!(matched(2)["payment_entry"], json!(pe_c.to_string()));
    assert_eq!(matched(3), Value::Null, "nothing in the books is 25.00");
    assert_eq!(matched(4)["payment_entry"], json!(pe_x.to_string()), "the first coffee takes the payment of the same day");
    assert_eq!(matched(5)["payment_entry"], json!(pe_y.to_string()), "the second takes the one that is left");
    // Scores: 100 for the amount, 40 for the reference, 30 for the party, up to 20 for the date
    // (20 × (30 - days) / 30). Line 1: 100 + 40 + 30 + 20 = 190. Line 2 is a day from its payment:
    // 100 + 40 + 30 + 20 × 29 / 30 = 189.33. Line 3 names no party: 100 + 40 + 20 = 160.
    // The coffees: 100 + 20 = 120 for the same day, 100 + 19.33 = 119.33 for the next.
    assert_eq!(
        (dec(&matched(0)["score"]), dec(&matched(1)["score"]), dec(&matched(2)["score"]), dec(&matched(4)["score"]), dec(&matched(5)["score"])),
        (n("190"), n("189.33"), n("160"), n("120"), n("119.33"))
    );
    assert_eq!((w["counts"]["unreconciled"].clone(), w["counts"]["matched"].clone()), (json!(6), json!(5)));
    // The figures. Books: 1500 + 800 - 500 + 250 - 300 - 3.50 - 3.50 = 1743. Statement: 1768. Not
    // yet at the bank: deposits 1500 + 800 + 250 = 2550, payments 500 + 300 + 3.50 + 3.50 = 807. On
    // the statement, not in the books: all six lines, 1500 + 800 - 500 - 25 - 7 = 1768. What the
    // statement should say: 1743 - 2550 + 807 + 1768 = 1768, which it does.
    let f = &w["figures"];
    assert_eq!((dec(&f["statement_balance"]), dec(&f["book_balance"]), dec(&f["difference"])), (n("1768"), n("1743"), n("25")), "{f}");
    assert_eq!((dec(&f["uncleared_deposits"]), dec(&f["uncleared_payments"]), dec(&f["unrecorded"])), (n("2550"), n("807"), n("1768")));
    assert_eq!((dec(&f["expected_bank_balance"]), dec(&f["unexplained"])), (n("1768"), Decimal::ZERO));
    assert_eq!(w["books"].as_array().unwrap().len(), 7, "all seven payments are still in the books' open list");
    assert_eq!(w["books"][0]["payment_entry"], json!(pe_d.to_string()), "newest first");

    // One key accepts every exact match; the lot goes through together or not at all.
    let batch: Vec<Value> = [0usize, 1, 2, 4, 5]
        .iter()
        .map(|i| json!({"transaction": lines[*i]["id"], "payment_document": "PaymentEntry", "payment_entry": lines[*i]["match"]["payment_entry"]}))
        .collect();
    let charges = lines[3]["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    let mut stale = batch.clone();
    stale.push(json!({"transaction": charges.to_string(), "payment_document": "PaymentEntry", "payment_entry": pe_c.to_string()}));
    let refused = t.k.run_action(&au, "BankAccount", ba_id, "accept_matches", json!({"matches": stale})).await.unwrap_err();
    assert!(invalid(&refused, "BTXN-") && invalid(&refused, "0 left to clear"), "names the line, and the payment already cleared: {refused:?}");
    let after_refusal = t.action(&au, "BankAccount", ba_id, "workbench", json!({"as_of": "2026-03-31"})).await;
    assert_eq!(after_refusal["counts"]["unreconciled"], json!(6), "a refused batch leaves every line as it was");
    let accepted = t.action(&au, "BankAccount", ba_id, "accept_matches", json!({"matches": batch})).await;
    assert_eq!(accepted["accepted"], json!(5));
    assert!(accepted["lines"].as_array().unwrap().iter().all(|l| l["status"] == json!("Reconciled")));
    assert_eq!(t.get("PaymentEntry", pe_a).await["fin_clearance_date"], json!("2026-03-02"));
    let w = t.action(&au, "BankAccount", ba_id, "workbench", json!({"as_of": "2026-03-31"})).await;
    assert_eq!((w["counts"]["unreconciled"].clone(), w["lines"].as_array().unwrap().len()), (json!(1), 1));
    // Left: the bank charges not in the books, 250 and 300 not at the bank. Expected 1743 - 250 + 300 - 25 = 1768.
    let f = &w["figures"];
    assert_eq!((dec(&f["uncleared_deposits"]), dec(&f["uncleared_payments"]), dec(&f["unrecorded"])), (n("250"), n("300"), n("-25")));
    assert_eq!((dec(&f["expected_bank_balance"]), dec(&f["statement_balance"]), dec(&f["unexplained"])), (n("1768"), n("1768"), Decimal::ZERO));
    assert_eq!(w["books"].as_array().unwrap().len(), 2);
    let everything = t.action(&au, "BankAccount", ba_id, "workbench", json!({"as_of": "2026-03-31", "reconciled": true})).await;
    let first = &everything["lines"][0];
    assert_eq!(
        (everything["lines"].as_array().unwrap().len(), first["status"].clone(), first["allocations"][0]["payment_entry"].clone()),
        (6, json!("Reconciled"), json!(pe_a.to_string()))
    );

    // The line the books lack becomes a journal entry, and the figures follow: books 1743 - 25 = 1718.
    let am_charges = t.action(&am, "BankTransaction", charges, "make_journal_entry", json!({"account": bank_charges.to_string()})).await;
    assert_eq!(am_charges["entity"], json!("JournalEntry"));
    let w = t.action(&au, "BankAccount", ba_id, "workbench", json!({"as_of": "2026-03-31"})).await;
    let f = &w["figures"];
    assert_eq!(
        (w["lines"].as_array().unwrap().len(), dec(&f["book_balance"]), dec(&f["unrecorded"]), dec(&f["unexplained"])),
        (0, n("1718"), Decimal::ZERO, Decimal::ZERO)
    );
    assert_eq!(dec(&f["difference"]), n("50"), "what remains is the two payments the bank has not yet seen: 300 - 250");

    // Undoing a line brings its match back.
    let first_id = first["id"].as_str().unwrap().parse::<Uuid>().unwrap();
    t.action(&au, "BankTransaction", first_id, "unreconcile", json!({})).await;
    let w = t.action(&au, "BankAccount", ba_id, "workbench", json!({"as_of": "2026-03-31"})).await;
    assert_eq!(
        (w["lines"][0]["match"]["payment_entry"].clone(), t.get("PaymentEntry", pe_a).await["fin_clearance_date"].clone()),
        (json!(pe_a.to_string()), Value::Null)
    );
    // A date before the statement starts shows no lines, and the statement's balance is the opening one.
    let early = t.action(&au, "BankAccount", ba_id, "workbench", json!({"as_of": "2026-03-01"})).await;
    assert_eq!((early["lines"].as_array().unwrap().len(), dec(&early["figures"]["statement_balance"])), (0, Decimal::ZERO));
    t.books_balance().await;
}

// ───────────────────────────── applying payments to open invoices ─────────────────────────────

async fn payment_allocation(t: &mut T) {
    let (a, am, au) = (t.admin.clone(), t.am.clone(), t.au.clone());
    let cid = t.open_company("Alloc Co", "AC").await;
    let cash = t.account("Cash").await;
    let bank_gl = t.account("Primary Bank").await;
    let debtors = t.account("Debtors").await;
    let (inr, eur) = (t.inr.to_string(), t.eur.to_string());
    let services = t.find_one("ItemGroup", json!({"group_name": "Services"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    // One unit costs 1, so a quantity is an amount; the other item costs 9,200 rupees, 100 euros at 92.
    let unit_json =
        json!({"item_code": "UNIT", "item_name": "UNIT", "item_group": services["id"], "stock_uom": nos["id"], "is_stock_item": false, "standard_rate": "1"});
    let unit = uid(&t.insert(&a, "Item", unit_json).await);
    let euro_item = json!({"item_code": "EU-SVC", "item_name": "EU-SVC", "item_group": services["id"], "stock_uom": nos["id"], "is_stock_item": false, "standard_rate": "9200"});
    let eu_svc = uid(&t.insert(&a, "Item", euro_item).await);
    let acme = uid(&t.insert(&a, "Customer", json!({"customer_name": "Acme"})).await);
    let beta = uid(&t.insert(&a, "Customer", json!({"customer_name": "Beta"})).await);
    let invoice = |customer: Uuid, date: &str, amount: &str| json!({"company": cid, "customer": customer.to_string(), "posting_date": date, "items": [{"item": unit.to_string(), "qty": amount}]});
    let receipt = |customer: Uuid, date: &str, amount: &str, invoice: Option<Uuid>| {
        let mut v = json!({"company": cid, "payment_type": "Receive", "customer": customer.to_string(), "posting_date": date, "bank_account": bank_gl.to_string(),
                           "currency": inr, "paid_amount": amount});
        if let Some(i) = invoice {
            v["references"] = json!([{"invoice_type": "Sales Invoice", "invoice": i.to_string(), "allocated_amount": amount}]);
        }
        v
    };

    // Acme is owed 1000 (A), 600 (B) and 400 (C); D (150) was paid, and credited back afterwards.
    let sinv_a = t.make(&am, "SalesInvoice", invoice(acme, "2026-03-01", "1000")).await;
    let sinv_b = t.make(&am, "SalesInvoice", invoice(acme, "2026-03-10", "600")).await;
    let sinv_c = t.make(&am, "SalesInvoice", invoice(acme, "2026-03-20", "400")).await;
    let sinv_d = uid(&t.make(&am, "SalesInvoice", invoice(acme, "2026-03-05", "150")).await);
    let (a_id, b_id, c_id) = (uid(&sinv_a), uid(&sinv_b), uid(&sinv_c));
    t.make(&am, "PaymentEntry", receipt(acme, "2026-03-06", "150", Some(sinv_d))).await;
    let credit_note = t.action(&am, "SalesInvoice", sinv_d, "make_credit_note", json!({})).await;
    let cn_id = vid(&credit_note);
    t.k.update(&am, "SalesInvoice", cn_id, obj(json!({"posting_date": "2026-03-27"})), None).await.expect("the credit note's date");
    t.submit(&am, "SalesInvoice", cn_id).await;
    // Money not applied yet: 1200 received with no invoice named, and a journal that credits Acme 80.
    let p1 = uid(&t.make(&am, "PaymentEntry", receipt(acme, "2026-03-25", "1200", None)).await);
    let journal = json!({"company": cid, "posting_date": "2026-03-26", "accounts": [dr(cash, "80"), {"account": debtors.to_string(), "credit": "80", "party_type": "Customer", "customer": acme.to_string()}]});
    t.make(&am, "JournalEntry", journal).await;
    let je_line = t.uuid("select id from open_item where tenant_id = $1 and entity = 'JournalEntry' and customer_id = $2", Some(acme)).await;

    // Open: A 1000 + B 600 + C 400, less the credit note 150, the 1200 and the 80 = 570, which is what the ledger says Acme owes.
    let owed = "select coalesce(sum(outstanding), 0) from open_item where tenant_id = $1 and customer_id = $2 and status = 'open'";
    let ledger = "select coalesce(sum(debit - credit), 0) from journal_line where tenant_id = $1 and customer_id = $2";
    assert_eq!((t.amount(owed, Some(acme)).await, t.amount(ledger, Some(acme)).await), (n("570"), n("570")));

    // The pairings: the 1200 covers A (1000) and 200 of B; the 80 and the credit note's 150 go to B and C.
    let rows = json!([
        {"credit": p1.to_string(), "invoice": a_id.to_string(), "allocated_amount": "1000"},
        {"credit": p1.to_string(), "invoice": b_id.to_string(), "allocated_amount": "200"},
        {"credit": je_line.to_string(), "invoice": b_id.to_string(), "allocated_amount": "80"},
        {"credit": cn_id.to_string(), "invoice": c_id.to_string(), "allocated_amount": "150"},
    ]);
    let allocation = json!({"company": cid, "party_type": "Customer", "customer": acme.to_string(), "posting_date": "2026-03-28", "allocations": rows});

    // Asked before it is saved, the server says what each row is and what comes to: 1000 + 200 + 80 + 150 = 1430, no exchange difference.
    let preview = t.k.preview(&au, "PaymentAllocation", obj(allocation.clone())).await.expect("a preview");
    assert_eq!(
        (preview["party_name"].clone(), dec(&preview["total_allocated"]), dec(&preview["exchange_difference"])),
        (json!("Acme"), n("1430"), Decimal::ZERO)
    );
    assert_eq!(
        (preview["allocations"][0]["credit_type"].clone(), preview["allocations"][0]["invoice_number"].clone()),
        (json!("Payment Entry"), sinv_a["number"].clone())
    );
    assert_eq!(preview["allocations"][3]["credit_type"], json!("Sales Invoice"), "a credit note is a sales invoice");
    assert_eq!(preview["allocations"][2]["credit_type"], json!("Journal Entry"));
    assert!(t.list("PaymentAllocation", json!({})).await.is_empty(), "a preview writes nothing");

    // What cannot be done is refused with the reason.
    let refuse = |what: Value| {
        let mut v = allocation.clone();
        v["allocations"] = what;
        v
    };
    let one = |credit: Uuid, invoice: Uuid, amount: &str| json!({"credit": credit.to_string(), "invoice": invoice.to_string(), "allocated_amount": amount});
    let e = t.k.insert(&au, "PaymentAllocation", obj(refuse(json!([one(p1, a_id, "1001")])))).await.unwrap_err();
    assert!(invalid(&e, "left, not 1001"), "more than the invoice has left: {e:?}");
    let e = t.k.insert(&au, "PaymentAllocation", obj(refuse(json!([one(p1, a_id, "1000"), one(p1, b_id, "201")])))).await.unwrap_err();
    assert!(invalid(&e, "left, not 1201"), "more than the money has left, over two rows: {e:?}");
    let beta_invoice = uid(&t.make(&am, "SalesInvoice", invoice(beta, "2026-03-02", "100")).await);
    let e = t.k.insert(&au, "PaymentAllocation", obj(refuse(json!([one(p1, beta_invoice, "50")])))).await.unwrap_err();
    assert!(invalid(&e, "another party"), "{e:?}");
    let e = t.k.insert(&au, "PaymentAllocation", obj(refuse(json!([one(a_id, b_id, "50")])))).await.unwrap_err();
    assert!(invalid(&e, "not money to apply"), "an invoice is not money: {e:?}");
    let e = t.k.insert(&au, "PaymentAllocation", obj(refuse(json!([one(p1, cn_id, "50")])))).await.unwrap_err();
    assert!(invalid(&e, "nothing to pay"), "a credit note is not owed: {e:?}");
    let mut early = allocation.clone();
    early["posting_date"] = json!("2026-03-24");
    let e = t.k.insert(&au, "PaymentAllocation", obj(early)).await.unwrap_err();
    assert!(invalid(&e, "cannot be dated before"), "the 1200 arrived on the 25th: {e:?}");
    let e = t.k.insert(&au, "PaymentAllocation", obj(refuse(json!([])))).await.unwrap_err();
    assert!(invalid(&e, "at least one"), "{e:?}");

    // An accounts user applies it.
    let made = t.insert(&au, "PaymentAllocation", allocation).await;
    assert_eq!((made["docstatus"].clone(), dec(&made["total_allocated"])), (json!(0), n("1430")));
    let done = t.submit(&au, "PaymentAllocation", uid(&made)).await;
    assert!(done["number"].as_str().unwrap().starts_with("PAL-"), "{:?}", done["number"]);
    // Left on the invoices: A 0, B 600 - 200 - 80 = 320, C 400 - 150 = 250 (570 together, as before); the money is all used.
    assert_eq!((t.outstanding(a_id).await, t.outstanding(b_id).await, t.outstanding(c_id).await), (n("0"), n("320"), n("250")));
    assert_eq!((t.outstanding(p1).await, t.outstanding(je_line).await, t.outstanding(cn_id).await), (n("0"), n("0"), n("0")));
    let (doc_a, doc_b, doc_c) = (t.get("SalesInvoice", a_id).await, t.get("SalesInvoice", b_id).await, t.get("SalesInvoice", c_id).await);
    assert_eq!(
        (doc_a["status"].clone(), dec(&doc_b["outstanding_amount"]), doc_b["status"].clone(), dec(&doc_c["outstanding_amount"]), doc_c["status"].clone()),
        (json!("Paid"), n("320"), json!("Partly Paid"), n("250"), json!("Partly Paid"))
    );
    assert_eq!((t.amount(owed, Some(acme)).await, t.amount(ledger, Some(acme)).await), (n("570"), n("570")), "applying money moves nothing in the ledger");
    // Neither the payment nor an invoice can be cancelled from under it.
    let e = t.k.cancel(&am, "PaymentEntry", p1, None).await.unwrap_err();
    assert!(invalid(&e, "applied by payment allocation PAL-"), "{e:?}");
    let e = t.k.cancel(&am, "SalesInvoice", a_id, None).await.unwrap_err();
    assert!(invalid(&e, "applied by payment allocation PAL-"), "{e:?}");
    // Cancelling it opens everything again.
    t.k.cancel(&am, "PaymentAllocation", uid(&made), None).await.expect("cancel");
    assert_eq!((t.outstanding(a_id).await, t.outstanding(b_id).await, t.outstanding(c_id).await), (n("1000"), n("600"), n("400")));
    assert_eq!((t.outstanding(p1).await, t.outstanding(je_line).await, t.outstanding(cn_id).await), (n("-1200"), n("-80"), n("-150")));
    assert_eq!(t.get("SalesInvoice", a_id).await["status"], json!("Unpaid"));
    assert_eq!(t.amount(owed, Some(acme)).await, n("570"));

    // A supplier: 500 paid ahead, a bill of 800; applying the 500 leaves 300 to pay.
    let supplies = uid(&t.insert(&a, "Supplier", json!({"supplier_name": "Supplies Co"})).await);
    let advance = uid(&t
        .make(
            &am,
            "PaymentEntry",
            json!({"company": cid, "payment_type": "Pay", "supplier": supplies.to_string(), "posting_date": "2026-03-12", "bank_account": bank_gl.to_string(),
                                                          "currency": inr, "paid_amount": "500"}),
        )
        .await);
    let bill = uid(&t
        .make(
            &am,
            "PurchaseInvoice",
            json!({"company": cid, "supplier": supplies.to_string(), "bill_no": "S-1", "posting_date": "2026-03-15",
                                                          "items": [{"item": unit.to_string(), "qty": "800", "price_list_rate": "1"}]}),
        )
        .await);
    assert_eq!((t.outstanding(advance).await, t.outstanding(bill).await), (n("-500"), n("800")));
    let supplier_side = json!({"company": cid, "party_type": "Supplier", "supplier": supplies.to_string(), "posting_date": "2026-03-16", "allocations": [one(advance, bill, "500")]});
    let applied = t.make(&au, "PaymentAllocation", supplier_side).await;
    assert_eq!((dec(&applied["total_allocated"]), applied["party_name"].clone()), (n("500"), json!("Supplies Co")));
    assert_eq!((t.outstanding(advance).await, t.outstanding(bill).await), (n("0"), n("300")));
    let paid = t.get("PurchaseInvoice", bill).await;
    assert_eq!((dec(&paid["outstanding_amount"]), paid["status"].clone()), (n("300"), json!("Partly Paid")));

    // Euros booked at 90 when they came in and 92 when they were billed: 100 euros are 9,000 rupees
    // against a bill of 9,200, so applying one to the other is a loss of 200 on exchange.
    for (rate, from) in [("90", "2026-01-01"), ("92", "2026-04-01")] {
        t.insert(&a, "ExchangeRate", json!({"from_currency": eur, "to_currency": inr, "rate": rate, "valid_from": from})).await;
    }
    let euro = uid(&t.insert(&a, "Customer", json!({"customer_name": "Euro GmbH", "default_currency": eur})).await);
    let euros_in = uid(&t
        .make(
            &am,
            "PaymentEntry",
            json!({"company": cid, "payment_type": "Receive", "customer": euro.to_string(), "posting_date": "2026-03-01", "bank_account": cash.to_string(),
                                                           "currency": eur, "paid_amount": "100"}),
        )
        .await);
    let euro_bill = uid(&t
        .make(
            &am,
            "SalesInvoice",
            json!({"company": cid, "customer": euro.to_string(), "posting_date": "2026-04-05", "items": [{"item": eu_svc.to_string(), "qty": "1"}]}),
        )
        .await);
    assert_eq!((t.outstanding(euros_in).await, t.outstanding(euro_bill).await), (n("-100"), n("100")));
    let fx = json!({"company": cid, "party_type": "Customer", "customer": euro.to_string(), "posting_date": "2026-04-06", "allocations": [one(euros_in, euro_bill, "100")]});
    let seen = t.k.preview(&au, "PaymentAllocation", obj(fx.clone())).await.expect("preview");
    assert_eq!(
        (dec(&seen["allocations"][0]["exchange_difference"]), dec(&seen["exchange_difference"]), seen["currency"].clone()),
        (n("-200"), n("-200"), json!(eur))
    );
    let before = t.gl("Exchange Gain or Loss").await;
    t.make(&au, "PaymentAllocation", fx).await;
    assert_eq!(t.gl("Exchange Gain or Loss").await - before, n("200"), "a loss of 200 is a debit of 200");
    assert_eq!(t.get("SalesInvoice", euro_bill).await["status"], json!("Paid"));
    let euro_ledger = "select coalesce(sum(debit - credit), 0) from journal_line where tenant_id = $1 and customer_id = $2";
    assert_eq!(t.amount(euro_ledger, Some(euro)).await, Decimal::ZERO, "nothing is left on the customer's account in rupees");
    t.books_balance().await;
}
