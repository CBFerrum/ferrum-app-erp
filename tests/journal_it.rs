//! Journal lines in an account's own currency, a balancing line, and settling a party line (S1-E1), against a real
//! PostgreSQL (`FERRUM_ERP_JOURNAL_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_journal_test`;
//! reset first). Each part works on its own records and runs as a task of its own, so one that fails does not hide
//! the others: the run names every part that failed, each after its own panic.

mod common;

use common::{T, dec, obj, run, uid};
use ferrum_kernel::capability::Invocation;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

#[test]
fn journal_lines_and_settling() {
    run(scenario());
}

#[derive(Clone)]
struct S {
    t: Arc<T>,
    c: String,
    usd: String,
    eur: String,
    usd_bank: String,
    inr_bank: String,
    cash: String,
    fees: String,
    rent: String,
    debtors: String,
    creditors: String,
    exchange: String,
    globex: String,
    initech: String,
}

impl S {
    async fn customer(&self, name: &str) -> String {
        uid(&self.t.insert("Customer", json!({"customer_name": name})).await).to_string()
    }
    async fn supplier(&self, name: &str) -> String {
        uid(&self.t.insert("Supplier", json!({"supplier_name": name})).await).to_string()
    }
    /// What a source posted on `account` in the account's own currency (debit less credit).
    async fn dollars(&self, source: Uuid, account: &str) -> (Decimal, usize) {
        let a: Uuid = account.parse().unwrap();
        let lines: Vec<_> = self.posted(source).await.into_iter().filter(|l| l.0 == a).collect();
        (lines.iter().map(|l| l.3 - l.4).sum(), lines.len())
    }
    /// A party's balance in the ledger, and the company's-currency value of what is open on it: equal when settling
    /// left nothing behind.
    async fn agrees(&self, party: &str) -> (Decimal, Decimal) {
        let p: Uuid = party.parse().unwrap();
        let ledger =
            self.t.sql::<(Decimal,)>("select coalesce(sum(debit - credit), 0) from journal_line where customer_id = $1 or supplier_id = $1", p).await[0].0;
        let open = self
            .t
            .sql::<(Decimal,)>(
                "select coalesce(sum(case when supplier_id is null then base_outstanding else -base_outstanding end), 0) from open_item \
                 where (customer_id = $1 or supplier_id = $1) and status <> 'cancelled'",
                p,
            )
            .await[0]
            .0;
        (ledger, open)
    }
    /// Inserts and submits a journal of `lines`; the error's text when refused.
    async fn je(&self, lines: Value) -> Result<ferrum_kernel::Doc, String> {
        self.t.try_post("JournalEntry", json!({"company": self.c, "accounts": lines})).await
    }
    async fn draft(&self, lines: Value) -> Result<ferrum_kernel::Doc, String> {
        self.t.k.insert(&self.t.ctx, "JournalEntry", obj(json!({"company": self.c, "accounts": lines}))).await.map_err(|e| format!("{e:?}"))
    }
    /// A credit note of `amount` for `customer` (in USD at `rate` when given): what is owed to them, open.
    async fn credit_note(&self, customer: &str, amount: &str, usd_at: Option<&str>) -> Uuid {
        let mut v = json!({"company": self.c, "customer": customer, "is_return": true, "return_reason": "Goodwill",
                           "items": [{"description": "Goodwill", "qty": "-1", "price_list_rate": amount}]});
        if let Some(rate) = usd_at {
            v["currency"] = json!(self.usd);
            v["conversion_rate"] = json!(rate);
        }
        let d = self.t.insert("SalesInvoice", v).await;
        self.t.submit("SalesInvoice", uid(&d)).await;
        uid(&d)
    }
    /// A refund journal: the customer's receivable debited `amount`, the rupee bank credited.
    async fn refund(&self, customer: &str, amount: &str) -> (Uuid, Uuid) {
        let e = self
            .je(json!([{"account": self.debtors, "party_type": "Customer", "customer": customer, "debit": amount}, {"account": self.inr_bank, "credit": amount}]))
            .await
            .unwrap_or_else(|e| panic!("refund journal: {e}"));
        (uid(&e), e["accounts"][0]["id"].as_str().unwrap().parse().unwrap())
    }
    async fn settle_as(&self, ctx: &ferrum_kernel::Ctx, je: Uuid, line: Uuid, against: Uuid, amount: Option<&str>, key: &str) -> Result<(Value, bool), String> {
        let mut params = json!({"line": line.to_string(), "against": against.to_string()});
        if let Some(a) = amount {
            params["amount"] = json!(a);
        }
        let call = Invocation::new("accounting.journal_entry.settle_line", json!({"id": je.to_string(), "params": params})).key(key);
        self.t.k.invoke(ctx, call).await.map(|r| (r.result, r.repeated)).map_err(|e| format!("{e:?}"))
    }
    async fn settle(&self, je: Uuid, line: Uuid, against: Uuid, amount: Option<&str>, key: &str) -> Result<Value, String> {
        self.settle_as(&self.t.ctx, je, line, against, amount, key).await.map(|r| r.0)
    }
    async fn open(&self, item: Uuid) -> Decimal {
        self.t.sql::<(Decimal,)>("select outstanding from open_item where id = $1", item).await[0].0
    }
    /// A new person holding Accounts Manager.
    async fn manager(&self, email: &str) -> Uuid {
        self.t.k.create_user(&self.t.ctx, email, "Books", Some("Correct-Horse-9"), &["Accounts Manager".into()], false).await.expect("user")
    }
    /// How many notices `user` has about `record`.
    async fn told(&self, user: Uuid, record: Uuid) -> i64 {
        self.t.sql::<(i64,)>(&format!("select count(*) from notification where record_id = '{record}' and user_id = $1"), user).await[0].0
    }
    /// Runs `q` with the workspace as $1 and `ids` as $2, and commits.
    async fn exec(&self, q: String, ids: &[Uuid]) {
        let mut tx = self.t.k.db.tenant_tx(self.t.ctx.tenant).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(q)).bind(self.t.ctx.tenant).bind(ids.to_vec()).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }
    /// The ledger lines `source` posted: account, debit, credit, debit and credit in the line's currency, currency, rate.
    async fn posted(&self, source: Uuid) -> Vec<(Uuid, Decimal, Decimal, Decimal, Decimal, String, Decimal)> {
        self.t
            .sql(
                "select l.account_id, l.debit, l.credit, l.debit_txn, l.credit_txn, trim(l.currency), l.exchange_rate from journal_line l \
                 join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id where e.source_id = $1",
                source,
            )
            .await
    }
}

fn line_of(posted: &[(Uuid, Decimal, Decimal, Decimal, Decimal, String, Decimal)], account: &str) -> (Decimal, Decimal, Decimal, Decimal, String, Decimal) {
    let a: Uuid = account.parse().unwrap();
    let l = posted.iter().find(|l| l.0 == a).unwrap_or_else(|| panic!("no line on {account}: {posted:?}"));
    (l.1, l.2, l.3, l.4, l.5.clone(), l.6)
}

fn err(r: Result<impl std::fmt::Debug, String>) -> String {
    match r {
        Ok(v) => format!("not refused: {v:?}"),
        Err(e) => e,
    }
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_JOURNAL_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_journal_test".into());
    let t = Arc::new(T::boot(url, |_, _| {}).await);
    let c = t.company.clone();
    let currency = async |code: &str| t.find_one("Currency", json!({"code": code})).await["id"].as_str().unwrap().to_string();
    let (usd, inr, eur) = (currency("USD").await, currency("INR").await, currency("EUR").await);
    let groups = t.account("Bank Accounts").await;
    let usd_bank = uid(&t
        .insert(
            "Account",
            json!({"account_name": "USD Bank", "company": c, "parent_account": groups, "root_type": "Asset", "account_type": "Bank", "account_currency": usd}),
        )
        .await)
    .to_string();
    let yesterday = (t.ctx.today() - chrono::Days::new(1)).to_string();
    t.insert("ExchangeRate", json!({"from_currency": usd, "to_currency": inr, "valid_from": yesterday, "rate": "83.10"})).await;
    let soon = (t.ctx.today() + chrono::Days::new(5)).to_string();
    t.insert("ExchangeRate", json!({"from_currency": usd, "to_currency": inr, "valid_from": soon, "rate": "84.00"})).await;
    let s = S {
        c: c.clone(),
        usd,
        eur,
        usd_bank,
        inr_bank: t.account("Primary Bank").await,
        cash: t.account("Cash").await,
        fees: t.account("Bank Charges").await,
        rent: t.account("Rent").await,
        debtors: t.account("Debtors").await,
        creditors: t.account("Creditors").await,
        exchange: t.account("Exchange Gain or Loss").await,
        globex: uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string(),
        initech: uid(&t.insert("Customer", json!({"customer_name": "Initech"})).await).to_string(),
        t: t.clone(),
    };
    let mut failed: Vec<&str> = Vec::new();
    macro_rules! part {
        ($name:literal, |$s:ident| $body:block) => {{
            let $s = s.clone();
            if tokio::spawn(async move { $body }).await.is_err() {
                failed.push($name);
            }
        }};
    }

    part!("a_bank_line_in_its_accounts_currency_posts_in_it", |s| {
        // 100 USD into the USD bank at 83.25 is 8,325.00 in rupees; the ledger keeps the 100 USD beside it.
        let e = s
            .je(json!([{"account": s.usd_bank, "currency": s.usd, "exchange_rate": "83.25", "debit_in_currency": "100"}, {"account": s.inr_bank, "credit": "8325"}]))
            .await
            .unwrap_or_else(|e| panic!("a bank line in its account's currency posts: {e}"));
        assert_eq!(dec(&e["accounts"][0]["debit"]), n("8325"));
        let posted = s.posted(uid(&e)).await;
        assert_eq!(line_of(&posted, &s.usd_bank), (n("8325"), n("0"), n("100"), n("0"), "USD".to_string(), n("83.25")));
    });

    part!("a_bank_line_in_another_currency_than_its_account_is_refused", |s| {
        // EUR on the USD bank; USD on the rupee bank and on cash, both kept in the company's currency.
        for (account, cur) in [(&s.usd_bank, &s.eur), (&s.inr_bank, &s.usd), (&s.cash, &s.usd)] {
            let e = err(s
                .draft(json!([{"account": account, "currency": cur, "exchange_rate": "90", "debit_in_currency": "10"}, {"account": s.rent, "credit": "900"}]))
                .await);
            assert!(e.contains("or a line on an account kept in it"), "{e}");
        }
    });

    part!("the_days_rate_is_taken_when_none_is_given", |s| {
        // 10 USD at the day's 83.10, shown as the day's rate (none given, so none to copy), taken again for the entry's
        // day when that changes, and posted at it.
        let lines = json!([{"account": s.usd_bank, "currency": s.usd, "debit_in_currency": "10"}, {"account": s.rent, "credit": "1", "balances": true}]);
        let d = s.draft(lines).await.unwrap_or_else(|e| panic!("draft: {e}"));
        let l = d["accounts"][0].clone();
        assert_eq!((dec(&l["day_rate"]), dec(&l["debit"])), (n("83.10"), n("831")), "{l}");
        assert!(l.get("exchange_rate").is_none_or(|v| v.is_null() || dec(v).is_zero()), "no rate given: {l}");
        let later = (s.t.ctx.today() + chrono::Days::new(6)).to_string();
        let d =
            s.t.k.update(&s.t.ctx, "JournalEntry", uid(&d), obj(json!({"posting_date": later})), None).await.unwrap_or_else(|e| panic!("a later day: {e:?}"));
        assert_eq!((dec(&d["accounts"][0]["day_rate"]), dec(&d["accounts"][0]["debit"])), (n("84.00"), n("840")), "the later day's rate");
        s.t.submit("JournalEntry", uid(&d)).await;
        assert_eq!(line_of(&s.posted(uid(&d)).await, &s.usd_bank), (n("840"), n("0"), n("10"), n("0"), "USD".to_string(), n("84.00")));
        // No rate on the day: refused on the line's own rate.
        let early = (s.t.ctx.today() - chrono::Days::new(30)).to_string();
        let e = err(s
            .t
            .k
            .insert(
                &s.t.ctx,
                "JournalEntry",
                obj(json!({"company": s.c, "posting_date": early, "accounts": [{"account": s.usd_bank, "currency": s.usd, "debit_in_currency": "10"},
                                                                              {"account": s.rent, "credit": "1", "balances": true}]})),
            )
            .await
            .map_err(|e| format!("{e:?}")));
        assert!(e.contains("field: \"exchange_rate\"") && e.contains("(\"accounts\", 0)"), "{e}");
    });

    part!("a_balancing_line_takes_what_balances_the_entry_after_rounding", |s| {
        // A dispute of 10.01 USD at 83.3333 with a fee of 25: the customer's line is 834.17 (834.166333 rounded), so the
        // disputed amount's line, given on the debit side, takes 809.17.
        let lines = json!([{"account": s.fees, "debit": "25"}, {"account": s.rent, "debit": "1", "balances": true},
                           {"account": s.debtors, "party_type": "Customer", "customer": s.globex, "currency": s.usd, "exchange_rate": "83.3333", "credit_in_currency": "10.01"}]);
        let d = s.draft(lines).await.unwrap_or_else(|e| panic!("draft: {e}"));
        assert_eq!(dec(&d["accounts"][1]["debit"]), n("809.17"), "what balances the entry");
        assert_eq!((dec(&d["total_debit"]), dec(&d["total_credit"])), (n("834.17"), n("834.17")));
        s.t.submit("JournalEntry", uid(&d)).await;
        let posted = s.posted(uid(&d)).await;
        let (dr, cr): (Decimal, Decimal) = (posted.iter().map(|l| l.1).sum(), posted.iter().map(|l| l.2).sum());
        assert_eq!((dr, cr), (n("834.17"), n("834.17")));
    });

    part!("two_balancing_lines_or_a_negative_one_are_refused", |s| {
        let party = |amount: &str| json!({"account": s.debtors, "party_type": "Customer", "customer": s.globex, "currency": s.usd, "exchange_rate": "83.3333", "credit_in_currency": amount});
        let e =
            err(s.draft(json!([{"account": s.fees, "debit": "1", "balances": true}, {"account": s.rent, "debit": "1", "balances": true}, party("1")])).await);
        assert!(e.contains("one line balances the entry"), "two: {e}");
        // A fee of 900 against 834.17 leaves the line on the debit side -65.83.
        let e = err(s.draft(json!([{"account": s.fees, "debit": "900"}, {"account": s.rent, "debit": "1", "balances": true}, party("10.01")])).await);
        assert!(e.contains("would be -65.83"), "negative: {e}");
        let e = err(s
            .draft(json!([{"account": s.debtors, "party_type": "Customer", "customer": s.globex, "currency": s.usd, "exchange_rate": "83", "debit_in_currency": "1", "balances": true},
                          {"account": s.rent, "credit": "83"}]))
            .await);
        assert!(e.contains("in the company's currency"), "a balancing line in dollars: {e}");
    });

    part!("a_foreign_journal_at_one_rate_on_both_lines_balances", |s| {
        // A refund of 12.34 USD at the receipt's 83.3333 on both lines: 1,028.33 each.
        let e = s
            .je(json!([{"account": s.debtors, "party_type": "Customer", "customer": s.globex, "currency": s.usd, "exchange_rate": "83.3333", "debit_in_currency": "12.34"},
                       {"account": s.usd_bank, "currency": s.usd, "exchange_rate": "83.3333", "credit_in_currency": "12.34"}]))
            .await
            .unwrap_or_else(|e| panic!("a refund at one rate posts: {e}"));
        assert_eq!((dec(&e["total_debit"]), dec(&e["total_credit"])), (n("1028.33"), n("1028.33")));
        assert_eq!(line_of(&s.posted(uid(&e)).await, &s.usd_bank), (n("0"), n("1028.33"), n("0"), n("12.34"), "USD".to_string(), n("83.3333")));
    });

    part!("settle_line_pairs_a_refund_journal_with_a_credit_note", |s| {
        let cn = s.credit_note(&s.globex, "100", None).await;
        let (je, line) = s.refund(&s.globex, "100").await;
        let r = s.settle(je, line, cn, None, "s7").await.unwrap_or_else(|e| panic!("settled: {e}"));
        assert_eq!((dec(&r["settled"]), dec(&r["left"])), (n("100"), n("0")));
        assert_eq!((s.open(cn).await, s.open(line).await), (n("0"), n("0")));
    });

    part!("settle_line_pairs_it_with_an_unallocated_receipt", |s| {
        let pe = uid(&s
            .t
            .post("PaymentEntry", json!({"company": s.c, "payment_type": "Receive", "customer": s.globex, "bank_account": s.inr_bank, "paid_amount": "50"}))
            .await);
        assert_eq!(s.open(pe).await, n("-50"), "an advance");
        let (je, line) = s.refund(&s.globex, "50").await;
        let r = s.settle(je, line, pe, None, "s8").await.unwrap_or_else(|e| panic!("settled: {e}"));
        assert_eq!((dec(&r["settled"]), dec(&r["left"]), s.open(pe).await), (n("50"), n("0"), n("0")));
    });

    part!("settle_line_stops_at_what_is_open_and_says_what_is_left", |s| {
        let (cn30, cn40) = (s.credit_note(&s.globex, "30", None).await, s.credit_note(&s.globex, "40", None).await);
        let (je, line) = s.refund(&s.globex, "80").await;
        let r = s.settle(je, line, cn30, None, "s9a").await.unwrap_or_else(|e| panic!("first: {e}"));
        assert_eq!((dec(&r["settled"]), dec(&r["left"])), (n("30"), n("50")));
        let r = s.settle(je, line, cn40, Some("10"), "s9b").await.unwrap_or_else(|e| panic!("ten of it: {e}"));
        assert_eq!((dec(&r["settled"]), dec(&r["left"])), (n("10"), n("40")));
        assert_eq!((s.open(cn40).await, s.open(line).await), (n("-30"), n("40")));
        let e = err(s.settle(je, line, cn30, None, "s9c").await);
        assert!(e.contains("is not open"), "{e}");
    });

    // settle_line_refuses_another_party_or_currency, the same side or a line that is not a party's: one part each.
    part!("settle_line_refuses_another_party", |s| {
        let (je, line) = s.refund(&s.globex, "20").await;
        let theirs = s.credit_note(&s.initech, "20", None).await;
        assert!(err(s.settle(je, line, theirs, None, "s10a").await).contains("another customer's or supplier's"));
        assert_eq!((s.open(line).await, s.open(theirs).await), (n("20"), n("-20")), "nothing settled");
    });

    part!("settle_line_refuses_another_currency", |s| {
        let (je, line) = s.refund(&s.globex, "20").await;
        let dollars = s.credit_note(&s.globex, "20", Some("82")).await;
        assert!(err(s.settle(je, line, dollars, None, "s10b").await).contains("in another currency"));
        assert_eq!((s.open(line).await, s.open(dollars).await), (n("20"), n("-20")), "nothing settled");
    });

    part!("settle_line_refuses_the_same_side", |s| {
        let (je, line) = s.refund(&s.globex, "20").await;
        let (_, other_debit) = s.refund(&s.globex, "5").await;
        assert!(err(s.settle(je, line, other_debit, None, "s10c").await).contains("on the same side"));
        assert_eq!((s.open(line).await, s.open(other_debit).await), (n("20"), n("5")), "nothing settled");
    });

    part!("settle_line_refuses_a_line_that_is_not_a_partys", |s| {
        let (je, _) = s.refund(&s.globex, "20").await;
        let ours = s.credit_note(&s.globex, "20", None).await;
        let bank_line: Uuid = s.t.get("JournalEntry", je).await["accounts"][1]["id"].as_str().unwrap().parse().unwrap();
        assert!(err(s.settle(je, bank_line, ours, None, "s10d").await).contains("takes a customer's or supplier's line"));
        assert_eq!(s.open(ours).await, n("-20"), "nothing settled");
    });

    part!("settle_line_refuses_another_account", |s| {
        let group = s.t.account("Accounts Receivable").await;
        let abroad = uid(&s
            .t
            .insert(
                "Account",
                json!({"account_name": "Debtors Abroad", "company": s.c, "parent_account": group, "root_type": "Asset", "account_type": "Receivable"}),
            )
            .await)
        .to_string();
        let cn = s.credit_note(&s.globex, "12", None).await;
        let e = s
            .je(json!([{"account": abroad, "party_type": "Customer", "customer": s.globex, "debit": "12"}, {"account": s.inr_bank, "credit": "12"}]))
            .await
            .unwrap_or_else(|e| panic!("a refund on the other receivable: {e}"));
        let line: Uuid = e["accounts"][0]["id"].as_str().unwrap().parse().unwrap();
        assert!(err(s.settle(uid(&e), line, cn, None, "s19").await).contains("on another account"));
    });

    part!("settle_line_refuses_a_line_no_longer_open", |s| {
        let (je, line) = s.refund(&s.globex, "12").await;
        s.settle(je, line, s.credit_note(&s.globex, "12", None).await, None, "s20a").await.unwrap_or_else(|e| panic!("settled: {e}"));
        let another = s.credit_note(&s.globex, "12", None).await;
        assert!(err(s.settle(je, line, another, None, "s20b").await).contains("this line is not open"));
        assert_eq!(s.open(another).await, n("-12"));
    });

    part!("settle_line_refuses_what_is_not_an_open_item", |s| {
        let (je, line) = s.refund(&s.globex, "12").await;
        assert!(err(s.settle(je, line, Uuid::now_v7(), None, "s21").await).contains("is not open"));
        assert_eq!(s.open(line).await, n("12"));
    });

    part!("a_balancing_line_of_nothing_is_refused", |s| {
        // The party's 834.17 against a fee of 834.17 leaves nothing for the balancing line.
        let party = json!({"account": s.debtors, "party_type": "Customer", "customer": s.globex, "currency": s.usd, "exchange_rate": "83.3333", "credit_in_currency": "10.01"});
        let e = err(s.draft(json!([{"account": s.fees, "debit": "834.17"}, {"account": s.rent, "debit": "1", "balances": true}, party])).await);
        assert!(e.contains("would be 0"), "nothing: {e}");
    });

    part!("a_balancing_line_without_a_side_is_refused", |s| {
        let party = json!({"account": s.debtors, "party_type": "Customer", "customer": s.globex, "currency": s.usd, "exchange_rate": "83.3333", "credit_in_currency": "10.01"});
        let e = err(s.draft(json!([{"account": s.fees, "debit": "25"}, {"account": s.rent, "balances": true}, party])).await);
        assert!(e.contains("give the side"), "no side: {e}");
    });

    part!("a_bank_line_needs_a_debit_or_a_credit_in_its_currency", |s| {
        let e = err(s
            .draft(json!([{"account": s.usd_bank, "currency": s.usd, "exchange_rate": "83", "debit_in_currency": "1", "credit_in_currency": "1"},
                          {"account": s.rent, "credit": "83"}]))
            .await);
        assert!(e.contains("one of the two"), "{e}");
    });

    part!("settle_line_once_per_key", |s| {
        let cn = s.credit_note(&s.globex, "60", None).await;
        let (je, line) = s.refund(&s.globex, "100").await;
        let (first, repeated) = s.settle_as(&s.t.ctx, je, line, cn, None, "s11").await.unwrap_or_else(|e| panic!("first: {e}"));
        assert!(!repeated);
        assert_eq!((dec(&first["settled"]), dec(&first["left"])), (n("60"), n("40")));
        let (_, repeated) = s.settle_as(&s.t.ctx, je, line, cn, None, "s11").await.unwrap_or_else(|e| panic!("again: {e}"));
        assert!(repeated, "the same key answers again without settling again");
        assert_eq!((s.open(cn).await, s.open(line).await), (n("0"), n("40")));
        let count = s.t.sql::<(i64,)>("select count(*) from allocation where open_item_id = $1", cn).await[0].0;
        assert_eq!(count, 1);
    });

    part!("two_settles_at_once_take_what_is_open_once", |s| {
        let cn = s.credit_note(&s.globex, "50", None).await;
        let (je, line) = s.refund(&s.globex, "50").await;
        let (a, b) = tokio::join!(s.settle(je, line, cn, None, "s12a"), s.settle(je, line, cn, None, "s12b"));
        assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|x| **x).count(), 1, "{a:?} {b:?}");
        assert_eq!((s.open(cn).await, s.open(line).await), (n("0"), n("0")));
    });

    part!("settle_line_across_two_rates_books_the_difference", |s| {
        // A credit note of 10 USD at 82 settled by a refund of 10 USD at 83: each side gives up its own rupees, and the
        // customer's account in the ledger still agrees with what is open on it.
        let umbrella = s.customer("Umbrella").await;
        let cn = s.credit_note(&umbrella, "10", Some("82")).await;
        let e = s
            .je(json!([{"account": s.debtors, "party_type": "Customer", "customer": umbrella, "currency": s.usd, "exchange_rate": "83", "debit_in_currency": "10"},
                       {"account": s.usd_bank, "currency": s.usd, "exchange_rate": "83", "credit_in_currency": "10"}]))
            .await
            .unwrap_or_else(|e| panic!("refund in dollars: {e}"));
        let line: Uuid = e["accounts"][0]["id"].as_str().unwrap().parse().unwrap();
        let r = s.settle(uid(&e), line, cn, None, "s13").await.unwrap_or_else(|e| panic!("settled: {e}"));
        assert_eq!((dec(&r["settled"]), dec(&r["left"])), (n("10"), n("0")));
        let who: Uuid = umbrella.parse().unwrap();
        let ledger = s.t.sql::<(Decimal,)>("select coalesce(sum(debit - credit), 0) from journal_line where customer_id = $1", who).await[0].0;
        let open =
            s.t.sql::<(Decimal,)>("select coalesce(sum(base_outstanding), 0) from open_item where customer_id = $1 and status <> 'cancelled'", who).await[0].0;
        assert_eq!((ledger, open), (n("0"), n("0")), "the ledger agrees with what is open");
    });

    part!("cancelling_a_settled_journal_is_refused", |s| {
        // As before 0.4.1: what settled it is undone by a new journal, never by a cancel.
        let cn = s.credit_note(&s.globex, "70", None).await;
        let (je, line) = s.refund(&s.globex, "70").await;
        s.settle(je, line, cn, None, "s14").await.unwrap_or_else(|e| panic!("settled: {e}"));
        let e = err(s.t.k.cancel(&s.t.ctx, "JournalEntry", je, None).await.map_err(|e| format!("{e:?}")));
        assert!(e.contains("book a reversing journal"), "{e}");
        assert_eq!((s.open(cn).await, s.open(line).await), (n("0"), n("0")), "still settled");
    });

    part!("lines_on_one_foreign_account_at_two_rates_keep_their_own_amounts", |s| {
        // In 100 USD at 83 and out 99 USD at 84 on one bank: its dollars move +1, on a line each.
        let e = s
            .je(json!([{"account": s.usd_bank, "currency": s.usd, "exchange_rate": "83", "debit_in_currency": "100"},
                       {"account": s.usd_bank, "currency": s.usd, "exchange_rate": "84", "credit_in_currency": "99"},
                       {"account": s.rent, "debit": "1", "balances": true}]))
            .await
            .unwrap_or_else(|e| panic!("posted: {e}"));
        assert_eq!(s.dollars(uid(&e), &s.usd_bank).await, (n("1"), 2));
        // In 100 USD at 83 and out 50 USD at 166, 8,300.00 each: neither line is lost, nor its dollars.
        let e = s
            .je(json!([{"account": s.usd_bank, "currency": s.usd, "exchange_rate": "83", "debit_in_currency": "100"},
                       {"account": s.usd_bank, "currency": s.usd, "exchange_rate": "166", "credit_in_currency": "50"}]))
            .await
            .unwrap_or_else(|e| panic!("posted: {e}"));
        assert_eq!(s.dollars(uid(&e), &s.usd_bank).await, (n("50"), 2));
    });

    part!("a_line_in_rupees_on_a_dollar_account_moves_its_dollars_at_the_days_rate", |s| {
        // 83.10 rupees out of the USD bank, no currency given: 1.00 USD at the day's 83.10.
        let e =
            s.je(json!([{"account": s.usd_bank, "credit": "83.10"}, {"account": s.rent, "debit": "83.10"}])).await.unwrap_or_else(|e| panic!("posted: {e}"));
        assert_eq!(line_of(&s.posted(uid(&e)).await, &s.usd_bank), (n("0"), n("83.10"), n("0"), n("1"), "USD".to_string(), n("83.10")));
        // Less than a cent at that rate is kept as before 0.4.1, marked (DECISIONS "ERP S1-E1, second read" 1: the old
        // caller wins and people are told); it posts in the part an_amount_under_a_cent_posts_as_before_and_is_marked.
        let e = s.draft(json!([{"account": s.usd_bank, "credit": "0.40"}, {"account": s.rent, "debit": "0.40"}])).await.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(e["accounts"][0]["without_rate"], json!(true), "marked: {e:?}");
        // An exchange revaluation changes what the account is worth in rupees alone.
        let e = s
            .t
            .try_post("JournalEntry", json!({"company": s.c, "entry_type": "Exchange Revaluation", "accounts": [{"account": s.usd_bank, "debit": "10"}, {"account": s.rent, "credit": "10"}]}))
            .await
            .unwrap_or_else(|e| panic!("a revaluation: {e}"));
        assert_eq!(line_of(&s.posted(uid(&e)).await, &s.usd_bank).4, "INR", "no dollars move");
    });

    part!("settle_line_takes_a_positive_amount_of_the_lines_currency", |s| {
        let cn = s.credit_note(&s.globex, "100", None).await;
        let (je, line) = s.refund(&s.globex, "100").await;
        for (i, bad) in ["ten", "10 USD", "0", "-5", "33.333"].into_iter().enumerate() {
            let e = err(s.settle(je, line, cn, Some(bad), &format!("s22-{i}")).await);
            assert!(e.contains("field: \"amount\""), "{bad}: {e}");
        }
        assert_eq!((s.open(cn).await, s.open(line).await), (n("-100"), n("100")), "nothing settled");
        let r = s.settle(je, line, cn, Some(""), "s22-all").await.unwrap_or_else(|e| panic!("empty settles all: {e}"));
        assert_eq!(dec(&r["settled"]), n("100"));
    });

    part!("a_settle_is_dated_the_later_of_its_items_or_today_and_never_in_a_closed_period", |s| {
        let dated = |on: chrono::NaiveDate, amount: &'static str| {
            let s = s.clone();
            async move {
                let e =
                    s.t.try_post(
                        "JournalEntry",
                        json!({"company": s.c, "posting_date": on.to_string(), "accounts": [
                        {"account": s.debtors, "party_type": "Customer", "customer": s.globex, "debit": amount}, {"account": s.inr_bank, "credit": amount}]}),
                    )
                    .await
                    .unwrap_or_else(|e| panic!("a refund dated {on}: {e}"));
                (uid(&e), e["accounts"][0]["id"].as_str().unwrap().parse::<Uuid>().unwrap())
            }
        };
        let later = s.t.ctx.today() + chrono::Days::new(10);
        let cn = s.credit_note(&s.globex, "15", None).await;
        let (je, line) = dated(later, "15").await;
        s.settle(je, line, cn, None, "s23a").await.unwrap_or_else(|e| panic!("settled: {e}"));
        let on = s.t.sql::<(chrono::NaiveDate,)>("select max(posting_date) from allocation where open_item_id = $1", cn).await[0].0;
        assert_eq!(on, later, "on the refund's own day");
        // The books closed on the day it would be dated: refused, though nothing would be posted.
        let far = s.t.ctx.today() + chrono::Days::new(40);
        let cn = s.credit_note(&s.globex, "15", None).await;
        let (je, line) = dated(far, "15").await;
        let company: Uuid = s.c.parse().unwrap();
        let mut tx = s.t.k.db.tenant_tx(s.t.ctx.tenant).await.unwrap();
        sqlx::query("update accounting_period set status = 'closed' where tenant_id = $1 and company_id = $2 and $3 between period_start and period_end")
            .bind(s.t.ctx.tenant)
            .bind(company)
            .bind(far)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let e = err(s.settle(je, line, cn, None, "s23b").await);
        let mut tx = s.t.k.db.tenant_tx(s.t.ctx.tenant).await.unwrap();
        sqlx::query("update accounting_period set status = 'open' where tenant_id = $1 and company_id = $2 and $3 between period_start and period_end")
            .bind(s.t.ctx.tenant)
            .bind(company)
            .bind(far)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert!(e.contains("closed"), "{e}");
        assert_eq!(s.open(cn).await, n("-15"));
    });

    part!("two_settles_at_once_on_two_journals_take_what_is_open_once", |s| {
        // Two refunds of 50 settle one credit note of 80 at once: the second waits for the note and takes the 30 left.
        let cn = s.credit_note(&s.globex, "80", None).await;
        let ((je1, l1), (je2, l2)) = (s.refund(&s.globex, "50").await, s.refund(&s.globex, "50").await);
        let mut tx = s.t.k.db.tenant_tx(s.t.ctx.tenant).await.unwrap();
        s.t.k
            .run_action_tx(&s.t.ctx, &mut tx, "JournalEntry", je1, "settle_line", &json!({"line": l1.to_string(), "against": cn.to_string()}))
            .await
            .expect("the first, not yet committed");
        let s2 = s.clone();
        let second = tokio::spawn(async move { s2.settle(je2, l2, cn, None, "s24").await });
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        tx.commit().await.unwrap();
        let r = second.await.unwrap().unwrap_or_else(|e| panic!("the second waits and takes what is left: {e}"));
        assert_eq!((dec(&r["settled"]), dec(&r["left"]), s.open(cn).await), (n("30"), n("20"), n("0")));
    });

    part!("settles_across_two_rates_book_a_loss_or_a_gain_for_customers_and_suppliers", |s| {
        // (party, owed at, settled at, the exchange account's debit less credit: a loss positive, a gain negative)
        for (i, (customer, owed_at, paid_at, loss)) in
            [(true, "84", "83", "-10"), (false, "82", "83", "10"), (false, "82", "81", "-10")].into_iter().enumerate()
        {
            let party = if customer { s.customer(&format!("Rated {i}")).await } else { s.supplier(&format!("Rated {i}")).await };
            let (kind, field, account) = if customer { ("Customer", "customer", &s.debtors) } else { ("Supplier", "supplier", &s.creditors) };
            // The party's line of 10 USD at `rate` on `side`.
            let party_line = |side: &str, rate: &str| {
                let mut o = serde_json::Map::new();
                for (k, v) in [
                    ("account", account.as_str()),
                    ("party_type", kind),
                    (field, party.as_str()),
                    ("currency", s.usd.as_str()),
                    ("exchange_rate", rate),
                    (side, "10"),
                ] {
                    o.insert(k.to_string(), json!(v));
                }
                Value::Object(o)
            };
            // What is owed: a credit note to the customer; a bill from the supplier, as a journal.
            let owed = if customer {
                s.credit_note(&party, "10", Some(owed_at)).await
            } else {
                let e = s
                    .je(json!([{"account": s.rent, "debit": "1", "balances": true}, party_line("credit_in_currency", owed_at)]))
                    .await
                    .unwrap_or_else(|e| panic!("the bill: {e}"));
                e["accounts"][1]["id"].as_str().unwrap().parse().unwrap()
            };
            let e = s
                .je(json!([party_line("debit_in_currency", paid_at),
                           {"account": s.usd_bank, "currency": s.usd, "exchange_rate": paid_at, "credit_in_currency": "10"}]))
                .await
                .unwrap_or_else(|e| panic!("paid back or paid: {e}"));
            let line: Uuid = e["accounts"][0]["id"].as_str().unwrap().parse().unwrap();
            s.settle(uid(&e), line, owed, None, &format!("s25-{i}")).await.unwrap_or_else(|e| panic!("settled: {e}"));
            let (dr, cr, ..) = line_of(&s.posted(uid(&e)).await, &s.exchange);
            assert_eq!(dr - cr, n(loss), "case {i}");
            assert_eq!(s.agrees(&party).await, (n("0"), n("0")), "case {i}: the ledger agrees with what is open");
        }
    });

    part!("three_partial_settles_leave_the_ledger_agreeing_with_what_is_open", |s| {
        let who = s.customer("Hooli").await;
        let cn = s.credit_note(&who, "100", Some("82.333")).await;
        let e = s
            .je(json!([{"account": s.debtors, "party_type": "Customer", "customer": who, "currency": s.usd, "exchange_rate": "83.777", "debit_in_currency": "100"},
                       {"account": s.usd_bank, "currency": s.usd, "exchange_rate": "83.777", "credit_in_currency": "100"}]))
            .await
            .unwrap_or_else(|e| panic!("refund in dollars: {e}"));
        let line: Uuid = e["accounts"][0]["id"].as_str().unwrap().parse().unwrap();
        for (k, amount) in [("s26a", Some("33.33")), ("s26b", Some("33.33")), ("s26c", None)] {
            s.settle(uid(&e), line, cn, amount, k).await.unwrap_or_else(|e| panic!("{k}: {e}"));
        }
        assert_eq!((s.open(cn).await, s.open(line).await), (n("0"), n("0")));
        assert_eq!(s.agrees(&who).await, (n("0"), n("0")));
    });

    part!("amounts_finer_than_the_currency_are_refused_when_saved", |s| {
        let e = err(s
            .draft(
                json!([{"account": s.fees, "debit": "10.005"}, {"account": s.fees, "debit": "10.005"}, {"account": s.rent, "credit": "1", "balances": true}]),
            )
            .await);
        assert!(e.contains("field: \"debit\""), "rupees: {e}");
        let e = err(s.draft(json!([{"account": s.usd_bank, "currency": s.usd, "exchange_rate": "83", "debit_in_currency": "2.505"}, {"account": s.rent, "credit": "1", "balances": true}])).await);
        assert!(e.contains("field: \"debit_in_currency\""), "dollars: {e}");
    });

    part!("companies_with_no_decimals_and_with_three", |s| {
        // 10.01 USD into each company's own USD bank: 1,503 yen at 150.1234, 3.074 dinars at 0.3071; the fee balances
        // it to the last unit; an amount finer than the currency is refused.
        for (code, rate, expect, finer) in [("JPY", "150.1234", "1503", "100.5"), ("KWD", "0.3071", "3.074", "1.0005")] {
            let cur = s.t.find_one("Currency", json!({"code": code})).await["id"].as_str().unwrap().to_string();
            let co = uid(&s.t.insert("Company", json!({"company_name": format!("{code} Co"), "abbr": code, "default_currency": cur})).await).to_string();
            let acc = async |name: &str| s.t.find_one("Account", json!({"account_name": name, "company": co})).await["id"].as_str().unwrap().to_string();
            let (groups, fees, rent) = (acc("Bank Accounts").await, acc("Bank Charges").await, acc("Rent").await);
            let bank = uid(&s
                .t
                .insert("Account", json!({"account_name": "USD Bank", "company": co, "parent_account": groups, "root_type": "Asset", "account_type": "Bank", "account_currency": s.usd}))
                .await)
            .to_string();
            let e = s
                .t
                .try_post("JournalEntry", json!({"company": co, "accounts": [{"account": fees, "debit": "1", "balances": true},
                                                                             {"account": bank, "currency": s.usd, "exchange_rate": rate, "credit_in_currency": "10.01"}]}))
                .await
                .unwrap_or_else(|e| panic!("{code}: {e}"));
            assert_eq!((dec(&e["accounts"][0]["debit"]), dec(&e["total_credit"])), (n(expect), n(expect)), "{code}");
            let e = err(s
                .t
                .k
                .insert(
                    &s.t.ctx,
                    "JournalEntry",
                    obj(json!({"company": co, "accounts": [{"account": fees, "debit": finer}, {"account": rent, "credit": finer}]})),
                )
                .await
                .map_err(|e| format!("{e:?}")));
            assert!(e.contains("field: \"debit\""), "{code}: {e}");
            s.t.try_post(
                "JournalEntry",
                json!({"company": co, "accounts": [
                {"account": bank, "currency": s.usd, "exchange_rate": rate, "debit_in_currency": "2.5", "debit": "2.5"},
                {"account": fees, "credit": "2.5", "balances": true}]}),
            )
            .await
            .unwrap_or_else(|e| panic!("{code}: the stand-ins are worked out, not refused: {e}"));
        }
    });

    part!("settle_line_needs_submit_on_journal_entries", |s| {
        // An Accounts User makes journals but does not submit them.
        let user = s.t.k.create_user(&s.t.ctx, "clerk@fin.test", "Clerk", Some("Correct-Horse-9"), &["Accounts User".into()], false).await.expect("user");
        let clerk = s.t.k.load_ctx(s.t.ctx.tenant, user, None, "it".into()).await.unwrap();
        let cn = s.credit_note(&s.globex, "5", None).await;
        let (je, line) = s.refund(&s.globex, "5").await;
        assert!(s.settle_as(&clerk, je, line, cn, None, "s15a").await.is_err(), "no submit right");
        assert_eq!(s.open(cn).await, n("-5"));
        s.settle(je, line, cn, None, "s15b").await.unwrap_or_else(|e| panic!("an Accounts Manager settles it: {e}"));
        assert_eq!(s.open(cn).await, n("0"));
    });

    part!("a_line_in_rupees_without_any_rate_posts_as_before_and_is_marked", |s| {
        // No USD rate 30 days ago: 500 rupees out of the USD bank posts as before 0.4.1, marked, and the accounts
        // managers are told; a rate typed on the line is used.
        let user = s.t.k.create_user(&s.t.ctx, "books@fin.test", "Books", Some("Correct-Horse-9"), &["Accounts Manager".into()], false).await.expect("user");
        let early = (s.t.ctx.today() - chrono::Days::new(30)).to_string();
        let post = |lines: Value| {
            let s = s.clone();
            let early = early.clone();
            async move { s.t.try_post("JournalEntry", json!({"company": s.c, "posting_date": early, "accounts": lines})).await }
        };
        let e = post(json!([{"account": s.usd_bank, "credit": "500"}, {"account": s.fees, "debit": "500"}]))
            .await
            .unwrap_or_else(|e| panic!("posts as before: {e}"));
        assert_eq!(e["accounts"][0]["without_rate"], json!(true));
        assert_eq!(line_of(&s.posted(uid(&e)).await, &s.usd_bank).4, "INR");
        let told = s.t.sql::<(i64,)>("select count(*) from notification where user_id = $1", user).await[0].0;
        assert_eq!(told, 1, "the accounts manager is told once");
        let e = post(json!([{"account": s.usd_bank, "credit": "831", "exchange_rate": "83.10"}, {"account": s.fees, "debit": "831"}]))
            .await
            .unwrap_or_else(|e| panic!("the rate given: {e}"));
        assert_eq!(line_of(&s.posted(uid(&e)).await, &s.usd_bank), (n("0"), n("831"), n("0"), n("10"), "USD".to_string(), n("83.10")));
    });

    part!("an_account_kept_in_dollars_takes_its_own_currency_whatever_its_type", |s| {
        // 1,000 USD of a dollar loan repaid at the bank's 83.25: the loan's dollars move 1,000.
        let group = s.t.account("Current Liabilities").await;
        let loan = uid(&s
            .t
            .insert(
                "Account",
                json!({"account_name": "USD Loan", "company": s.c, "parent_account": group, "root_type": "Liability", "account_currency": s.usd}),
            )
            .await)
        .to_string();
        let e = s
            .je(json!([{"account": loan, "currency": s.usd, "exchange_rate": "83.25", "debit_in_currency": "1000"}, {"account": s.inr_bank, "credit": "83250"}]))
            .await
            .unwrap_or_else(|e| panic!("a loan line in dollars: {e}"));
        assert_eq!(s.dollars(uid(&e), &loan).await, (n("1000"), 1));
    });

    part!("a_settle_dated_in_no_accounting_period_is_refused", |s| {
        let cn = s.credit_note(&s.globex, "9", None).await;
        let (je, line) = s.refund(&s.globex, "9").await;
        let company: Uuid = s.c.parse().unwrap();
        let today = s.t.ctx.today();
        let (start, end) =
            s.t.sql::<(chrono::NaiveDate, chrono::NaiveDate)>(
                &format!(
                    "select period_start, period_end from accounting_period where company_id = $1 and '{today}'::date between period_start and period_end"
                ),
                company,
            )
            .await[0];
        let sql = |q: &'static str| {
            let s = s.clone();
            async move {
                let mut tx = s.t.k.db.tenant_tx(s.t.ctx.tenant).await.unwrap();
                sqlx::query(q).bind(s.t.ctx.tenant).bind(company).bind(start).bind(end).execute(&mut *tx).await.unwrap();
                tx.commit().await.unwrap();
            }
        };
        sql("delete from accounting_period where tenant_id = $1 and company_id = $2 and period_start = $3 and period_end = $4").await;
        let e = err(s.settle(je, line, cn, None, "s27").await);
        sql("insert into accounting_period (tenant_id, company_id, period_start, period_end) values ($1, $2, $3, $4)").await;
        assert!(e.contains("no accounting period"), "{e}");
        assert_eq!(s.open(cn).await, n("-9"));
    });

    part!("the_other_side_of_a_settled_pair_says_to_book_a_reversing_journal", |s| {
        // A credit note and another journal's line that refund journals were settled against: their cancels are refused
        // naming the way that exists (a settling journal is never cancelled), not "cancel those payments first".
        let umbrella = s.customer("Umbrella").await;
        let cn = s.credit_note(&umbrella, "11", None).await;
        let (je, line) = s.refund(&umbrella, "11").await;
        s.settle(je, line, cn, None, "s31a").await.unwrap_or_else(|e| panic!("settled: {e}"));
        let e = err(s.t.k.cancel(&s.t.ctx, "SalesInvoice", cn, None).await.map_err(|e| format!("{e:?}")));
        assert!(e.contains("book a reversing journal"), "the credit note: {e}");
        let received = s
            .je(json!([{"account": s.debtors, "party_type": "Customer", "customer": umbrella, "credit": "12"}, {"account": s.inr_bank, "debit": "12"}]))
            .await
            .unwrap_or_else(|e| panic!("received by journal: {e}"));
        let held: Uuid = received["accounts"][0]["id"].as_str().unwrap().parse().unwrap();
        let (je, line) = s.refund(&umbrella, "12").await;
        s.settle(je, line, held, None, "s31b").await.unwrap_or_else(|e| panic!("settled: {e}"));
        let e = err(s.t.k.cancel(&s.t.ctx, "JournalEntry", uid(&received), None).await.map_err(|e| format!("{e:?}")));
        assert!(e.contains("book a reversing journal"), "the journal: {e}");
    });

    part!("an_amount_under_a_cent_posts_as_before_and_is_marked", |s| {
        // 0.40 rupees out of the USD bank at the day's 83.10 is less than a cent: posted as before 0.4.1, marked.
        let e = s
            .je(json!([{"account": s.usd_bank, "credit": "0.40"}, {"account": s.fees, "debit": "0.40"}]))
            .await
            .unwrap_or_else(|e| panic!("posts as before: {e}"));
        assert_eq!(e["accounts"][0]["without_rate"], json!(true));
        let l = line_of(&s.posted(uid(&e)).await, &s.usd_bank);
        assert_eq!((l.1, l.4.as_str()), (n("0.40"), "INR"), "in rupees alone: {l:?}");
    });

    part!("the_notice_names_the_step_that_moves_the_accounts_own_balance", |s| {
        // The notice of a line posted without its account's currency names an Exchange Revaluation journal, and that
        // journal moves the account's dollars alone: 6.02 out, the rupees net to nothing.
        let early = (s.t.ctx.today() - chrono::Days::new(30)).to_string();
        let e =
            s.t.try_post(
                "JournalEntry",
                json!({"company": s.c, "posting_date": early, "accounts": [{"account": s.usd_bank, "credit": "500"}, {"account": s.fees, "debit": "500"}]}),
            )
            .await
            .unwrap_or_else(|e| panic!("posts as before: {e}"));
        let body = s.t.sql::<(Option<String>,)>("select body from notification where record_id = $1", uid(&e)).await;
        assert!(!body.is_empty() && body.iter().all(|b| b.0.as_deref().is_some_and(|b| b.contains("Exchange Revaluation"))), "{body:?}");
        let r =
            s.t.try_post(
                "JournalEntry",
                json!({"company": s.c, "entry_type": "Exchange Revaluation", "accounts": [
                    {"account": s.usd_bank, "currency": s.usd, "exchange_rate": "83.10", "credit_in_currency": "6.02"},
                    {"account": s.usd_bank, "debit": "1", "balances": true}]}),
            )
            .await
            .unwrap_or_else(|e| panic!("the step it names: {e}"));
        let posted = s.posted(uid(&r)).await;
        let rupees: Decimal = posted.iter().map(|l| l.1 - l.2).sum();
        let dollars: Decimal = posted.iter().filter(|l| l.5 == "USD").map(|l| l.3 - l.4).sum();
        assert_eq!((rupees, dollars), (n("0"), n("-6.02")), "{posted:?}");
    });

    part!("a_refused_recurring_journal_is_told_once", |s| {
        // A template whose line holds 1,234.567 rupees is refused at every round of the upkeep (about once a minute): its
        // company's accounts managers are told once, again when the template is changed, and not once it is mended.
        let m = s.manager("templates@fin.test").await;
        let today = s.t.ctx.today();
        let tid = uid(&s
            .t
            .insert(
                "JournalTemplate",
                json!({"template_name": "Finer rent", "company": s.c, "repeat": true, "frequency": "Monthly", "start_date": today.to_string(),
                       "occurrences": 1, "submit": true, "accounts": [{"account": s.rent, "debit": "1234.567"}, {"account": s.cash, "credit": "1234.567"}]}),
            )
            .await);
        let round = || ferrum_app_erp::journal_template::upkeep(&s.t.k, &s.t.ctx, today);
        assert_eq!(round().await.unwrap(), 0, "refused");
        assert_eq!(round().await.unwrap(), 0, "refused again");
        assert_eq!(s.told(m, tid).await, 1, "told once, not at every round");
        s.t.k.update(&s.t.ctx, "JournalTemplate", tid, obj(json!({"remark": "looked at"})), None).await.expect("changed");
        round().await.unwrap();
        assert_eq!(s.told(m, tid).await, 2, "told again once the template was changed");
        let mended = json!({"accounts": [{"account": s.rent, "debit": "1234.57"}, {"account": s.cash, "credit": "1234.57"}]});
        s.t.k.update(&s.t.ctx, "JournalTemplate", tid, obj(mended), None).await.expect("mended");
        assert_eq!(round().await.unwrap(), 1, "made once mended");
        assert_eq!(s.told(m, tid).await, 2);
    });

    part!("an_entry_not_reversed_on_its_day_is_told_once", |s| {
        // An accrual whose reversal day is in no accounting period cannot be reversed: two rounds at once and a third tell
        // its company's accounts managers once; another reason tells them again; once it can be, it is reversed.
        let m = s.manager("accruals@fin.test").await;
        let e = s
            .t
            .try_post(
                "JournalEntry",
                json!({"company": s.c, "auto_reverse": true, "reversal_date": "2999-01-01", "accounts": [{"account": s.rent, "debit": "3"}, {"account": s.cash, "credit": "3"}]}),
            )
            .await
            .unwrap_or_else(|e| panic!("the accrual: {e}"));
        let jid = [uid(&e)];
        let back_on = |day: String| s.exec(format!("update e_journal_entry set reversal_date = '{day}' where tenant_id = $1 and id = any($2)"), &jid);
        let jid = jid[0];
        back_on("2001-01-01".into()).await;
        let round = || ferrum_app_erp::accounting::upkeep(&s.t.k, &s.t.ctx);
        let (a, b) = tokio::join!(round(), round());
        a.unwrap();
        b.unwrap();
        round().await.unwrap();
        assert_eq!(s.told(m, jid).await, 1, "told once, not at every round");
        back_on("2001-02-01".into()).await;
        round().await.unwrap();
        assert_eq!(s.told(m, jid).await, 2, "told again for another reason");
        let today = s.t.ctx.today().to_string();
        back_on(today.clone()).await;
        round().await.unwrap();
        assert_eq!(s.t.get("JournalEntry", jid).await["reversed_on"], json!(today));
        assert_eq!(s.told(m, jid).await, 2);
    });

    part!("the_notice_goes_to_the_companys_managers_with_a_current_grant_else_the_administrators", |s| {
        // Four hold Accounts Manager: one as the platform counts role holders, one whose grant has run out, a service
        // account, one kept to another company. A journal of this company posted without a rate tells the first alone;
        // with no Accounts Manager left to the company, its administrators are told.
        let (current, lapsed, robot, elsewhere) =
            (s.manager("current@fin.test").await, s.manager("lapsed@fin.test").await, s.manager("robot@fin.test").await, s.manager("elsewhere@fin.test").await);
        s.exec("update user_role set expires_at = now() - interval '1 minute' where tenant_id = $1 and user_id = any($2)".into(), &[lapsed]).await;
        s.exec("update app_user set kind = 'service' where tenant_id = $1 and id = any($2)".into(), &[robot]).await;
        s.exec(
            format!(
                "insert into user_scope (tenant_id, user_id, entity, value, is_default) select $1, u, 'Company', '{}', true from unnest($2::uuid[]) u",
                Uuid::now_v7()
            ),
            &[elsewhere],
        )
        .await;
        let early = (s.t.ctx.today() - chrono::Days::new(30)).to_string();
        let post = || {
            s.t.try_post(
                "JournalEntry",
                json!({"company": s.c, "posting_date": early, "accounts": [{"account": s.usd_bank, "credit": "7"}, {"account": s.fees, "debit": "7"}]}),
            )
        };
        let e = uid(&post().await.unwrap_or_else(|e| panic!("posts: {e}")));
        let told = [s.told(current, e).await, s.told(lapsed, e).await, s.told(robot, e).await, s.told(elsewhere, e).await];
        assert_eq!(told, [1, 0, 0, 0], "current, lapsed, service, another company's");
        let mut tx = s.t.k.db.tenant_tx(s.t.ctx.tenant).await.unwrap();
        let held: Vec<Uuid> = sqlx::query_scalar(
            "update user_role set expires_at = now() - interval '1 minute' where tenant_id = $1 and role = 'Accounts Manager' \
             and (expires_at is null or expires_at > now()) returning user_id",
        )
        .bind(s.t.ctx.tenant)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let none = post().await;
        s.exec("update user_role set expires_at = null where tenant_id = $1 and role = 'Accounts Manager' and user_id = any($2)".into(), &held).await;
        let e = uid(&none.unwrap_or_else(|e| panic!("posts: {e}")));
        assert_eq!((s.told(s.t.ctx.user, e).await, s.told(current, e).await), (1, 0), "the administrators");
    });

    assert!(failed.is_empty(), "parts that failed: {failed:#?}");
}
