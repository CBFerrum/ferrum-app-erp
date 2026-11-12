//! Payment instruments (FN-34) against a real PostgreSQL (`FERRUM_ERP_CHEQUES_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_cheques_test`; reset first): a
//! post-dated cheque waits in the cheques account and cannot clear before its date; a bounce takes the
//! payment back (the invoice is owed again) and charges the customer; a cheque that cleared reaches
//! the bank, cannot be cleared twice even at once, and its payment cannot just be cancelled; an issued
//! cheque is replaced; the upkeep says once that a cheque can be deposited.

mod common;
use common::*;
use rust_decimal::Decimal;
use serde_json::{Value, json};

#[test]
fn instruments() {
    run(scenario());
}

async fn balance(t: &T, account: &str) -> Decimal {
    let id: uuid::Uuid = account.parse().unwrap();
    let rows: Vec<(Decimal,)> = t.sql("select coalesce(sum(debit - credit), 0) from journal_line where account_id = $1", id).await;
    rows[0].0
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_CHEQUES_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_cheques_test".into());
    let t = T::boot(url, |_, _| {}).await;
    let c = t.company.clone();
    let group = |name: &str| {
        let t = &t;
        let name = name.to_string();
        async move { t.find_one("Account", json!({"account_name": name, "company": t.company})).await["id"].clone() }
    };
    let account = |name: &str, root: &str, parent: Value| json!({"account_name": name, "company": c, "root_type": root, "parent_account": parent});
    let in_hand = uid(&t.insert("Account", account("Cheques in Hand", "Asset", group("Current Assets").await)).await).to_string();
    let issued = uid(&t.insert("Account", account("Cheques Issued", "Liability", group("Current Liabilities").await)).await).to_string();
    let fees = uid(&t.insert("Account", account("Bounce Charges", "Income", group("Indirect Income").await)).await).to_string();
    let bank = t.account("Primary Bank").await;
    let mode = uid(&t
        .insert(
            "ModeOfPayment",
            json!({"mode_name": "Cheque", "uses_instruments": true, "bounce_charge": "50",
                   "accounts": [{"company": c, "in_hand_account": in_hand, "issued_account": issued, "bounce_charge_account": fees}]}),
        )
        .await)
    .to_string();

    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let advice = uid(&t
        .insert("Item", json!({"item_code": "Advice", "item_name": "Advice", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "1000", "is_stock_item": false}))
        .await)
    .to_string();
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let invoice = json!({"company": c, "customer": globex, "items": [{"item": advice, "qty": "1"}]});
    let today = t.ctx.today();
    let later = (today + chrono::Duration::days(10)).to_string();
    let receipt = |si: &str, no: &str, dated: &str| {
        json!({"company": c, "payment_type": "Receive", "customer": globex, "mode_of_payment": mode, "bank_account": bank, "paid_amount": "1000",
               "reference_no": no, "reference_date": dated, "instrument_bank": "West Bank", "references": [{"invoice": si, "allocated_amount": "1000"}]})
    };

    // ───── a post-dated cheque, bounced ─────
    let si = uid(&t.post("SalesInvoice", invoice.clone()).await).to_string();
    let mut undated = receipt(&si, "000123", &later);
    undated.as_object_mut().unwrap().remove("reference_date");
    let refused = t.try_post("PaymentEntry", undated).await.unwrap_err();
    assert!(refused.contains("the date written on it"), "{refused}");
    let pe = t.post("PaymentEntry", receipt(&si, "000123", &later)).await;
    let cheque = pe["payment_instrument"].as_str().expect("an instrument").parse().unwrap();
    let inst = t.get("PaymentInstrument", cheque).await;
    assert_eq!((inst["status"].clone(), inst["instrument_no"].clone(), inst["direction"].clone()), (json!("Received"), json!("000123"), json!("Received")));
    assert_eq!((balance(&t, &in_hand).await, balance(&t, &bank).await), (Decimal::from(1000), Decimal::ZERO), "it waits in the cheques account");
    assert_eq!(t.get("SalesInvoice", si.parse().unwrap()).await["status"], json!("Paid"));
    let early = t.action("PaymentInstrument", cheque, "clear", json!({})).await.unwrap_err();
    assert!(early.contains("cannot clear before"), "{early}");
    t.action("PaymentInstrument", cheque, "deposit", json!({})).await.expect("deposit");
    t.action("PaymentInstrument", cheque, "bounce", json!({"reason": "Insufficient funds"})).await.expect("bounce");
    let inst = t.get("PaymentInstrument", cheque).await;
    assert_eq!(inst["status"], json!("Bounced"));
    assert_eq!(t.get("PaymentEntry", uid(&pe)).await["docstatus"], json!(2), "the payment is taken back");
    let si_doc = t.get("SalesInvoice", si.parse().unwrap()).await;
    assert_eq!((si_doc["status"].clone(), dec(&si_doc["outstanding_amount"])), (json!("Unpaid"), Decimal::from(1000)), "owed again");
    assert_eq!(balance(&t, &in_hand).await, Decimal::ZERO);
    let charge = t.get("JournalEntry", inst["charge_entry"].as_str().unwrap().parse().unwrap()).await;
    assert_eq!((dec(&charge["total_debit"]), charge["docstatus"].clone()), (Decimal::from(50), json!(1)), "the mode's charge to the customer");
    assert_eq!(balance(&t, &fees).await, Decimal::from(-50));
    let again = t.action("PaymentInstrument", cheque, "bounce", json!({"reason": "twice"})).await.unwrap_err();
    assert!(again.contains("not available"), "{again}");

    // ───── a cheque dated today: reminded once, cleared once though asked twice at once ─────
    let pe = t.post("PaymentEntry", receipt(&si, "000124", &today.to_string())).await;
    let cheque: uuid::Uuid = pe["payment_instrument"].as_str().unwrap().parse().unwrap();
    assert_eq!(ferrum_app_erp::instruments::upkeep(&t.k, &t.ctx, today).await.unwrap(), 1, "the cheque's day has come");
    assert_eq!(ferrum_app_erp::instruments::upkeep(&t.k, &t.ctx, today).await.unwrap(), 0, "once");
    let (a, b) = tokio::join!(t.action("PaymentInstrument", cheque, "clear", json!({})), t.action("PaymentInstrument", cheque, "clear", json!({})));
    assert!(a.is_ok() != b.is_ok(), "one clears, the other is told: {a:?} {b:?}");
    assert_eq!((balance(&t, &in_hand).await, balance(&t, &bank).await), (Decimal::ZERO, Decimal::from(1000)), "in the bank");
    let cancel = t.k.cancel(&t.ctx, "PaymentEntry", uid(&pe), None).await.unwrap_err();
    assert!(format!("{cancel:?}").contains("record the instrument's bounce"), "{cancel:?}");

    // ───── an issued cheque, replaced ─────
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await).to_string();
    let pay = t
        .post(
            "PaymentEntry",
            json!({"company": c, "payment_type": "Pay", "supplier": acme, "mode_of_payment": mode, "bank_account": bank, "paid_amount": "300",
                   "reference_no": "900001", "reference_date": later}),
        )
        .await;
    assert_eq!(balance(&t, &issued).await, Decimal::from(-300), "owed on the cheque until it clears");
    let old: uuid::Uuid = pay["payment_instrument"].as_str().unwrap().parse().unwrap();
    let replaced =
        t.action("PaymentInstrument", old, "replace", json!({"instrument_no": "900002", "instrument_date": later, "reason": "Torn"})).await.expect("replace");
    assert_eq!(replaced["status"], json!("Replaced"));
    let new: uuid::Uuid = replaced["replaced_by"].as_str().unwrap().parse().unwrap();
    let new_doc = t.get("PaymentInstrument", new).await;
    assert_eq!(
        (new_doc["status"].clone(), new_doc["instrument_no"].clone(), new_doc["replaces"].clone()),
        (json!("Issued"), json!("900002"), json!(old.to_string()))
    );
    assert_eq!(t.get("PaymentEntry", uid(&pay)).await["payment_instrument"], json!(new.to_string()));
    // The register: by state and the date written on them.
    let open = t.find("PaymentInstrument", json!({"status": "Issued"})).await;
    assert_eq!(open.len(), 1);
}
