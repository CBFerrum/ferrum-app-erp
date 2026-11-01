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
    let mut t = T { k: k.clone(), tenant, admin, company: Uuid::nil(), inr: Uuid::nil(), eur: Uuid::nil() };
    t.inr = uid(&t.find_one("Currency", json!({"code": "INR"})).await);
    t.eur = uid(&t.find_one("Currency", json!({"code": "EUR"})).await);

    bank_workbench(&mut t).await;
}

// ───────────────────────────── the bank reconciliation workbench ─────────────────────────────

async fn bank_workbench(t: &mut T) {
    let a = t.admin.clone();
    let am = t.user("am@books.test", "Accounts Manager").await;
    let au = t.user("au@books.test", "Accounts User").await;
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
    let pe_e = uid(&t.make(&am, "PaymentEntry", pay(supplies, "Pay", "2026-03-20", "300", "CHQ-7")).await);
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
