//! A person's erasure reaches what ERP 0.19 to 0.22 added, against a real PostgreSQL
//! (`FERRUM_FINANCE_ERASE_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_finance_erase_test`; reset first):
//! an individual supplier's bank details and the payment run lines that copied them are blanked; an
//! individual customer's name and contact details go from the customer; payments, the cheques they
//! were made by, withholding certificates and the customer's gift card stay, each named as retained.

#[path = "../../../tests/common/mod.rs"]
mod common;
use common::*;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[test]
fn erasure() {
    run(scenario());
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_FINANCE_ERASE_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_finance_erase_test".into());
    let t = T::boot(url, |packages, hooks| {
        packages.extend(ferrum_finance::packages().expect("finance"));
        ferrum_finance::register(hooks);
    })
    .await;
    let c = t.company.clone();
    let today = t.ctx.today().to_string();
    let bank = t.account("Primary Bank").await;
    let group = |name: &str| {
        let t = &t;
        let name = name.to_string();
        async move { t.find_one("Account", json!({"account_name": name, "company": t.company})).await["id"].clone() }
    };
    let account = |name: &str, root: &str, parent: Value| json!({"account_name": name, "company": c, "root_type": root, "parent_account": parent});
    let in_hand = uid(&t.insert("Account", account("Cheques in Hand", "Asset", group("Current Assets").await)).await).to_string();
    let issued = uid(&t.insert("Account", account("Cheques Issued", "Liability", group("Current Liabilities").await)).await).to_string();
    let cards = uid(&t.insert("Account", account("Gift Cards", "Liability", group("Current Liabilities").await)).await).to_string();
    let cheque = uid(&t
        .insert(
            "ModeOfPayment",
            json!({"mode_name": "Cheque", "uses_instruments": true, "accounts": [{"company": c, "in_hand_account": in_hand, "issued_account": issued}]}),
        )
        .await)
    .to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await["id"].clone();
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await["id"].clone();
    let service = uid(&t
        .insert(
            "Item",
            json!({"item_code": "Work", "item_name": "Work", "item_group": products, "stock_uom": nos, "standard_rate": "100", "is_stock_item": false}),
        )
        .await)
    .to_string();
    let wht = t.account("Output Tax").await;
    let category = uid(&t
        .insert("WithholdingCategory", json!({"category_name": "Fees", "applies": "On Bill", "rates": [{"valid_from": "2000-01-01", "rate": "10"}], "accounts": [{"company": c, "account": wht}]}))
        .await)
    .to_string();

    // An individual supplier: bank details, a run that copied them, a cheque paid to them, a certificate.
    let bob = uid(&t
        .insert("Supplier", json!({"supplier_name": "Bob Smith", "email": "bob@people.test", "fin_iban": "DE89370400440532013000", "fin_bic": "COBADEFFXXX"}))
        .await);
    t.post(
        "PurchaseInvoice",
        json!({"company": c, "supplier": bob.to_string(), "bill_no": "B-1", "items": [{"item": service, "qty": "1", "price_list_rate": "100"}]}),
    )
    .await;
    let run = uid(&t.insert("PaymentRun", json!({"company": c, "bank_account": bank, "due_by": today})).await);
    let proposed = t.action("PaymentRun", run, "propose", json!({})).await.expect("propose");
    assert_eq!(proposed["lines"][0]["supplier_email"], json!("bob@people.test"));
    let paid_bob = t
        .post(
            "PaymentEntry",
            json!({"company": c, "payment_type": "Pay", "supplier": bob.to_string(), "mode_of_payment": cheque, "bank_account": bank,
            "paid_amount": "10", "reference_no": "900001", "reference_date": today}),
        )
        .await;
    let cert = uid(&t
        .insert("WithholdingCertificate", json!({"certificate_no": "LDC-1", "category": category, "party_type": "Supplier", "supplier": bob.to_string(), "valid_from": "2000-01-01", "rate": "2"}))
        .await);

    // An individual customer: a cheque from them, a gift card they hold.
    let ann = uid(&t
        .insert("Customer", json!({"customer_name": "Ann Lee", "customer_type": "Individual", "email": "ann@people.test", "phone": "+1 555 0100"}))
        .await);
    let si = uid(&t.post("SalesInvoice", json!({"company": c, "customer": ann.to_string(), "items": [{"item": service, "qty": "1"}]})).await);
    let paid_ann = t
        .post(
            "PaymentEntry",
            json!({"company": c, "payment_type": "Receive", "customer": ann.to_string(), "mode_of_payment": cheque, "bank_account": bank,
            "paid_amount": "100", "reference_no": "000123", "reference_date": today, "references": [{"invoice": si.to_string(), "allocated_amount": "100"}]}),
        )
        .await;
    let kind = uid(&t
        .insert(
            "StoredValueKind",
            json!({"kind_name": "Gift Card", "company": c, "liability_account": cards, "adjustment_account": t.account("Administrative Expenses").await}),
        )
        .await);
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    ferrum_app_erp::stored_value::issue(&t.k, &t.ctx, &mut tx, kind, "50".parse().unwrap(), "goodwill", Some(ann), None, None, "card-1").await.unwrap();
    tx.commit().await.unwrap();

    // Erased.
    let out = t.k.erase_personal_data(&t.ctx, "bob@people.test", "request 1").await.expect("erase the supplier");
    let retained: BTreeSet<String> = out["retained"].as_array().unwrap().iter().map(|r| r["entity"].as_str().unwrap().to_string()).collect();
    assert_eq!(retained, ["PaymentEntry", "PaymentInstrument", "WithholdingCertificate"].map(String::from).into(), "{out}");
    let sup = t.get("Supplier", bob).await;
    assert!(sup["fin_iban"].is_null() && sup["fin_bic"].is_null() && sup["email"].is_null(), "{sup:?}");
    let line = &t.get("PaymentRun", run).await["lines"][0];
    assert!(["iban", "bic", "supplier_name", "supplier_email"].iter().all(|f| line[*f].is_null()), "{line}");
    assert!(line["invoice"].is_string() && line["amount"].is_string(), "what is paid stays: {line}");
    let instrument: uuid::Uuid = paid_bob["payment_instrument"].as_str().unwrap().parse().unwrap();
    assert_eq!(t.get("PaymentInstrument", instrument).await["instrument_no"], json!("900001"), "kept: the books' cheque");
    assert_eq!(t.get("WithholdingCertificate", cert).await["certificate_no"], json!("LDC-1"), "kept: the evidence for the rate");

    let out = t.k.erase_personal_data(&t.ctx, "ann@people.test", "request 2").await.expect("erase the customer");
    let retained: BTreeSet<String> = out["retained"].as_array().unwrap().iter().map(|r| r["entity"].as_str().unwrap().to_string()).collect();
    assert_eq!(retained, ["PaymentEntry", "PaymentInstrument", "StoredValueAccount"].map(String::from).into(), "{out}");
    let cust = t.get("Customer", ann).await;
    assert!(cust["email"].is_null() && cust["phone"].is_null() && cust["customer_name"] != json!("Ann Lee"), "{cust:?}");
    let instrument: uuid::Uuid = paid_ann["payment_instrument"].as_str().unwrap().parse().unwrap();
    assert_eq!(t.get("PaymentInstrument", instrument).await["party_name"], json!("Ann Lee"), "kept: the name the cheque was drawn in");
}
