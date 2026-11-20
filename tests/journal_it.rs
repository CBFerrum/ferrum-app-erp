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
    globex: String,
    initech: String,
}

impl S {
    async fn customer(&self, name: &str) -> String {
        uid(&self.t.insert("Customer", json!({"customer_name": name})).await).to_string()
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
            assert!(e.contains("or a line on a bank or cash account kept in it"), "{e}");
        }
    });

    part!("the_days_rate_is_taken_when_none_is_given", |s| {
        // 10 USD at the day's 83.10.
        let e = s
            .je(json!([{"account": s.usd_bank, "currency": s.usd, "debit_in_currency": "10"}, {"account": s.inr_bank, "credit": "831"}]))
            .await
            .unwrap_or_else(|e| panic!("a bank line without a rate posts at the day's: {e}"));
        assert_eq!((dec(&e["accounts"][0]["exchange_rate"]), dec(&e["accounts"][0]["debit"])), (n("83.10"), n("831")));
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

    part!("settle_line_refuses_another_party_or_currency", |s| {
        let (je, line) = s.refund(&s.globex, "20").await;
        let theirs = s.credit_note(&s.initech, "20", None).await;
        assert!(err(s.settle(je, line, theirs, None, "s10a").await).contains("another customer's or supplier's"));
        let dollars = s.credit_note(&s.globex, "20", Some("82")).await;
        assert!(err(s.settle(je, line, dollars, None, "s10b").await).contains("in another currency"));
        let (_, other_debit) = s.refund(&s.globex, "5").await;
        assert!(err(s.settle(je, line, other_debit, None, "s10c").await).contains("on the same side"));
        let ours = s.credit_note(&s.globex, "20", None).await;
        let bank_line: Uuid = s.t.get("JournalEntry", je).await["accounts"][1]["id"].as_str().unwrap().parse().unwrap();
        assert!(err(s.settle(je, bank_line, ours, None, "s10d").await).contains("takes a customer's or supplier's line"));
        assert_eq!((s.open(line).await, s.open(ours).await, s.open(theirs).await), (n("20"), n("-20"), n("-20")), "nothing settled");
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

    part!("cancelling_a_settled_journal_opens_both_again", |s| {
        let cn = s.credit_note(&s.globex, "70", None).await;
        let (je, line) = s.refund(&s.globex, "70").await;
        s.settle(je, line, cn, None, "s14").await.unwrap_or_else(|e| panic!("settled: {e}"));
        s.t.k.cancel(&s.t.ctx, "JournalEntry", je, None).await.unwrap_or_else(|e| panic!("the settled refund is cancelled: {e:?}"));
        assert_eq!(s.open(cn).await, n("-70"), "the credit note is owed again");
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

    assert!(failed.is_empty(), "parts that failed: {failed:#?}");
}
