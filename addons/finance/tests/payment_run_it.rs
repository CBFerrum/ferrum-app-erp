//! Payment runs (FN-26) and early-payment discounts (FN-27) against a real PostgreSQL
//! (`FERRUM_FINANCE_RUN_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_finance_run_test`;
//! reset first): a run proposes what is due and what has a discount open, leaving out a bill on hold,
//! a bill not due and a supplier under the least; it waits for the approval the workspace set on
//! submitting it; paying books the discount with its tax taken back, sends remittance advice, marks
//! the supplier whose payment is refused as failed, and pays nothing twice when pressed again. A
//! payment made by hand in time takes the discount by itself.

#[path = "../../../tests/common/mod.rs"]
mod common;
use common::*;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

#[test]
fn payment_runs() {
    run(scenario());
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_FINANCE_RUN_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_finance_run_test".into());
    let t = T::boot(url, |packages, hooks| {
        packages.extend(ferrum_finance::packages().expect("finance"));
        ferrum_finance::register(hooks);
    })
    .await;
    let c = t.company.clone();
    let today = t.ctx.today();
    let bank = t.account("Primary Bank").await;
    let input = t.account("Input Tax").await;
    let other_income = t.account("Other Income").await;
    let creditors = t.account("Creditors").await;
    let terms = uid(&t
        .insert(
            "PaymentTermsTemplate",
            json!({"template_name": "2/10 net 30", "discount_reduces_tax": true,
                   "terms": [{"invoice_portion": "100", "due_date_based_on": "Day(s) after invoice date", "credit_days": 30, "discount_percent": "2", "discount_days": 10}],
                   "discount_accounts": [{"company": c, "received_account": other_income}]}),
        )
        .await)
    .to_string();
    let vat = uid(&t.insert("TaxTemplate", json!({"title": "Input VAT", "company": c, "kind": "Purchase", "rows": [{"charge_type": "On Net Total", "account": input, "description": "VAT", "rate": "10"}]})).await).to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let service = uid(&t
        .insert("Item", json!({"item_code": "Service", "item_name": "Service", "item_group": products["id"], "stock_uom": nos["id"], "is_stock_item": false}))
        .await)
    .to_string();
    let supplier = |name: &str| {
        let t = &t;
        let name = name.to_string();
        async move {
            uid(&t.insert("Supplier", json!({"supplier_name": name, "email": format!("{}@suppliers.test", name.to_lowercase()), "fin_iban": "DE89370400440532013000", "fin_bic": "COBADEFFXXX"})).await)
                .to_string()
        }
    };
    let (acme, bolt, cheap) = (supplier("Acme").await, supplier("Bolt").await, supplier("Cheap").await);
    let bill = |sup: &str, no: &str, rate: &str, extra: serde_json::Value| {
        let mut v = json!({"company": c, "supplier": sup, "bill_no": no, "items": [{"item": service, "qty": "1", "price_list_rate": rate}]});
        v.as_object_mut().unwrap().extend(extra.as_object().cloned().unwrap());
        v
    };
    let b1 = t.post("PurchaseInvoice", bill(&acme, "A-1", "1000", json!({"tax_template": vat, "fin_payment_terms_template": terms}))).await;
    let b2 = t.post("PurchaseInvoice", bill(&bolt, "B-1", "500", json!({}))).await;
    let (n1, n2) = (b1["number"].as_str().unwrap().to_string(), b2["number"].as_str().unwrap().to_string());
    let (b1, b2) = (uid(&b1), uid(&b2));
    let b3 = uid(&t.post("PurchaseInvoice", bill(&bolt, "B-2", "300", json!({}))).await);
    t.post("PurchaseInvoice", bill(&cheap, "C-1", "50", json!({}))).await;
    t.post("PurchaseInvoice", bill(&acme, "A-2", "200", json!({"due_date": (today + chrono::Duration::days(40)).to_string()}))).await;
    t.action("PurchaseInvoice", b3, "fin_hold_payment", json!({"reason": "Disputed quantity"})).await.expect("hold");

    // ───── proposed, adjusted, approved ─────
    let house = uid(&t
        .insert("BankAccount", json!({"account_name": "House Bank", "company": c, "account": bank, "iban": "GB82WEST12345698765432", "swift_code": "WESTGB2L"}))
        .await)
    .to_string();
    let run =
        uid(&t.insert("PaymentRun", json!({"company": c, "bank_account": bank, "bank": house, "due_by": today.to_string(), "minimum_amount": "100"})).await);
    let proposed = t.action("PaymentRun", run, "propose", json!({})).await.expect("propose");
    let lines: Vec<(String, Decimal, Decimal)> = proposed["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| (l["invoice_number"].as_str().unwrap().to_string(), dec(&l["discount"]), dec(&l["amount"])))
        .collect();
    assert_eq!(
        lines,
        vec![(n1.clone(), Decimal::from(22), Decimal::from(1078)), (n2.clone(), Decimal::ZERO, Decimal::from(500))],
        "A-1 for its discount though not due, B-1 due; B-2 held, A-2 not due, Cheap under the least"
    );
    assert_eq!(dec(&proposed["total_amount"]), Decimal::from(1578));
    let admin_id = t.ctx.user;
    let approver = t.k.create_user(&t.ctx, "approver@fin.test", "Approver", Some("Correct-Horse-9"), &["Accounts Manager".into()], false).await.expect("user");
    let policy = json!({"steps": [{"label": "Finance", "approvers": {"role": "Accounts Manager"}}]});
    let op: ferrum_kernel::customize::NewCustomization =
        serde_json::from_value(json!({"entity": "PaymentRun", "op": {"op": "set_approval", "on": "submit", "approval": policy}})).unwrap();
    t.k.batch_customizations(&t.ctx, vec![op], false).await.map_err(|(_, e)| e).expect("approval policy");
    // Read again: the policy is part of the schema now.
    let admin = t.k.load_ctx(t.ctx.tenant, admin_id, None, "it".into()).await.unwrap();
    let approver = t.k.load_ctx(t.ctx.tenant, approver, None, "it".into()).await.unwrap();
    let asked = t.k.submit(&admin, "PaymentRun", run, None).await.expect("asks");
    assert_eq!(asked["docstatus"], json!(0), "waits for the approval");
    t.k.decide_approval(&approver, "PaymentRun", run, true, Some("Fine")).await.expect("approve");
    let approved = t.get("PaymentRun", run).await;
    assert_eq!((approved["docstatus"].clone(), approved["run_status"].clone()), (json!(1), json!("Approved")));
    assert_ne!(approver.user, admin_id);

    // Bolt is paid by hand meanwhile: the run's payment to Bolt is refused, Acme's stands.
    t.post("PaymentEntry", json!({"company": c, "payment_type": "Pay", "supplier": bolt, "bank_account": bank, "paid_amount": "500", "references": [{"invoice": b2.to_string(), "allocated_amount": "500"}]}))
        .await;
    let paid = t.action("PaymentRun", run, "make_payments", json!({})).await.expect("make payments");
    let states: Vec<(String, String)> = paid["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| (l["invoice_number"].as_str().unwrap().to_string(), l["state"].as_str().unwrap().to_string()))
        .collect();
    assert_eq!(states, vec![(n1, "Paid".into()), (n2, "Failed".into())]);
    assert!(paid["lines"][1]["error"].as_str().unwrap().contains("outstanding"), "{}", paid["lines"][1]["error"]);
    assert_eq!((paid["run_status"].clone(), paid["failed_count"].clone(), dec(&paid["paid_amount"])), (json!("Partly Paid"), json!(1), Decimal::from(1078)));
    let acme_pay: Uuid = paid["lines"][0]["payment_entry"].as_str().unwrap().parse().unwrap();
    let posted: Vec<(Uuid, Decimal)> = t
        .sql(
            "select l.account_id, sum(l.debit - l.credit) from journal_line l join journal_entry e on e.id = l.entry_id where e.source_id = $1 group by 1",
            acme_pay,
        )
        .await;
    let on = |a: &str| posted.iter().find(|(x, _)| x.to_string() == a).map(|p| p.1).unwrap_or_default();
    assert_eq!(
        (on(&creditors), on(&bank), on(&input), on(&other_income)),
        (Decimal::from(1100), Decimal::from(-1078), Decimal::from(-2), Decimal::from(-20)),
        "1,100 settled by 1,078 paid and 22 of discount, 2 of it the tax taken back"
    );
    assert_eq!(t.get("PurchaseInvoice", b1).await["status"], json!("Paid"));
    // The tax the discount took back reaches the tax summary, as less input tax on the payment's day.
    let day = today.to_string();
    let summary = t.report("tax_summary", &[("from_date", &day), ("to_date", &day)]).await;
    let back = summary["rows"].as_array().unwrap().iter().find(|r| r["voucher_id"] == json!(acme_pay.to_string())).cloned().expect("the payment's row");
    assert_eq!((back["direction"].clone(), dec(&back["tax"]), dec(&back["taxable"])), (json!("Input"), Decimal::from(-2), Decimal::from(-20)), "{back}");
    assert_eq!(dec(&summary["totals"]["input_tax"]), Decimal::from(98), "100 on A-1 less 2 taken back");
    let advice: Vec<(i64,)> = t.sql("select count(*) from message_send where template = 'finance.remittance_advice' and record_id = $1", acme_pay).await;
    assert_eq!(advice[0].0, 1, "Acme is told what was paid");
    // The bank's file: what was paid, kept on the run; the CSV a bank without ISO 20022 takes.
    let xml = String::from_utf8(t.k.render_document(&t.ctx, "finance.bank_transfer_pain001", run, None, false).await.expect("pain.001").bytes).unwrap();
    assert!(xml.contains("<NbOfTxs>1</NbOfTxs>") && xml.contains("<CtrlSum>1078.00</CtrlSum>") && xml.contains("<IBAN>DE89370400440532013000</IBAN>"), "{xml}");
    assert!(xml.contains("<IBAN>GB82WEST12345698765432</IBAN>") && xml.contains("Ccy=\"INR\""), "{xml}");
    let csv = String::from_utf8(t.k.render_document(&t.ctx, "finance.bank_transfer_csv", run, None, false).await.expect("csv").bytes).unwrap();
    assert_eq!(csv.lines().count(), 2, "{csv}");
    let kept: Vec<(i64,)> = t.sql("select count(*) from file_object where record_id = $1 and document is not null", run).await;
    assert_eq!(kept[0].0, 1, "the file is kept on the run");
    // Pressed again: nothing is paid twice.
    let again = t.action("PaymentRun", run, "make_payments", json!({})).await.expect("again");
    assert_eq!(again["lines"][0]["payment_entry"], paid["lines"][0]["payment_entry"]);
    let acme_payments = t.find("PaymentEntry", json!({"supplier": acme, "docstatus": 1})).await;
    assert_eq!(acme_payments.len(), 1);

    // ───── by hand, in time: the discount is taken by itself ─────
    let b6 = uid(&t.post("PurchaseInvoice", bill(&acme, "A-3", "1000", json!({"fin_payment_terms_template": terms}))).await);
    let pe = t
        .post("PaymentEntry", json!({"company": c, "payment_type": "Pay", "supplier": acme, "bank_account": bank, "paid_amount": "980", "references": [{"invoice": b6.to_string(), "allocated_amount": "980"}]}))
        .await;
    assert_eq!(dec(&pe["references"][0]["discount_amount"]), Decimal::from(20));
    assert_eq!(t.get("PurchaseInvoice", b6).await["status"], json!("Paid"));

    // ───── by cheque: a number given on a supplier's line, the next from the mode's book otherwise ─────
    let liabilities = t.find_one("Account", json!({"account_name": "Current Liabilities", "company": c})).await["id"].clone();
    let issued =
        uid(&t.insert("Account", json!({"account_name": "Cheques Issued", "company": c, "root_type": "Liability", "parent_account": liabilities})).await);
    let cheque = uid(&t
        .insert(
            "ModeOfPayment",
            json!({"mode_name": "Cheque", "uses_instruments": true,
            "accounts": [{"company": c, "issued_account": issued.to_string(), "next_instrument_no": 501, "last_instrument_no": 502, "instrument_digits": 6}]}),
        )
        .await)
    .to_string();
    let later = (today + chrono::Duration::days(60)).to_string();
    let run = uid(&t.insert("PaymentRun", json!({"company": c, "bank_account": bank, "bank": house, "mode_of_payment": cheque, "due_by": later})).await);
    let proposed = t.action("PaymentRun", run, "propose", json!({})).await.expect("propose");
    let mut lines = proposed["lines"].as_array().unwrap().clone();
    assert_eq!(lines.len(), 2, "A-2 and Cheap's C-1: {lines:?}");
    let cheap_line = lines.iter().position(|l| l["supplier"] == json!(cheap)).unwrap();
    lines[cheap_line]["instrument_no"] = json!("777001");
    t.k.update(&t.ctx, "PaymentRun", run, obj(json!({"lines": lines})), None).await.expect("a cheque number by hand");
    t.k.submit(&admin, "PaymentRun", run, None).await.expect("asks");
    t.k.decide_approval(&approver, "PaymentRun", run, true, None).await.expect("approve");
    let paid = t.action("PaymentRun", run, "make_payments", json!({})).await.expect("make payments");
    assert_eq!(paid["run_status"], json!("Paid"), "{paid}");
    let number = |l: &Value| {
        let t = &t;
        let pe: Uuid = l["payment_entry"].as_str().unwrap().parse().unwrap();
        async move { t.get("PaymentEntry", pe).await["reference_no"].as_str().unwrap().to_string() }
    };
    let ls = paid["lines"].as_array().unwrap();
    let (by_hand, from_book) = if ls[0]["supplier"] == json!(cheap) { (&ls[0], &ls[1]) } else { (&ls[1], &ls[0]) };
    assert_eq!((number(by_hand).await, number(from_book).await), ("777001".to_string(), "000501".to_string()));
    let modes = t.get("ModeOfPayment", cheque.parse().unwrap()).await;
    assert_eq!(modes["accounts"][0]["next_instrument_no"], json!(502), "one number taken from the book");
}
