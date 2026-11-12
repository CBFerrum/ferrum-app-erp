//! Accounting dimensions (FN-01) against a real PostgreSQL (`FERRUM_ERP_DIMS_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_dims_test`; reset first): a list
//! dimension required on expense accounts with a default by item; values on journal lines and on an
//! invoice reach the ledger, a wrong or missing one is refused, a line from before reads as none and
//! its cancellation still goes through; the trial balance, general ledger and statements filter by
//! it and the profit and loss groups by it, its columns adding up to the whole.

mod common;
use common::*;
use rust_decimal::Decimal;
use serde_json::{Value, json};

#[test]
fn dimensions() {
    run(scenario());
}

fn total(report: &Value, account: &str, key: &str) -> Decimal {
    report["rows"].as_array().unwrap().iter().find(|r| r["account"] == json!(account)).map(|r| dec(&r[key])).unwrap_or_default()
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_DIMS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_dims_test".into());
    let t = T::boot(url, |_, _| {}).await;
    let c = t.company.clone();
    let rent = t.account("Rent").await;
    let cash = t.account("Cash").await;
    let je = |amount: &str, dims: Value| {
        json!({"company": c, "accounts": [
            {"account": rent, "debit": amount, "dimensions": dims},
            {"account": cash, "credit": amount}]})
    };
    // Before any dimension: booked as none.
    let before = t.post("JournalEntry", je("40", Value::Null)).await;

    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let cleaning = uid(&t
        .insert("Item", json!({"item_code": "Cleaning", "item_name": "Cleaning", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "30", "is_stock_item": false, "expense_account": rent}))
        .await)
    .to_string();
    t.insert(
        "AccountingDimension",
        json!({"dimension_name": "Department", "code": "department", "source": "List",
               "values": [{"value": "Sales"}, {"value": "Ops"}],
               "required_for": [{"root_type": "Expense"}],
               "defaults": [{"item": cleaning, "value": "Ops"}]}),
    )
    .await;
    let bad = t.k.insert(&t.ctx, "AccountingDimension", obj(json!({"dimension_name": "Built in", "code": "project", "source": "List"}))).await;
    assert!(bad.is_err(), "cost_center and project are already dimensions");

    let sales = t.post("JournalEntry", je("100", json!({"department": "Sales"}))).await;
    let lines: Vec<(Value,)> =
        t.sql("select l.dims from journal_line l join journal_entry e on e.id = l.entry_id where e.source_id = $1 order by l.debit desc", uid(&sales)).await;
    assert_eq!(lines.iter().map(|l| l.0.clone()).collect::<Vec<_>>(), vec![json!({"department": "Sales"}), json!({})], "on the rent line only");
    let missing = t.try_post("JournalEntry", je("10", Value::Null)).await.unwrap_err();
    assert!(missing.contains("needs the dimension Department"), "{missing}");
    let wrong = t.try_post("JournalEntry", je("10", json!({"department": "Legal"}))).await.unwrap_err();
    assert!(wrong.contains("Legal is not one of the values"), "{wrong}");
    let unknown = t.try_post("JournalEntry", je("10", json!({"department": "Sales", "region": "North"}))).await.unwrap_err();
    assert!(unknown.contains("no accounting dimension region"), "{unknown}");

    // A bill's line takes the item's default; the supplier and tax lines are the bill's (none here).
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await).to_string();
    let bill = t
        .post("PurchaseInvoice", json!({"company": c, "supplier": acme, "bill_no": "B-1", "items": [{"item": cleaning, "qty": "1", "price_list_rate": "30"}]}))
        .await;
    let lines: Vec<(Value,)> = t.sql("select l.dims from journal_line l join journal_entry e on e.id = l.entry_id join e_account a on a.id = l.account_id where e.source_id = $1 and a.root_type = 'Expense'", uid(&bill)).await;
    let all: Vec<(String, Value, Decimal, Decimal)> = t.sql("select a.account_name, l.dims, l.debit, l.credit from journal_line l join journal_entry e on e.id = l.entry_id join e_account a on a.id = l.account_id where e.source_id = $1", uid(&bill)).await;
    assert_eq!(lines.first().map(|l| l.0.clone()), Some(json!({"department": "Ops"})), "the default by item: {all:?} {bill:?}");

    // Filters: Sales is the 100, Ops the 30; the 40 from before is in neither.
    let day = sales["posting_date"].as_str().unwrap().to_string();
    let period = [("from_date", day.as_str()), ("to_date", day.as_str())];
    let pl = |extra: Vec<(&'static str, &'static str)>| {
        let mut p: Vec<(&str, &str)> = period.to_vec();
        p.extend(extra);
        let t = &t;
        async move { t.report("profit_and_loss", &p).await }
    };
    let whole = pl(vec![]).await;
    assert_eq!(dec(&whole["totals"]["expense"]), Decimal::from(170));
    let only = pl(vec![("dimension", "department"), ("dimension_value", "Sales")]).await;
    assert_eq!(dec(&only["totals"]["expense"]), Decimal::from(100));
    let grouped = pl(vec![("group_by", "department")]).await;
    let labels: Vec<String> = grouped["meta"]["groups"].as_array().unwrap().iter().map(|g| g["label"].as_str().unwrap().to_string()).collect();
    assert_eq!(labels, ["Ops", "Sales", "No department"]);
    let row = grouped["rows"].as_array().unwrap().iter().find(|r| r["account"] == json!("Total Expenses")).unwrap().clone();
    assert_eq!((dec(&row["amount"]), dec(&row["amount_1"]), dec(&row["amount_2"]), dec(&row["total"])), (30.into(), 100.into(), 40.into(), 170.into()));
    let tb = t.report("trial_balance", &[("from_date", &day), ("to_date", &day), ("dimension", "department"), ("dimension_value", "Ops")]).await;
    assert_eq!(total(&tb, "5230 - Rent", "debit"), Decimal::from(30));
    let gl = t.report("general_ledger", &[("from_date", &day), ("to_date", &day), ("dimension", "department"), ("dimension_value", "Sales")]).await;
    assert_eq!(dec(&gl["rows"].as_array().unwrap().last().unwrap()["debit"]), Decimal::from(100));
    let bs = t.report("balance_sheet", &[("as_of", &day), ("group_by", "department")]).await;
    assert!(bs["meta"]["groups"].as_array().unwrap().len() == 3);
    // Every statement that filters by a dimension groups by one too.
    let tb = t.report("trial_balance", &[("from_date", &day), ("to_date", &day), ("group_by", "department")]).await;
    let rent = tb["rows"].as_array().unwrap().iter().find(|r| r["account"] == json!("5230 - Rent")).unwrap().clone();
    assert_eq!(dec(&rent["amount"]), Decimal::from(30), "Ops' rent in the period: {rent}");
    let gl = t.report("general_ledger", &[("from_date", &day), ("to_date", &day), ("group_by", "department")]).await;
    let subtotals: Vec<(String, Decimal)> = gl["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["account"].as_str().is_some_and(|a| a.starts_with("Total ")))
        .map(|r| (r["group"].as_str().unwrap().to_string(), dec(&r["debit"])))
        .collect();
    assert_eq!(subtotals.iter().map(|(g, _)| g.as_str()).collect::<Vec<_>>(), ["Ops", "Sales", "No department"], "{subtotals:?}");
    let cf = t.report("cash_flow", &[("from_date", &day), ("to_date", &day), ("group_by", "department")]).await;
    assert_eq!(cf["meta"]["groups"].as_array().unwrap().len(), 3);
    let profit = cf["rows"].as_array().unwrap().iter().find(|r| r["section"] == json!("profit")).unwrap().clone();
    assert_eq!((dec(&profit["amount"]), dec(&profit["amount_1"])), (Decimal::from(-30), Decimal::from(-100)), "{profit}");

    // The entry from before reverses as it was, though the dimension is now required.
    t.k.cancel(&t.ctx, "JournalEntry", uid(&before), None).await.expect("cancel the entry from before");

    // The filter is served by an index of the lines that carry a dimension.
    let index: Vec<(String,)> = t.sql("select indexdef from pg_indexes where indexname = 'journal_line_dims' and $1::uuid is not null", uid(&sales)).await;
    assert!(index[0].0.contains("gin") && index[0].0.contains("jsonb_path_ops"), "{}", index[0].0);
}
