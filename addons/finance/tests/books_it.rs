//! The accountant's screens, against a real PostgreSQL (`FERRUM_FINANCE_BOOKS_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_finance_books_test`; the database is
//! reset first): what the bank reconciliation workbench shows and does, applying payments to open
//! invoices, comparative statements and the cash flow, foreign exchange revaluation, the
//! finance overview, finance books, and a group's books added up.
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
    statements(&mut t).await;
    revaluation(&mut t).await;
    overview(&mut t).await;
    finance_books(&mut t).await;
    consolidation(&mut t).await;
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

// ───────────────────────────── statements, compared, and the cash flow ─────────────────────────────

/// The row of a statement for an account (or total) by its words.
fn line<'a>(report: &'a Value, account: &str) -> &'a Value {
    report["rows"].as_array().unwrap().iter().find(|r| r["account"] == json!(account)).unwrap_or_else(|| panic!("no row {account}: {}", report["rows"]))
}

async fn statements(t: &mut T) {
    let (a, am) = (t.admin.clone(), t.am.clone());
    let cid = t.open_company("Stmt Co", "SC").await;
    let bank = t.account("Primary Bank").await;
    let capital = t.account("Capital Stock").await;
    let loans = t.account("Secured Loans").await;
    let computers = t.account("Computers").await;
    let rent = t.account("Rent").await;
    let unit = uid(&t.find_one("Item", json!({"item_code": "UNIT"})).await);
    let customer = uid(&t.insert(&a, "Customer", json!({"customer_name": "Stmt Customer"})).await);
    let operations = uid(&t.insert(&a, "CostCenter", json!({"cost_center_name": "Operations", "company": cid, "is_group": true})).await);
    let marketing =
        uid(&t.insert(&a, "CostCenter", json!({"cost_center_name": "Marketing", "company": cid, "parent_cost_center": operations.to_string()})).await);
    let sales_team =
        uid(&t.insert(&a, "CostCenter", json!({"cost_center_name": "Sales Team", "company": cid, "parent_cost_center": operations.to_string()})).await);
    let website = uid(&t.insert(&a, "Project", json!({"project_name": "Website", "company": cid})).await);
    let entry = |date: &str, lines: Value| t.journal(date, lines);
    let rent_line = |amount: &str, cc: Uuid, project: Option<Uuid>| json!({"account": rent.to_string(), "debit": amount, "cost_center": cc.to_string(), "project": project.map(|p| p.to_string())});

    // January: capital 10,000 and a loan of 4,000 come in, 2,000 of computers go out, 3,000 is
    // invoiced and 1,000 of it paid, 500 of rent. February: 600 of rent, 1,000 invoiced, 1,500 of the loan repaid.
    t.make(&am, "JournalEntry", entry("2026-01-05", json!([dr(bank, "10000"), cr(capital, "10000")]))).await;
    t.make(&am, "JournalEntry", entry("2026-01-08", json!([dr(bank, "4000"), cr(loans, "4000")]))).await;
    t.make(&am, "JournalEntry", entry("2026-01-10", json!([dr(computers, "2000"), cr(bank, "2000")]))).await;
    let jan_invoice = uid(&t
        .make(
            &am,
            "SalesInvoice",
            json!({"company": cid, "customer": customer.to_string(), "posting_date": "2026-01-15", "items": [{"item": unit.to_string(), "qty": "3000"}]}),
        )
        .await);
    t.make(
        &am,
        "PaymentEntry",
        json!({"company": cid, "payment_type": "Receive", "customer": customer.to_string(), "posting_date": "2026-01-20", "bank_account": bank.to_string(), "currency": t.inr.to_string(),
               "paid_amount": "1000", "references": [{"invoice_type": "Sales Invoice", "invoice": jan_invoice.to_string(), "allocated_amount": "1000"}]}),
    )
    .await;
    t.make(&am, "JournalEntry", entry("2026-01-25", json!([rent_line("500", marketing, None), cr(bank, "500")]))).await;
    t.make(&am, "JournalEntry", entry("2026-02-05", json!([rent_line("600", sales_team, Some(website)), cr(bank, "600")]))).await;
    t.make(
        &am,
        "SalesInvoice",
        json!({"company": cid, "customer": customer.to_string(), "posting_date": "2026-02-10", "items": [{"item": unit.to_string(), "qty": "1000"}]}),
    )
    .await;
    t.make(&am, "JournalEntry", entry("2026-02-14", json!([dr(loans, "1500"), cr(bank, "1500")]))).await;

    // Profit and loss: January 3,000 - 500 = 2,500; February 1,000 - 600 = 400. Asked on its own it is what it always was.
    let own = t.report(&am, "profit_and_loss", &[("company", cid.clone()), ("from_date", "2026-02-01".into()), ("to_date", "2026-02-28".into())]).await;
    assert_eq!((dec(&own["totals"]["income"]), dec(&own["totals"]["expense"]), dec(&own["totals"]["net_profit"])), (n("1000"), n("600"), n("400")));
    assert_eq!(own["columns"].as_array().unwrap().len(), 2, "one period has an account column and an amount column");
    assert!(own["rows"][1].get("amount_1").is_none() && line(&own, "Net Profit").get("change").is_none());

    // Set beside the month before: sales 1,000 against 3,000 (down 2,000, which is 66.67% of 3,000), rent 600
    // against 500 (up 100, 20%), the profit 400 against 2,500 (down 2,100, 84%).
    let both = t
        .report(
            &am,
            "profit_and_loss",
            &[("company", cid.clone()), ("from_date", "2026-02-01".into()), ("to_date", "2026-02-28".into()), ("compare", "Previous period".into())],
        )
        .await;
    let sales_row = line(&both, "4110 - Sales");
    assert_eq!(
        (dec(&sales_row["amount"]), dec(&sales_row["amount_1"]), dec(&sales_row["change"]), dec(&sales_row["change_pct"])),
        (n("1000"), n("3000"), n("-2000"), n("-66.67"))
    );
    let rent_row = line(&both, "5230 - Rent");
    assert_eq!(
        (dec(&rent_row["amount"]), dec(&rent_row["amount_1"]), dec(&rent_row["change"]), dec(&rent_row["change_pct"])),
        (n("600"), n("500"), n("100"), n("20"))
    );
    assert_eq!(rent_row["account_id"], json!(rent.to_string()), "a row names its account, to open the entries behind it");
    let net = line(&both, "Net Profit");
    assert_eq!((dec(&net["amount"]), dec(&net["amount_1"]), dec(&net["change"]), dec(&net["change_pct"])), (n("400"), n("2500"), n("-2100"), n("-84")));
    let heads: Vec<&str> = both["columns"].as_array().unwrap().iter().map(|c| c["label"].as_str().unwrap()).collect();
    assert_eq!(heads, ["Account", "Feb 2026", "Jan 2026", "Change", "Change %"]);
    assert_eq!((both["meta"]["periods"][1]["from_date"].clone(), both["meta"]["periods"][1]["to_date"].clone()), (json!("2026-01-01"), json!("2026-01-31")));
    assert_eq!(dec(&both["meta"]["periods"][1]["net_profit"]), n("2500"));
    // A year back has no February to compare with: every figure of it is nothing, and the change is not a percentage of nothing.
    let year = t
        .report(
            &am,
            "profit_and_loss",
            &[("company", cid.clone()), ("from_date", "2026-02-01".into()), ("to_date", "2026-02-28".into()), ("compare", "Previous year".into())],
        )
        .await;
    assert_eq!((dec(&line(&year, "4110 - Sales")["amount_1"]), line(&year, "4110 - Sales")["change_pct"].clone()), (Decimal::ZERO, Value::Null));
    let bad = ferrum_domain::reports::run(
        &t.k,
        &am,
        "profit_and_loss",
        &HashMap::from([("company".to_string(), cid.clone()), ("compare".to_string(), "last time".to_string())]),
    )
    .await
    .unwrap_err();
    assert!(invalid(&bad, "Previous period"), "{bad:?}");

    // The same by cost center: Operations holds Marketing (January's 500) and Sales Team (February's 600).
    let expense_in = |cc: Uuid, from: &'static str, to: &'static str| {
        let (t, am, cid) = (&*t, &am, &cid);
        async move {
            let r = t
                .report(am, "profit_and_loss", &[("company", cid.clone()), ("from_date", from.into()), ("to_date", to.into()), ("cost_center", cc.to_string())])
                .await;
            dec(&r["totals"]["expense"])
        }
    };
    assert_eq!(expense_in(operations, "2026-01-01", "2026-02-28").await, n("1100"), "a group is the cost centers below it");
    assert_eq!(expense_in(marketing, "2026-01-01", "2026-02-28").await, n("500"));
    assert_eq!(expense_in(sales_team, "2026-02-01", "2026-02-28").await, n("600"));
    assert_eq!(expense_in(sales_team, "2026-01-01", "2026-01-31").await, Decimal::ZERO);
    let project = t
        .report(
            &am,
            "profit_and_loss",
            &[("company", cid.clone()), ("from_date", "2026-01-01".into()), ("to_date", "2026-02-28".into()), ("project", website.to_string())],
        )
        .await;
    assert_eq!(dec(&project["totals"]["expense"]), n("600"), "the Website project had February's rent");

    // The entries behind a figure: the group of the indirect expenses is its accounts' entries, 500 and 600.
    let group = uid(&t.find_one("Account", json!({"account_name": "Indirect Expenses", "company": cid})).await);
    let ledger = t
        .report(
            &am,
            "general_ledger",
            &[("company", cid.clone()), ("from_date", "2026-01-01".into()), ("to_date", "2026-02-28".into()), ("account", group.to_string())],
        )
        .await;
    let entries: Vec<&Value> = ledger["rows"].as_array().unwrap().iter().filter(|r| r.get("voucher_type").is_some()).collect();
    assert_eq!(entries.len(), 2, "{ledger}");
    assert_eq!((dec(&line(&ledger, "Closing")["debit"]), dec(&line(&ledger, "Closing")["balance"])), (n("1100"), n("1100")));
    assert_eq!(entries[0]["voucher_type"], json!("JournalEntry"));

    // Balance sheet at the end of February against the end of January (a month back from a month's end is a month's end).
    // January: assets 12,500 (bank) + 2,000 (debtors) + 2,000 (computers) = 16,500; loan 4,000; equity 10,000 + 2,500 profit.
    // February: bank 12,500 - 600 - 1,500 = 10,400; debtors 3,000; computers 2,000 = 15,400; loan 2,500; equity 10,000 + 2,900.
    let sheet = t.report(&am, "balance_sheet", &[("company", cid.clone()), ("as_of", "2026-02-28".into()), ("compare", "Previous period".into())]).await;
    let total_assets = line(&sheet, "Total Assets");
    assert_eq!(
        (dec(&total_assets["amount"]), dec(&total_assets["amount_1"]), dec(&total_assets["change"]), dec(&total_assets["change_pct"])),
        (n("15400"), n("16500"), n("-1100"), n("-6.67"))
    );
    let (loan_row, open_row) = (line(&sheet, "2210 - Secured Loans"), line(&sheet, "Profit/loss not yet closed to equity"));
    assert_eq!((dec(&loan_row["amount"]), dec(&loan_row["amount_1"])), (n("2500"), n("4000")));
    assert_eq!((dec(&open_row["amount"]), dec(&open_row["amount_1"])), (n("2900"), n("2500")));
    assert_eq!(
        (dec(&line(&sheet, "Total Liabilities and Equity")["amount"]), dec(&line(&sheet, "Total Liabilities and Equity")["amount_1"])),
        (n("15400"), n("16500"))
    );
    assert_eq!((sheet["meta"]["balanced"].clone(), sheet["meta"]["periods"][1]["label"].clone()), (json!(true), json!("31 Jan 2026")));
    assert_eq!(dec(&sheet["meta"]["periods"][1]["assets"]), n("16500"));

    // Cash flow, January. Profit 2,500; debtors up 2,000 (-2,000), so operating is 500. Computers 2,000 bought: investing -2,000.
    // Capital 10,000 and the loan 4,000 came in: financing 14,000. Net 500 - 2,000 + 14,000 = 12,500, the bank's own movement.
    let jan = t.report(&am, "cash_flow", &[("company", cid.clone()), ("from_date", "2026-01-01".into()), ("to_date", "2026-01-31".into())]).await;
    let totals = &jan["totals"];
    assert_eq!(
        [&totals["operating"], &totals["investing"], &totals["financing"], &totals["net_change"], &totals["opening"], &totals["closing"]].map(dec),
        [n("500"), n("-2000"), n("14000"), n("12500"), Decimal::ZERO, n("12500")]
    );
    assert_eq!((jan["meta"]["reconciled"].clone(), dec(&jan["meta"]["difference"])), (json!(true), Decimal::ZERO));
    assert_eq!(dec(&line(&jan, "Profit (loss) for the period")["amount"]), n("2500"));
    assert_eq!(dec(&line(&jan, "1111 - Debtors")["amount"]), n("-2000"));
    assert_eq!(dec(&line(&jan, "1230 - Computers")["amount"]), n("-2000"));
    assert_eq!(dec(&line(&jan, "3100 - Capital Stock")["amount"]), n("10000"));
    assert_eq!(dec(&line(&jan, "2210 - Secured Loans")["amount"]), n("4000"), "the loan is financing because the standard chart says so");
    assert_eq!(line(&jan, "2210 - Secured Loans")["section"], json!("financing"));
    // February against January: profit 400, debtors up 1,000, so operating is -600; 1,500 of the loan repaid; 12,500 - 2,100 = 10,400.
    let feb = t
        .report(
            &am,
            "cash_flow",
            &[("company", cid.clone()), ("from_date", "2026-02-01".into()), ("to_date", "2026-02-28".into()), ("compare", "Previous period".into())],
        )
        .await;
    assert_eq!(
        [
            &feb["totals"]["operating"],
            &feb["totals"]["investing"],
            &feb["totals"]["financing"],
            &feb["totals"]["net_change"],
            &feb["totals"]["opening"],
            &feb["totals"]["closing"]
        ]
        .map(dec),
        [n("-600"), Decimal::ZERO, n("-1500"), n("-2100"), n("12500"), n("10400")]
    );
    let net = line(&feb, "Net increase (decrease) in cash");
    assert_eq!((dec(&net["amount"]), dec(&net["amount_1"]), dec(&net["change"]), dec(&net["change_pct"])), (n("-2100"), n("12500"), n("-14600"), n("-116.8")));
    assert_eq!((dec(&line(&feb, "1111 - Debtors")["amount"]), dec(&line(&feb, "1111 - Debtors")["amount_1"])), (n("-1000"), n("-2000")));
    assert_eq!(feb["meta"]["reconciled"], json!(true));
    assert_eq!(feb["meta"]["periods"][1]["label"], json!("Jan 2026"));
    // An account sets its own section: the computers become part of operations, and the cash flow still ends where the bank does.
    t.k.update(&am, "Account", computers, obj(json!({"cash_flow_section": "Operating"})), None).await.expect("a section of its own");
    let moved = t.report(&am, "cash_flow", &[("company", cid.clone()), ("from_date", "2026-01-01".into()), ("to_date", "2026-01-31".into())]).await;
    assert_eq!(
        (dec(&moved["totals"]["operating"]), dec(&moved["totals"]["investing"]), dec(&moved["totals"]["net_change"])),
        (n("-1500"), Decimal::ZERO, n("12500"))
    );
    t.books_balance().await;
}

// ───────────────────────────── foreign exchange revaluation ─────────────────────────────

async fn revaluation(t: &mut T) {
    let (a, am) = (t.admin.clone(), t.am.clone());
    let cid = t.open_company("Reval Co", "RV").await;
    let (inr, eur) = (t.inr.to_string(), t.eur.to_string());
    let bank_group = t.account("Bank Accounts").await;
    let eur_bank = uid(
        &t.insert(
            &a,
            "Account",
            json!({"account_name": "EUR Bank", "account_number": "1122", "company": cid, "parent_account": bank_group.to_string(), "root_type": "Asset", "account_type": "Bank", "account_currency": eur}),
        )
        .await,
    );
    // Euros are 90 rupees through March (the rates the allocation section put in), 92 from April, 89 from May.
    t.insert(&a, "ExchangeRate", json!({"from_currency": eur, "to_currency": inr, "rate": "89", "valid_from": "2026-05-01"})).await;
    let services = t.find_one("ItemGroup", json!({"group_name": "Services"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    // One of these costs 45,000 rupees: 500 euros at 90.
    let big = uid(&t.insert(&a, "Item", json!({"item_code": "EU-45K", "item_name": "EU-45K", "item_group": services["id"], "stock_uom": nos["id"], "is_stock_item": false, "standard_rate": "45000"})).await);
    let unit = uid(&t.find_one("Item", json!({"item_code": "UNIT"})).await);
    let euro_gmbh = uid(&t.insert(&a, "Customer", json!({"customer_name": "Euro GmbH", "default_currency": eur})).await);
    let paris = uid(&t.insert(&a, "Customer", json!({"customer_name": "Paris SA", "default_currency": eur})).await);
    let berlin = uid(&t.insert(&a, "Supplier", json!({"supplier_name": "Berlin AG", "default_currency": eur})).await);

    // March, at 90: Euro GmbH is invoiced 500 euros (45,000 rupees); Berlin AG bills us 200 euros (18,000); Paris SA pays 300 euros
    // ahead (27,000) into the euro account.
    t.make(
        &am,
        "SalesInvoice",
        json!({"company": cid, "customer": euro_gmbh.to_string(), "posting_date": "2026-03-10", "items": [{"item": big.to_string(), "qty": "1"}]}),
    )
    .await;
    let bill = t
        .make(&am, "PurchaseInvoice", json!({"company": cid, "supplier": berlin.to_string(), "bill_no": "B-1", "posting_date": "2026-03-12", "currency": eur, "items": [{"item": unit.to_string(), "qty": "200", "price_list_rate": "1"}]}))
        .await;
    assert_eq!(
        (bill["currency"].clone(), dec(&bill["grand_total"]), dec(&bill["base_grand_total"])),
        (json!(eur), n("200"), n("18000")),
        "the bill: 200 euros at 90"
    );
    t.make(&am, "PaymentEntry", json!({"company": cid, "payment_type": "Receive", "customer": paris.to_string(), "posting_date": "2026-03-14", "bank_account": eur_bank.to_string(), "currency": eur, "paid_amount": "300"})).await;
    assert_eq!(
        (t.gl("EUR Bank").await, t.gl("Debtors").await, t.gl("Creditors").await),
        (n("27000"), n("18000"), n("-18000")),
        "45,000 owed by Euro GmbH less the 27,000 Paris SA paid ahead"
    );

    // At the end of April, euros are 92. Worked per balance (euros at 92, against the books):
    //   Euro GmbH owes 500: 46,000 against 45,000, +1,000.   Paris SA is owed 300: -27,600 against -27,000, -600.
    //   The euro bank holds 300: 27,600 against 27,000, +600. Berlin AG is owed 200: -18,400 against -18,000, -400.
    // Net +600, a gain: it is credited to the exchange gain/loss account.
    let draft = json!({"company": cid, "posting_date": "2026-04-30", "reversal_date": "2999-01-01"});
    let seen = t.k.preview(&am, "ExchangeRevaluation", obj(draft.clone())).await.expect("a preview");
    let balances = seen["rows"].as_array().unwrap();
    assert_eq!(balances.len(), 4, "{:?}", seen["rows"]);
    let gain = |r: Option<&Value>| dec(&r.unwrap_or_else(|| panic!("a balance is missing: {:?}", seen["rows"]))["gain_loss"]);
    let of = |key: &str, party: Uuid| gain(balances.iter().find(|r| r[key] == json!(party.to_string())));
    let bank_row = gain(balances.iter().find(|r| r["customer"].is_null() && r["supplier"].is_null()));
    assert_eq!((of("customer", euro_gmbh), of("customer", paris), of("supplier", berlin), bank_row), (n("1000"), n("-600"), n("-400"), n("600")));
    let euro_row = balances.iter().find(|r| r["customer"] == json!(euro_gmbh.to_string())).unwrap();
    assert_eq!(
        (dec(&euro_row["balance"]), dec(&euro_row["book_value"]), dec(&euro_row["exchange_rate"]), dec(&euro_row["revalued"])),
        (n("500"), n("45000"), n("92"), n("46000"))
    );
    assert_eq!(dec(&seen["total_gain_loss"]), n("600"));
    assert_eq!(seen["gain_loss_account"], json!(t.account("Exchange Gain or Loss").await.to_string()), "the company's own account unless another is chosen");
    assert_eq!(seen["reversal_date"], json!("2999-01-01"));
    let early = t.k.preview(&am, "ExchangeRevaluation", obj(json!({"company": cid, "posting_date": "2026-02-01"}))).await.expect("a preview");
    assert!(early["rows"].as_array().unwrap().is_empty(), "nothing in euros has been booked by February");
    assert!(early.get("reversal_date").is_none_or(Value::is_null), "the day after unless another is chosen, taken when it is submitted");
    // No rate, no revaluation.
    let rateless = t.k.preview(&am, "ExchangeRevaluation", obj(json!({"company": cid, "posting_date": "2025-12-31"}))).await.expect("nothing to value yet");
    assert!(rateless["rows"].as_array().unwrap().is_empty());

    // Posted, the books carry the day's rate: debtors 18,000 + 1,000 - 600, creditors -18,000 - 400, the euro bank 27,600, a gain of 600.
    let run = t.insert(&am, "ExchangeRevaluation", draft.clone()).await;
    assert_eq!((run["docstatus"].clone(), dec(&run["total_gain_loss"])), (json!(0), n("600")));
    let run = t.submit(&am, "ExchangeRevaluation", uid(&run)).await;
    assert!(run["number"].as_str().unwrap().starts_with("ERV-"));
    assert_eq!(
        (t.gl("Debtors").await, t.gl("Creditors").await, t.gl("EUR Bank").await, t.gl("Exchange Gain or Loss").await),
        (n("18400"), n("-18400"), n("27600"), n("-600"))
    );
    assert_eq!(run["reversed_on"], Value::Null, "its reversal day is far off");
    t.books_balance().await;
    // The ageing is the documents', not the ledger's: the invoice still says 500 euros owed, booked at 90.
    let ageing = t.report(&am, "receivables", &[("company", cid.clone()), ("customer", euro_gmbh.to_string())]).await;
    assert_eq!(dec(&ageing["totals"]["outstanding_base"]), n("45000"));
    // Run again at the same rates and there is nothing left to revalue.
    let again = t.insert(&am, "ExchangeRevaluation", draft.clone()).await;
    assert!(again["rows"].as_array().unwrap().is_empty(), "the balances already stand at 92");
    let e = t.k.submit(&am, "ExchangeRevaluation", uid(&again), None).await.unwrap_err();
    assert!(invalid(&e, "nothing to revalue"), "{e:?}");

    // April's cash flow: the 600 is a gain on the euro bank that moved no cash, so it is told apart: operating 0 (profit 600, debtors
    // up 400, creditors up 400, less the 600 not realised), the exchange line 600, the bank 27,000 to 27,600.
    let april = t.report(&am, "cash_flow", &[("company", cid.clone()), ("from_date", "2026-04-01".into()), ("to_date", "2026-04-30".into())]).await;
    assert_eq!(
        [&april["totals"]["operating"], &april["totals"]["net_change"], &april["totals"]["opening"], &april["totals"]["closing"]].map(dec),
        [Decimal::ZERO, n("600"), n("27000"), n("27600")]
    );
    assert_eq!((dec(&line(&april, "Effect of exchange rates on cash and bank")["amount"]), april["meta"]["reconciled"].clone()), (n("600"), json!(true)));

    // Cancelled, it goes at once.
    t.k.cancel(&am, "ExchangeRevaluation", uid(&run), None).await.expect("cancel");
    assert_eq!(
        (t.gl("Debtors").await, t.gl("Creditors").await, t.gl("EUR Bank").await, t.gl("Exchange Gain or Loss").await),
        (n("18000"), n("-18000"), n("27000"), Decimal::ZERO)
    );

    // Run again and let its day come: the daily upkeep takes the difference out, once, and records the day.
    let second = t.make(&am, "ExchangeRevaluation", draft.clone()).await;
    assert_eq!(t.gl("Exchange Gain or Loss").await, n("-600"));
    ferrum_finance::maintenance(&t.k).await.expect("upkeep");
    assert_eq!(t.gl("Exchange Gain or Loss").await, n("-600"), "2999 has not come");
    {
        let mut tx = t.k.db.system_tx().await.unwrap();
        sqlx::query("update e_exchange_revaluation set reversal_date = '2026-05-01' where id = $1").bind(uid(&second)).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }
    ferrum_finance::maintenance(&t.k).await.expect("upkeep");
    assert_eq!((t.gl("Exchange Gain or Loss").await, t.gl("Debtors").await, t.gl("EUR Bank").await), (Decimal::ZERO, n("18000"), n("27000")));
    assert_eq!(t.get("ExchangeRevaluation", uid(&second)).await["reversed_on"], json!("2026-05-01"));
    let may_first: i64 = t
        .count(
            "select count(*) from journal_entry where tenant_id = $1 and source_id = $2 and posting_date = '2026-05-01' and reversal_of is not null",
            Some(uid(&second)),
        )
        .await;
    assert_eq!(may_first, 1, "the reversal is dated its own day");
    ferrum_finance::maintenance(&t.k).await.expect("upkeep");
    assert_eq!(
        t.count("select count(*) from journal_entry where tenant_id = $1 and source_id = $2", Some(uid(&second))).await,
        2,
        "reversed once: the entry and its reversal"
    );

    // May, at 89: Euro GmbH's 500 is 44,500 against 45,000 (-500), Paris SA's -300 is -26,700 against -27,000 (+300), the euro bank's
    // 300 is 26,700 against 27,000 (-300), Berlin AG's -200 is -17,800 against -18,000 (+200). Net -300, a loss. The reversal day
    // has passed, so it is reversed as soon as it is submitted.
    let may = t.make(&am, "ExchangeRevaluation", json!({"company": cid, "posting_date": "2026-05-31"})).await;
    assert_eq!((dec(&may["total_gain_loss"]), may["reversal_date"].clone(), may["reversed_on"].clone()), (n("-300"), json!("2026-06-01"), json!("2026-06-01")));
    assert_eq!(t.gl("Exchange Gain or Loss").await, Decimal::ZERO, "posted and taken out again");
    t.books_balance().await;
}

// ───────────────────────────── what needs attention ─────────────────────────────

async fn overview(t: &mut T) {
    let (a, am, au) = (t.admin.clone(), t.am.clone(), t.au.clone());
    let cid = t.open_company("Overview Co", "OV").await;
    t.insert(&a, "FiscalYear", json!({"year_name": "OV 2025", "company": cid, "start_date": "2025-01-01", "end_date": "2025-12-31"})).await;
    let fy26 = uid(&t.find_one("FiscalYear", json!({"year_name": "OV 2026", "company": cid})).await);
    let bank = t.account("Primary Bank").await;
    let capital = t.account("Capital Stock").await;
    let unit = uid(&t.find_one("Item", json!({"item_code": "UNIT"})).await);
    let customer = uid(&t.insert(&a, "Customer", json!({"customer_name": "Slow Payer"})).await);
    let supplier = uid(&t.insert(&a, "Supplier", json!({"supplier_name": "Slow Biller"})).await);
    // 100 in the bank since the end of 2025, a year nothing has closed.
    t.make(&am, "JournalEntry", t.journal("2025-12-31", json!([dr(bank, "100"), cr(capital, "100")]))).await;
    // Owed to us: 1000 due on 15 October (late), 400 due on 5 November (this week), 300 due on 1 December.
    for (date, amount, due) in [("2026-09-01", "1000", "2026-10-15"), ("2026-10-20", "400", "2026-11-05"), ("2026-10-25", "300", "2026-12-01")] {
        t.make(&am, "SalesInvoice", json!({"company": cid, "customer": customer.to_string(), "posting_date": date, "due_date": due, "items": [{"item": unit.to_string(), "qty": amount}]})).await;
    }
    // Owed by us: 500 due on 20 October (late), 200 due on 3 November (this week).
    for (date, amount, due, no) in [("2026-09-05", "500", "2026-10-20", "B-1"), ("2026-10-22", "200", "2026-11-03", "B-2")] {
        t.make(
            &am,
            "PurchaseInvoice",
            json!({"company": cid, "supplier": supplier.to_string(), "bill_no": no, "posting_date": date, "due_date": due,
                                              "items": [{"item": unit.to_string(), "qty": amount, "price_list_rate": "1"}]}),
        )
        .await;
    }
    let ops = uid(&t.insert(&am, "BankAccount", json!({"account_name": "Ops", "company": cid, "account": bank.to_string()})).await);
    let csv = "Date,Description,Amount\n28/10/2026,Deposit,50\n30/10/2026,Fee,-5\n";
    t.action(
        &am,
        "BankAccount",
        ops,
        "import_transactions",
        json!({"csv": csv, "mapping": {"date": "Date", "description": "Description", "amount": "Amount"}}),
    )
    .await;
    // Two drafts waiting, and January 2026 closed. Months that have ended and are open: the twelve of 2025 and February to October 2026.
    t.insert(&am, "JournalEntry", t.journal("2026-11-01", json!([dr(bank, "1"), cr(capital, "1")]))).await;
    t.insert(
        &am,
        "SalesInvoice",
        json!({"company": cid, "customer": customer.to_string(), "posting_date": "2026-11-01", "items": [{"item": unit.to_string(), "qty": "10"}]}),
    )
    .await;
    t.action(&am, "FiscalYear", fy26, "close_period", json!({"period_start": "2026-01-01"})).await;

    let report = t.report(&am, "finance_overview", &[("company", cid.clone()), ("as_of", "2026-11-01".into())]).await;
    let row = |key: &str| {
        report["rows"].as_array().unwrap().iter().find(|r| r["key"] == json!(key)).unwrap_or_else(|| panic!("no row {key}: {}", report["rows"])).clone()
    };
    let counted = |key: &str| (row(key)["count"].clone(), dec(&row(key)["amount"]), row(key)["oldest"].clone());
    assert_eq!(counted("overdue_receivables"), (json!(1), n("1000"), json!("2026-10-15")));
    assert_eq!(counted("receivables_due_week"), (json!(1), n("400"), json!("2026-11-05")));
    assert_eq!(counted("overdue_payables"), (json!(1), n("500"), json!("2026-10-20")));
    assert_eq!(counted("payables_due_week"), (json!(1), n("200"), json!("2026-11-03")));
    let lines = row("unreconciled_lines");
    assert_eq!((lines["count"].clone(), lines["oldest"].clone(), lines["detail"][0]["name"].clone()), (json!(2), json!("2026-10-28"), json!("Ops")));
    let drafts = row("drafts");
    assert_eq!((drafts["count"].clone(), drafts["detail"].as_array().unwrap().len()), (json!(2), 2));
    let kinds: Vec<&str> = drafts["detail"].as_array().unwrap().iter().map(|d| d["entity"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["SalesInvoice", "JournalEntry"]);
    let months = row("open_months");
    assert_eq!(
        (months["count"].clone(), months["oldest"].clone(), months["detail"][0]["start"].clone()),
        (json!(21), json!("2025-01-01"), json!("2026-10-01"))
    );
    assert_eq!(months["detail"][0]["fiscal_year"], json!(fy26.to_string()));
    let years = row("unclosed_years");
    assert_eq!((years["count"].clone(), years["detail"][0]["name"].clone()), (json!(1), json!("OV 2025")));
    assert_eq!(dec(&row("cash")["amount"]), n("100"));
    // Closing a month takes it off: with February closed, twenty remain.
    t.action(&am, "FiscalYear", fy26, "close_period", json!({"period_start": "2026-02-01"})).await;
    let report = t.report(&am, "finance_overview", &[("company", cid.clone()), ("as_of", "2026-11-01".into())]).await;
    assert_eq!(report["rows"].as_array().unwrap().iter().find(|r| r["key"] == json!("open_months")).unwrap()["count"], json!(20));
    // It counts only what the person may read: an accounts user cannot read sales invoices, so their draft is not theirs to be told of.
    let mine = t.report(&au, "finance_overview", &[("company", cid.clone()), ("as_of", "2026-11-01".into())]).await;
    let drafts = mine["rows"].as_array().unwrap().iter().find(|r| r["key"] == json!("drafts")).unwrap().clone();
    assert_eq!((drafts["count"].clone(), drafts["detail"][0]["entity"].clone()), (json!(1), json!("JournalEntry")));
}

// ───────────────────────────── finance books ─────────────────────────────

/// Income and expense, and their total, as the profit and loss shows them for the year in `book`.
async fn year_in(t: &T, cid: &str, book: Option<Uuid>) -> (Decimal, Decimal) {
    let mut p = vec![("company", cid.to_string()), ("from_date", "2026-01-01".to_string()), ("to_date", "2026-12-31".to_string())];
    if let Some(b) = book {
        p.push(("finance_book", b.to_string()));
    }
    let pl = t.report(&t.am, "profit_and_loss", &p).await;
    (dec(&pl["totals"]["expense"]), dec(&pl["totals"]["net_profit"]))
}

async fn finance_books(t: &mut T) {
    let am = t.am.clone();
    let cid = t.open_company("Books Co", "BK").await;
    let (rent, cash, admin_exp, computers) =
        (t.account("Rent").await, t.account("Cash").await, t.account("Administrative Expenses").await, t.account("Computers").await);
    let ifrs = uid(&t.insert(&am, "FinanceBook", json!({"finance_book_name": "IFRS"})).await);
    let tax = uid(&t.insert(&am, "FinanceBook", json!({"finance_book_name": "Tax"})).await);
    // Every book: rent of 1000 paid in cash. IFRS writes the computers down by 200 more, the tax
    // books by 500. So expenses are 1000 in the common books, 1200 in IFRS's and 1500 in the tax books'.
    t.make(&am, "JournalEntry", t.journal("2026-05-10", json!([dr(rent, "1000"), cr(cash, "1000")]))).await;
    let mut ifrs_je = t.journal("2026-06-30", json!([dr(admin_exp, "200"), cr(computers, "200")]));
    ifrs_je["finance_book"] = json!(ifrs.to_string());
    let ifrs_je = t.make(&am, "JournalEntry", ifrs_je).await;
    let mut tax_je = t.journal("2026-06-30", json!([dr(admin_exp, "500"), cr(computers, "500")]));
    tax_je["finance_book"] = json!(tax.to_string());
    t.make(&am, "JournalEntry", tax_je).await;
    assert_eq!(year_in(t, &cid, None).await, (n("1000"), n("-1000")), "the common books have no book's adjustments");
    assert_eq!(year_in(t, &cid, Some(ifrs)).await, (n("1200"), n("-1200")), "IFRS: the common entries and its own");
    assert_eq!(year_in(t, &cid, Some(tax)).await, (n("1500"), n("-1500")));
    // Each book balances, and names itself.
    for book in [None, Some(ifrs), Some(tax)] {
        let mut p = vec![("company", cid.clone()), ("from_date", "2026-01-01".to_string()), ("to_date", "2026-12-31".to_string())];
        if let Some(b) = book {
            p.push(("finance_book", b.to_string()));
        }
        let tb = t.report(&am, "trial_balance", &p).await;
        assert_eq!(tb["meta"]["balanced"], json!(true), "{book:?}: {}", tb["totals"]);
        assert_eq!(tb["meta"]["finance_book"], book.map(|b| json!(if b == ifrs { "IFRS" } else { "Tax" })).unwrap_or(Value::Null));
    }
    // What customers and suppliers owe is the same in every book.
    let debtors = t.account("Debtors").await;
    let customer = uid(&t.insert(&am, "Customer", json!({"customer_name": "Book Reader"})).await);
    let mut owed = t.journal(
        "2026-07-01",
        json!([{"account": debtors.to_string(), "party_type": "Customer", "customer": customer.to_string(), "debit": "10"}, cr(cash, "10")]),
    );
    owed["finance_book"] = json!(ifrs.to_string());
    let e = t.k.insert(&am, "JournalEntry", obj(owed)).await.unwrap_err();
    assert!(invalid(&e, "not what customers and suppliers owe"), "{e:?}");
    // A book's entry cancelled is reversed in that book.
    t.k.cancel(&am, "JournalEntry", uid(&ifrs_je), None).await.unwrap();
    assert_eq!(year_in(t, &cid, Some(ifrs)).await, (n("1000"), n("-1000")));
    assert_eq!(year_in(t, &cid, Some(tax)).await, (n("1500"), n("-1500")), "the other book is as it was");
    let reversals_in_ifrs = t
        .count(
            &format!("select count(*) from journal_entry where tenant_id = $1 and company_id = '{}' and finance_book_id = '{ifrs}' and reversal_of is not null and $2::uuid is null", t.company),
            None,
        )
        .await;
    assert_eq!(reversals_in_ifrs, 1);
    // The year closes in every book: income and expense at zero in each, retained earnings each book's own result.
    let fy = uid(&t.find_one("FiscalYear", json!({"company": cid.clone()})).await);
    let pcv = t.make(&am, "PeriodClosingVoucher", json!({"company": cid.clone(), "fiscal_year": fy.to_string()})).await;
    assert_eq!(dec(&pcv["net_profit"]), n("-1000"), "the profit it records is the common books'");
    for (book, result) in [(None, "-1000"), (Some(tax), "-1500"), (Some(ifrs), "-1000")] {
        let mut p = vec![("company", cid.clone()), ("as_of", "2026-12-31".to_string())];
        if let Some(b) = book {
            p.push(("finance_book", b.to_string()));
        }
        let bs = t.report(&am, "balance_sheet", &p).await;
        assert_eq!(bs["meta"]["balanced"], json!(true), "{book:?}: {}", bs["totals"]);
        let retained = bs["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["account"].as_str().is_some_and(|a| a.ends_with("Retained Earnings")))
            .unwrap_or_else(|| panic!("{}", bs["rows"]));
        assert_eq!(dec(&retained["amount"]), n(result), "{book:?}: the year went into retained earnings");
        let open = bs["rows"].as_array().unwrap().iter().find(|r| r["account"] == json!("Profit/loss not yet closed to equity")).unwrap();
        assert_eq!(dec(&open["amount"]), n("0"), "{book:?}: nothing left to close");
    }
    t.books_balance().await;
}

// ───────────────────────────── a group's books added up ─────────────────────────────

/// A company of the group, in `currency` and under `parent`, with the standard chart and the fiscal year 2026.
async fn group_company(t: &T, name: &str, abbr: &str, currency: Uuid, parent: Uuid) -> Uuid {
    let c = uid(&t
        .insert(
            &t.admin,
            "Company",
            json!({"company_name": name, "abbr": abbr, "default_currency": currency.to_string(), "parent_company": parent.to_string()}),
        )
        .await);
    t.insert(
        &t.admin,
        "FiscalYear",
        json!({"year_name": format!("{abbr} 2026"), "company": c.to_string(), "start_date": "2026-01-01", "end_date": "2026-12-31"}),
    )
    .await;
    c
}

async fn account_of(t: &T, company: Uuid, name: &str) -> Uuid {
    uid(&t.find_one("Account", json!({"account_name": name, "company": company.to_string()})).await)
}

/// A new account marked Eliminate in Consolidation, under the group account `parent`.
async fn eliminated(t: &T, company: Uuid, name: &str, number: &str, parent: &str, root: &str) -> Uuid {
    let parent = account_of(t, company, parent).await;
    let v = json!({"account_name": name, "account_number": number, "company": company.to_string(), "parent_account": parent.to_string(), "root_type": root, "eliminate_in_consolidation": true});
    uid(&t.insert(&t.admin, "Account", v).await)
}

/// A line with a customer or a supplier on it.
fn party(account: Uuid, kind: &str, who: Uuid, side: &str, amount: &str) -> Value {
    let mut v = json!({"account": account.to_string(), "party_type": kind});
    v[kind.to_lowercase()] = json!(who.to_string());
    v[side] = json!(amount);
    v
}

fn entry(company: Uuid, date: &str, lines: Value) -> Value {
    json!({"company": company.to_string(), "posting_date": date, "accounts": lines})
}

/// A trial balance's accounts, and for each its debits, credits and closing balance (debit, credit).
fn tb_lines(tb: &Value) -> Vec<(String, [Decimal; 4])> {
    tb["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["account"].as_str().unwrap().to_string(), [dec(&r["debit"]), dec(&r["credit"]), dec(&r["closing_debit"]), dec(&r["closing_credit"])]))
        .collect()
}

async fn consolidation(t: &mut T) {
    let (a, am) = (t.admin.clone(), t.am.clone());
    let group: Uuid = t.open_company("Group Co", "GP").await.parse().unwrap();
    // Group Co (rupees) owns Group India (rupees), which owns Group Europe (euros, 100 rupees
    // from December).
    let india = group_company(t, "Group India", "GI", t.inr, group).await;
    let europe = group_company(t, "Group Europe", "GE", t.eur, india).await;
    t.insert(&a, "ExchangeRate", json!({"from_currency": t.eur.to_string(), "to_currency": t.inr.to_string(), "rate": "100", "valid_from": "2026-12-01"}))
        .await;
    let e = t.k.update(&a, "Company", group, obj(json!({"parent_company": europe.to_string()})), None).await.unwrap_err();
    assert!(invalid(&e, "cycle"), "the platform keeps a hierarchy a tree: {e:?}");

    let ic_sales = eliminated(t, group, "Intercompany Sales", "4130", "Direct Income", "Income").await;
    let investment = eliminated(t, group, "Investment in Group India", "1240", "Fixed Assets", "Asset").await;
    let india_buys = eliminated(t, india, "Intercompany Purchases", "5290", "Indirect Expenses", "Expense").await;
    let europe_buys = eliminated(t, europe, "Intercompany Purchases", "5290", "Indirect Expenses", "Expense").await;
    let india_capital = account_of(t, india, "Capital Stock").await;
    t.k.update(&a, "Account", india_capital, obj(json!({"eliminate_in_consolidation": true})), None).await.expect("India's capital is the group's investment");
    // Europe's grants account has no number: Group Co's has the same name, India has none.
    let indirect = account_of(t, europe, "Indirect Income").await;
    let grants = uid(&t
        .insert(
            &a,
            "Account",
            json!({"account_name": "Grants Received", "company": europe.to_string(), "parent_account": indirect.to_string(), "root_type": "Income"}),
        )
        .await);
    let indirect = account_of(t, group, "Indirect Income").await;
    t.insert(&a, "Account", json!({"account_name": "Grants Received", "account_number": "4220", "company": group.to_string(), "parent_account": indirect.to_string(), "root_type": "Income"})).await;
    let to_india = uid(&t.insert(&a, "Customer", json!({"customer_name": "Group India Ltd", "represents_company": india.to_string()})).await);
    let to_europe = uid(&t.insert(&a, "Customer", json!({"customer_name": "Group Europe GmbH", "represents_company": europe.to_string()})).await);
    let outsider = uid(&t.insert(&a, "Customer", json!({"customer_name": "Outside Buyer"})).await);
    let from_group = uid(&t.insert(&a, "Supplier", json!({"supplier_name": "Group Co Ltd", "represents_company": group.to_string()})).await);
    let (cash, debtors, sales) = (account_of(t, group, "Cash").await, account_of(t, group, "Debtors").await, account_of(t, group, "Sales").await);
    let (india_cash, india_creditors, india_rent) =
        (account_of(t, india, "Cash").await, account_of(t, india, "Creditors").await, account_of(t, india, "Rent").await);
    let (europe_cash, europe_creditors, europe_sales) =
        (account_of(t, europe, "Cash").await, account_of(t, europe, "Creditors").await, account_of(t, europe, "Sales").await);
    for je in [
        // Group Co puts 5,000 into Group India.
        entry(group, "2026-01-15", json!([dr(investment, "5000"), cr(cash, "5000")])),
        entry(india, "2026-01-15", json!([dr(india_cash, "5000"), cr(india_capital, "5000")])),
        entry(group, "2026-02-01", json!([dr(cash, "3000"), cr(sales, "3000")])),
        // Group Co sells India 1,000 and both book it.
        entry(group, "2026-03-01", json!([party(debtors, "Customer", to_india, "debit", "1000"), cr(ic_sales, "1000")])),
        entry(india, "2026-03-01", json!([dr(india_buys, "1000"), party(india_creditors, "Supplier", from_group, "credit", "1000")])),
        // Group Co sells Europe 900; Europe books 9.50 euros (950).
        entry(group, "2026-04-01", json!([party(debtors, "Customer", to_europe, "debit", "900"), cr(ic_sales, "900")])),
        entry(europe, "2026-04-01", json!([dr(europe_buys, "9.5"), party(europe_creditors, "Supplier", from_group, "credit", "9.5")])),
        entry(india, "2026-05-01", json!([dr(india_rent, "500"), cr(india_cash, "500")])),
        // Group Co sells India 200 more, to plain Sales; India never books it.
        entry(group, "2026-06-01", json!([party(debtors, "Customer", to_india, "debit", "200"), cr(sales, "200")])),
        entry(europe, "2026-07-01", json!([dr(europe_cash, "20"), cr(europe_sales, "20")])),
        entry(europe, "2026-08-01", json!([dr(europe_cash, "5"), cr(grants, "5")])),
        // A sale to someone outside the group on the intercompany account stays in.
        entry(group, "2026-09-01", json!([party(debtors, "Customer", outsider, "debit", "70"), cr(ic_sales, "70")])),
    ] {
        t.make(&am, "JournalEntry", je).await;
    }
    let year = |c: Uuid| vec![("company", c.to_string()), ("from_date", "2026-01-01".to_string()), ("to_date", "2026-12-31".to_string())];
    let figures = |d: &str, c: &str, cd: &str, cc: &str| [n(d), n(c), n(cd), n(cc)];

    // The whole group. Left after eliminating, per account (debits, credits):
    //   Cash: Group Co 3,000 - 5,000, India 5,000 - 500, Europe 25 euros at the closing 100 =
    //   2,500: 10,500 - 5,500. Europe's income at the rate of its day (IAS 21), 89 from May: its July
    //   sale of 20 euros 1,780, its August grant of 5 euros 445.
    //   Sales: 3,000 + 200 + 1,780. Rent: India's 500. Debtors and Intercompany Sales: the outside
    //   buyer's 70. Grants: Europe's 445, on Group Co's account of that name.
    //   The translation reserve: Europe's 25 euros of income are 2,225, its cash 2,500: 275.
    //   The 200 sale's debtor went (India is in the group) but its sale stayed: a difference of 200.
    //   The investment, India's capital and the rest cancel.
    let tb = t.report(&am, "consolidated_trial_balance", &year(group)).await;
    assert_eq!(
        tb_lines(&tb),
        vec![
            ("1111 - Debtors".to_string(), figures("70", "0", "70", "0")),
            ("1131 - Cash".to_string(), figures("10500", "5500", "5000", "0")),
            ("4110 - Sales".to_string(), figures("0", "4980", "0", "4980")),
            ("4130 - Intercompany Sales".to_string(), figures("0", "70", "0", "70")),
            ("4220 - Grants Received".to_string(), figures("0", "445", "0", "445")),
            ("5230 - Rent".to_string(), figures("500", "0", "500", "0")),
            ("Foreign currency translation reserve".to_string(), figures("0", "275", "0", "275")),
            ("Intercompany difference (entries eliminated in part)".to_string(), figures("200", "0", "200", "0")),
        ]
    );
    let totals = &tb["totals"];
    assert_eq!((dec(&totals["debit"]), dec(&totals["credit"]), dec(&totals["closing_debit"])), (n("11270"), n("11270"), n("5770")));
    let meta = &tb["meta"];
    assert_eq!((meta["balanced"].clone(), meta["company"].clone(), meta["currency"].clone()), (json!(true), json!("Group Co"), json!("INR")));
    assert_eq!(dec(&meta["intercompany_difference"]), n("200"));
    // Eliminated, per company (debits, credits): Group Co the debtors 1,000 + 900 + 200 and the
    // investment 5,000, against sales of 1,000 + 900; India 1,000 against 1,000 and its capital;
    // Europe its purchase at April's 92 (874) against its creditor at the closing 100 (950).
    let companies: Vec<(String, Decimal, Decimal, Decimal)> = meta["companies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["company"].as_str().unwrap().to_string(), dec(&c["rate"]), dec(&c["eliminated_debit"]), dec(&c["eliminated_credit"])))
        .collect();
    assert_eq!(
        companies,
        vec![
            ("Group Co".to_string(), n("1"), n("7100"), n("1900")),
            ("Group Europe".to_string(), n("100"), n("874"), n("950")),
            ("Group India".to_string(), n("1"), n("1000"), n("6000")),
        ]
    );
    // What they say they owe each other: Europe 950 against Group Co's 900; India 1,000 against 1,200.
    let pairs: Vec<(String, Decimal, Decimal, Decimal)> = meta["intercompany_balances"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["counterparty"].as_str().unwrap().to_string(), dec(&p["company_says"]), dec(&p["counterparty_says"]), dec(&p["difference"])))
        .collect();
    assert_eq!(pairs, vec![("Group Europe".to_string(), n("900"), n("950"), n("-50")), ("Group India".to_string(), n("1200"), n("1000"), n("200"))]);
    assert_eq!(dec(&meta["intercompany_balances_disagree_by"]), n("250"));
    // The group's statements say the same: income 4,980 + 70 + 445, expenses 500; assets the
    // debtor's 70 and 5,000 cash, held by the year's profit and the reserve of 275 less the
    // difference of 200.
    let mut consolidated = year(group);
    consolidated.push(("consolidated", "1".into()));
    let pl = t.report(&am, "profit_and_loss", &consolidated).await;
    assert_eq!((dec(&pl["totals"]["income"]), dec(&pl["totals"]["expense"]), dec(&pl["totals"]["net_profit"])), (n("5495"), n("500"), n("4995")));
    assert_eq!(pl["meta"]["companies"], json!("Group Co, Group Europe, Group India"));
    let as_of = |c: Uuid| vec![("company", c.to_string()), ("as_of", "2026-12-31".to_string()), ("consolidated", "1".to_string())];
    let bs = t.report(&am, "balance_sheet", &as_of(group)).await;
    assert_eq!((dec(&bs["totals"]["assets"]), dec(&bs["totals"]["liabilities"]), dec(&bs["totals"]["equity"])), (n("5070"), n("0"), n("5070")));
    assert_eq!(bs["meta"]["balanced"], json!(true), "{}", bs["rows"]);
    let difference =
        bs["rows"].as_array().unwrap().iter().find(|r| r["account"] == json!("Intercompany difference (entries eliminated in part)")).expect("the difference");
    assert_eq!(dec(&difference["amount"]), n("-200"));

    // India holds 80% of Europe: a fifth of what Europe holds and earned is non-controlling
    // interests'. Europe's own net assets are its 25 euros of cash less the 9.50 it owes Group Co, at
    // the closing 100: 1,550, of which 310; its own profit is the 2,225 it earned less its 874
    // purchase from Group Co: 1,351, of which 270.20. The group's totals stay as they were.
    t.k.update(&a, "Company", europe, obj(json!({"ownership_percent": "80"})), None).await.unwrap();
    let pl = t.report(&am, "profit_and_loss", &consolidated).await;
    let split = |r: &Value, a: &str, b: &str| (dec(&r["totals"][a]), dec(&r["totals"][b]));
    assert_eq!(dec(&pl["totals"]["net_profit"]), n("4995"));
    assert_eq!(split(&pl, "profit_attributable_to_owners", "profit_attributable_to_non_controlling_interests"), (n("4724.8"), n("270.2")));
    let bs = t.report(&am, "balance_sheet", &as_of(group)).await;
    assert_eq!(dec(&bs["totals"]["equity"]), n("5070"));
    assert_eq!(split(&bs, "equity_attributable_to_owners", "non_controlling_interests"), (n("4760"), n("310")));
    assert_eq!(bs["meta"]["balanced"], json!(true));
    let part = &bs["meta"]["non_controlling_interests"][0];
    assert_eq!((part["company"].clone(), dec(&part["held_outside_pct"]), dec(&part["net_assets"])), (json!("Group Europe"), n("20"), n("1550")));
    // Group Co holding 90% of India as well: a tenth of India's own (its 4,500 of cash less the
    // 1,000 it owes Group Co, 3,500, and its loss of 1,500), and 28% of Europe's.
    t.k.update(&a, "Company", india, obj(json!({"ownership_percent": "90"})), None).await.unwrap();
    let bs = t.report(&am, "balance_sheet", &as_of(group)).await;
    assert_eq!(dec(&bs["totals"]["non_controlling_interests"]), n("784"), "350 + 434");
    let pl = t.report(&am, "profit_and_loss", &consolidated).await;
    assert_eq!(dec(&pl["totals"]["profit_attributable_to_non_controlling_interests"]), n("228.28"), "-150 + 378.28");
    // Whole again for what follows: no split is shown.
    for c in [india, europe] {
        t.k.update(&a, "Company", c, obj(json!({"ownership_percent": "100"})), None).await.unwrap();
    }
    assert!(t.report(&am, "balance_sheet", &as_of(group)).await["totals"].get("non_controlling_interests").is_none());

    // The group's cash flow: every flow at the rate of its day. The year's profit of 4,995 less the
    // outside buyer's 70 still owed, less the 200 India never booked (eliminated only in part), is
    // 4,725 in; Europe's 25 euros came in at 89 (2,225) and are held at the closing 100 (2,500): 275
    // is the effect of exchange rates. The investment in India and India's capital are the group's
    // business with itself: no investing, no financing. It ends where the group's cash does.
    let flows = t.report(&am, "cash_flow", &consolidated).await;
    let totals = &flows["totals"];
    let total = |k: &str| dec(&totals[k]);
    assert_eq!(
        (total("operating"), total("investing"), total("financing"), total("net_change"), total("opening"), total("closing")),
        (n("4725"), n("0"), n("0"), n("5000"), n("0"), n("5000")),
        "{}",
        flows["rows"]
    );
    assert_eq!((flows["meta"]["reconciled"].clone(), dec(&flows["meta"]["difference"])), (json!(true), n("0")));
    let flow_line = |name: &str| flows["rows"].as_array().unwrap().iter().find(|r| r["account"] == json!(name)).map(|r| dec(&r["amount"]));
    assert_eq!(flow_line("Profit (loss) for the period"), Some(n("4995")));
    assert_eq!(flow_line("1111 - Debtors"), Some(n("-70")));
    assert_eq!(flow_line("Intercompany difference (entries eliminated in part)"), Some(n("-200")));
    assert_eq!(flow_line("Effect of exchange rates on cash and bank"), Some(n("275")));
    assert_eq!(flows["meta"]["companies"], json!("Group Co, Group Europe, Group India"));
    // Group Co alone: its own cash, 3,000 in from its sale and 5,000 out into India.
    let own = t.report(&am, "cash_flow", &year(group)).await;
    assert_eq!((dec(&own["totals"]["closing"]), own["meta"]["reconciled"].clone()), (n("-2000"), json!(true)));

    // India's own group: Group Co is outside it now, so business with it stays, intercompany
    // accounts included, and India's capital is the group's own. Europe's April purchase of 9.50
    // euros is at April's 92 (874), its creditor at the closing 100 (950): the reserve is 220 on
    // the sale, 55 on the grant, less 76 on the purchase, 199.
    let tb = t.report(&am, "consolidated_trial_balance", &year(india)).await;
    assert_eq!(
        tb_lines(&tb),
        vec![
            ("1131 - Cash".to_string(), figures("7500", "500", "7000", "0")),
            ("2111 - Creditors".to_string(), figures("0", "1950", "0", "1950")),
            ("3100 - Capital Stock".to_string(), figures("0", "5000", "0", "5000")),
            ("4110 - Sales".to_string(), figures("0", "1780", "0", "1780")),
            ("5230 - Rent".to_string(), figures("500", "0", "500", "0")),
            ("5290 - Intercompany Purchases".to_string(), figures("1874", "0", "1874", "0")),
            ("Grants Received (Group Europe)".to_string(), figures("0", "445", "0", "445")),
            ("Foreign currency translation reserve".to_string(), figures("0", "199", "0", "199")),
        ]
    );
    assert_eq!((tb["meta"]["balanced"].clone(), tb["meta"]["intercompany_balances"].clone()), (json!(true), json!([])));
    // In its statements Europe's grants, which India's chart lacks, show on the account above them,
    // Indirect Income: income 1,780 + 445, expenses 500 + 1,874; equity the capital, the loss of
    // 149 and the reserve of 199.
    let mut consolidated = year(india);
    consolidated.push(("consolidated", "1".into()));
    let pl = t.report(&am, "profit_and_loss", &consolidated).await;
    assert_eq!((dec(&pl["totals"]["income"]), dec(&pl["totals"]["expense"]), dec(&pl["totals"]["net_profit"])), (n("2225"), n("2374"), n("-149")));
    assert_eq!(dec(&line(&pl, "4200 - Indirect Income")["amount"]), n("445"));
    let bs = t.report(&am, "balance_sheet", &as_of(india)).await;
    assert_eq!((dec(&bs["totals"]["assets"]), dec(&bs["totals"]["liabilities"]), dec(&bs["totals"]["equity"])), (n("7000"), n("1950"), n("5050")));
    assert_eq!(bs["meta"]["balanced"], json!(true), "{}", bs["rows"]);

    // Someone who may see only two of the three companies cannot add up all three.
    let scoped = t.user("group@books.test", "Accounts Manager").await;
    t.k.set_user_scopes(&a, scoped.user, "Company", &[group, india], None).await.expect("scope");
    let scoped = t.k.load_ctx(t.tenant, scoped.user, None, "it".into()).await.expect("ctx");
    let p: HashMap<String, String> = year(group).into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    let e = ferrum_domain::reports::run(&t.k, &scoped, "consolidated_trial_balance", &p).await.unwrap_err();
    assert!(matches!(&e, Error::Forbidden(m) if m.contains("Group Europe")), "{e:?}");
    // Before the first euro rate (January 2026) Europe cannot be translated.
    let p: HashMap<String, String> = [("company", group.to_string()), ("from_date", "2025-01-01".into()), ("to_date", "2025-12-31".into())]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let e = ferrum_domain::reports::run(&t.k, &am, "consolidated_trial_balance", &p).await.unwrap_err();
    assert!(invalid(&e, "no exchange rate from EUR to INR"), "{e:?}");

    // With a rate of 90 from July as well, Europe's July sale (20 euros) and August grant (5) take
    // it, 1,800 and 450, while its 25 euros of cash stay at the closing 100, 2,500. The 250 between
    // them is the translation reserve, and the intercompany difference is still only what was
    // eliminated in part. April's purchase is business within the group: eliminated, no part of it.
    t.insert(&a, "ExchangeRate", json!({"from_currency": t.eur.to_string(), "to_currency": t.inr.to_string(), "rate": "90", "valid_from": "2026-07-01"})).await;
    let tb = t.report(&am, "consolidated_trial_balance", &year(group)).await;
    let lines = tb_lines(&tb);
    let of = |name: &str| lines.iter().find(|(a, _)| a == name).map(|(_, f)| *f).unwrap_or_else(|| panic!("{name} in {lines:?}"));
    assert_eq!(of("4110 - Sales"), figures("0", "5000", "0", "5000"), "Europe's 20 euros at 90");
    assert_eq!(of("4220 - Grants Received"), figures("0", "450", "0", "450"));
    assert_eq!(of("1131 - Cash"), figures("10500", "5500", "5000", "0"), "at the closing rate");
    assert_eq!(of("Foreign currency translation reserve"), figures("0", "250", "0", "250"));
    assert_eq!(of("Intercompany difference (entries eliminated in part)"), figures("200", "0", "200", "0"));
    assert_eq!((tb["meta"]["balanced"].clone(), dec(&tb["meta"]["translation_reserve"])), (json!(true), n("250")));
    let mut grouped = year(group);
    grouped.push(("consolidated", "1".into()));
    let pl = t.report(&am, "profit_and_loss", &grouped).await;
    assert_eq!((dec(&pl["totals"]["income"]), dec(&pl["totals"]["net_profit"])), (n("5520"), n("5020")), "the reserve is no profit");
    let bs = t.report(&am, "balance_sheet", &as_of(group)).await;
    assert_eq!((dec(&bs["totals"]["assets"]), dec(&bs["totals"]["equity"]), bs["meta"]["balanced"].clone()), (n("5070"), n("5070"), json!(true)));
    let row = |name: &str| dec(&bs["rows"].as_array().unwrap().iter().find(|r| r["account"] == json!(name)).unwrap_or_else(|| panic!("{name}"))["amount"]);
    assert_eq!((row("Foreign currency translation reserve"), row("Intercompany difference (entries eliminated in part)")), (n("250"), n("-200")));
    t.books_balance().await;
}
