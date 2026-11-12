//! Recurring journals (FN-06, FN-07) against a real PostgreSQL (`FERRUM_ERP_RECURRING_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_recurring_test`; reset first): a
//! monthly template started two months ago makes this month's and the two missed, each once, submitted
//! as it says; two upkeeps at once make a draft template's periods once; an accrual template's
//! entries are taken out again the day after; a start on the 31st falls on the months' last days.

mod common;
use chrono::{Datelike, NaiveDate};
use common::*;
use ferrum_app_erp::journal_template::{period, upkeep};
use serde_json::{Value, json};

#[test]
fn recurring_journals() {
    run(scenario());
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_RECURRING_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_recurring_test".into());
    let t = T::boot(url, |_, _| {}).await;
    let c = t.company.clone();
    let rent = t.account("Rent").await;
    let cash = t.account("Cash").await;
    let today = t.ctx.today();
    let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
    let start = period(first, Some("Monthly"), 0).unwrap().checked_sub_months(chrono::Months::new(2)).unwrap();
    let template = |name: &str, submit: bool, reverse: bool| {
        json!({"template_name": name, "company": c, "repeat": true, "frequency": "Monthly", "start_date": start.to_string(), "occurrences": 5,
               "submit": submit, "auto_reverse": reverse,
               "accounts": [{"account": rent, "debit": "100"}, {"account": cash, "credit": "100"}]})
    };
    let rent_t = uid(&t.insert("JournalTemplate", template("Rent", true, false)).await);
    assert_eq!(t.get("JournalTemplate", rent_t).await["next_date"], json!(start.to_string()));
    assert_eq!(upkeep(&t.k, &t.ctx, today).await.unwrap(), 3, "two missed months and this one");
    assert_eq!(upkeep(&t.k, &t.ctx, today).await.unwrap(), 0, "each once");
    let made = t.find("JournalEntry", json!({"journal_template": rent_t.to_string()})).await;
    let mut days: Vec<String> = made.iter().map(|j| j["posting_date"].as_str().unwrap().to_string()).collect();
    days.sort();
    assert_eq!(days, (0..3).map(|n| period(start, Some("Monthly"), n).unwrap().to_string()).collect::<Vec<_>>());
    assert!(made.iter().all(|j| j["docstatus"] == json!(1)));
    let after = t.get("JournalTemplate", rent_t).await;
    assert_eq!((after["made_count"].clone(), after["next_date"].clone()), (json!(3), json!(period(start, Some("Monthly"), 3).unwrap().to_string())));

    // Two upkeeps at once: a draft template's periods are made once.
    let drafts = uid(&t.insert("JournalTemplate", template("Accrual drafts", false, false)).await);
    let (a, b) = tokio::join!(upkeep(&t.k, &t.ctx, today), upkeep(&t.k, &t.ctx, today));
    assert_eq!(a.unwrap() + b.unwrap(), 3);
    let made = t.find("JournalEntry", json!({"journal_template": drafts.to_string()})).await;
    assert_eq!(made.len(), 3);
    assert!(made.iter().all(|j| j["docstatus"] == json!(0)), "left as drafts to check");

    // An accrual: each entry is taken out again the day after.
    let accrual = uid(&t.insert("JournalTemplate", template("Accrual", true, true)).await);
    upkeep(&t.k, &t.ctx, today).await.unwrap();
    let made = t.find("JournalEntry", json!({"journal_template": accrual.to_string()})).await;
    let reversed = made.iter().filter(|j| j["reversed_on"] != Value::Null).count();
    assert!(reversed >= 2, "the past months' are reversed: {reversed}");

    // A start on the 31st: the months' last days.
    let jan31 = NaiveDate::from_ymd_opt(2027, 1, 31).unwrap();
    assert_eq!(period(jan31, Some("Monthly"), 1), NaiveDate::from_ymd_opt(2027, 2, 28));
    assert_eq!(period(jan31, Some("Monthly"), 2), NaiveDate::from_ymd_opt(2027, 3, 31));
    assert_eq!(period(jan31, Some("Quarterly"), 1), NaiveDate::from_ymd_opt(2027, 4, 30));
}
