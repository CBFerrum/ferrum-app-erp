//! Chart of accounts templates and accounts found by the role they play, against a real PostgreSQL
//! (`FERRUM_ERP_CHART_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_chart_test`; reset
//! first). A company without a template gets the standard chart as before; one made from a template gets the
//! template's accounts, its default links and postings on the accounts standing for the roles, never on an account
//! that only shares a standard number; its chart cannot be changed afterwards; the set-up action adds what it lacks,
//! once; and only an Accounts Manager writes templates.

mod common;

use common::{T, dec, run, uid};
use ferrum_kernel::Error;
use serde_json::{Value, json};
use uuid::Uuid;

#[test]
fn charts_from_templates() {
    run(scenario());
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_CHART_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_chart_test".into());
    let t = T::boot(url, |_, _| {}).await;
    let standard = t.company.clone();

    // a_company_without_a_template_is_as_before
    let accounts = t.find("Account", json!({"company": standard})).await;
    assert!(accounts.len() > 50);
    assert!(accounts.iter().all(|a| a["standard_number"] == a["account_number"]), "each account stands for itself");
    let co = t.get("Company", standard.parse().unwrap()).await;
    assert_eq!(co["default_receivable_account"], json!(t.account("Debtors").await));
    assert_eq!(t.find("Warehouse", json!({"company": standard, "is_group": false})).await.len(), 3);

    // The standard chart turned into a national one: other numbers (7 before each) and names, each standing for the
    // standard account it was; and an expense account that a national chart numbers 2131.
    let by_id: std::collections::HashMap<String, Value> = accounts.iter().map(|a| (a["id"].as_str().unwrap().to_string(), Value::Object(a.clone()))).collect();
    let mut rows: Vec<Value> = accounts
        .iter()
        .map(|a| {
            let number = a["account_number"].as_str().unwrap();
            let parent = a["parent_account"].as_str().and_then(|p| by_id.get(p)).map(|p| format!("7{}", p["account_number"].as_str().unwrap()));
            json!({"account_number": format!("7{number}"), "account_name": format!("National {}", a["account_name"].as_str().unwrap()),
                   "parent_number": parent, "is_group": a["is_group"], "root_type": a["root_type"], "account_type": a["account_type"], "standard_number": number})
        })
        .collect();
    rows.push(json!({"account_number": "2131", "account_name": "Advertising", "parent_number": "75200", "is_group": false, "root_type": "Expense"}));
    // an_accounts_user_cannot_write_templates, and a template that leaves a role out is refused.
    let user = t.k.create_user(&t.ctx, "clerk@fin.test", "Clerk", Some("Correct-Horse-9"), &["Accounts User".into()], false).await.expect("user");
    let clerk = t.k.load_ctx(t.ctx.tenant, user, None, "it".into()).await.unwrap();
    let template = |name: &str, rows: &Vec<Value>| common::obj(json!({"template_name": name, "accounts": rows}));
    let e = t.k.insert(&clerk, "ChartTemplate", template("National", &rows)).await.unwrap_err();
    assert!(matches!(e, Error::Forbidden(_)), "{e:?}");
    let mut short = rows.clone();
    short.retain(|r| r["standard_number"] != json!("2131"));
    let e = t.k.insert(&t.ctx, "ChartTemplate", template("Short", &short)).await.unwrap_err();
    assert!(format!("{e:?}").contains("no account stands for 2131 (Output Tax)"), "{e:?}");
    let national = uid(&t.insert("ChartTemplate", json!({"template_name": "National", "country": "IN", "accounts": rows})).await);

    // a_company_made_from_a_template_gets_its_chart
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let made =
        t.insert("Company", json!({"company_name": "National Co", "abbr": "NC", "default_currency": inr["id"], "chart_template": national.to_string()})).await;
    let nc = uid(&made);
    let ncs = nc.to_string();
    let account_of = |number: &str| {
        let number = number.to_string();
        let ncs = ncs.clone();
        let t = &t;
        async move { t.find_one("Account", json!({"company": ncs, "account_number": number})).await["id"].as_str().unwrap().to_string() }
    };
    let co = t.get("Company", nc).await;
    let receivable = account_of("71111").await;
    assert_eq!(co["default_receivable_account"], json!(receivable));
    assert_eq!(co["default_income_account"], json!(account_of("74110").await));
    assert_eq!(t.find("Account", json!({"company": ncs, "account_name": "Debtors"})).await.len(), 0, "the template's names, not the standard ones");
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let advice = uid(&t
        .insert("Item", json!({"item_code": "Advice", "item_name": "Advice", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "1000", "is_stock_item": false}))
        .await);
    let customer = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await);
    let si = uid(&t.post("SalesInvoice", json!({"company": ncs, "customer": customer.to_string(), "items": [{"item": advice.to_string(), "qty": "1"}]})).await);
    let lines: Vec<(Uuid, rust_decimal::Decimal)> = t
        .sql(
            "select l.account_id, l.debit - l.credit from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id where e.source_id = $1",
            si,
        )
        .await;
    assert!(lines.iter().any(|(a, amt)| a.to_string() == receivable && *amt == dec(&json!("1000"))), "{lines:?}");
    let widget = uid(&t
        .insert("Item", json!({"item_code": "Widget", "item_name": "Widget", "item_group": products["id"], "stock_uom": nos["id"], "is_stock_item": true}))
        .await);
    let supplier = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await);
    let stores = t.find_one("Warehouse", json!({"company": ncs, "warehouse_name": "Stores"})).await;
    assert_eq!(stores["account"], json!(account_of("71141").await), "the warehouse books to the account standing for Stock In Hand");
    let pr = uid(&t
        .post("PurchaseReceipt", json!({"company": ncs, "supplier": supplier.to_string(), "items": [{"item": widget.to_string(), "qty": "10", "price_list_rate": "5", "warehouse": stores["id"]}]}))
        .await);
    let srbnb = account_of("72121").await;
    let lines: Vec<(Uuid, rust_decimal::Decimal)> = t
        .sql(
            "select l.account_id, l.debit - l.credit from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id where e.source_id = $1",
            pr,
        )
        .await;
    assert!(lines.iter().any(|(a, amt)| a.to_string() == srbnb && *amt == dec(&json!("-50"))), "{lines:?}");

    // a_national_number_that_collides_is_not_taken_for_a_role
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    let output = ferrum_app_erp::setup::standard_account(&t.k, &t.ctx, &mut tx, nc, "2131").await.unwrap();
    assert_eq!(output, Some(account_of("72131").await), "the account standing for output tax, not the one numbered 2131");
    let made_tax = ferrum_app_erp::setup::ensure_account(&t.k, &t.ctx, &mut tx, nc, "2136", "Luxury Tax", "2130", "Liability", "Tax").await.unwrap();
    let again = ferrum_app_erp::setup::ensure_account(&t.k, &t.ctx, &mut tx, nc, "2136", "Luxury Tax", "2130", "Liability", "Tax").await.unwrap();
    assert_eq!(made_tax, again, "made once, found after");
    tx.commit().await.unwrap();
    let lux = t.get("Account", made_tax.parse().unwrap()).await;
    assert_eq!(
        (lux["account_number"].clone(), lux["standard_number"].clone(), lux["parent_account"].clone()),
        (Value::Null, json!("2136"), json!(account_of("72130").await))
    );

    // the_chart_cannot_be_changed_after_creation
    let e = t.k.update(&t.ctx, "Company", nc, common::obj(json!({"chart_template": null})), None).await.unwrap_err();
    assert!(format!("{e:?}").contains("chosen when it is made"), "{e:?}");

    // setup_chart_adds_what_a_template_company_lacks_once
    let computers = t.find_one("Account", json!({"company": ncs, "account_number": "71230"})).await;
    t.k.delete(&t.ctx, "Account", uid(&computers), None).await.expect("delete an unused account");
    let first = t.action("Company", nc, "setup_chart_of_accounts", json!({})).await.unwrap();
    assert_eq!(first["added"], json!(["71230"]), "{first}");
    let second = t.action("Company", nc, "setup_chart_of_accounts", json!({})).await.unwrap();
    assert_eq!((second["added"].clone(), second["defaults_set"].clone()), (json!([]), json!(0)), "{second}");
}
