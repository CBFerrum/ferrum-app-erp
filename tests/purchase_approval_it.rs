//! Purchase order and payment approvals by value band, as the packages' default policies, against a real PostgreSQL
//! (`FERRUM_ERP_PO_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_po_test`; reset first):
//! with Ask for Approval off (the default) a big order and a big payment submit at once, as they always did; on, an
//! order under the first band submits at once, one above it waits for a Purchase Manager, one above the second band for
//! an Accounts Manager after them; a payment out above its band waits for an Accounts Manager who did not make it, and
//! above the second for two; the bands are the settings records', so moving one moves which wait, without touching the
//! policy.

mod common;
use common::*;
use serde_json::json;

#[test]
fn purchase_approvals() {
    run(scenario());
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_PO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_po_test".into());
    let t = T::boot(url, |_, _| {}).await;
    let c = t.company.clone();
    let admin = t.k.load_ctx(t.ctx.tenant, t.ctx.user, None, "it".into()).await.unwrap();
    let person = |email: &str, role: &str| {
        let (t, email, role) = (&t, email.to_string(), role.to_string());
        async move {
            let u = t.k.create_user(&t.ctx, &email, &email, Some("Correct-Horse-9"), &[role], false).await.expect("user");
            t.k.load_ctx(t.ctx.tenant, u, None, "it".into()).await.unwrap()
        }
    };
    let buyer = person("pm@fin.test", "Purchase Manager").await;
    let accounts = person("am@fin.test", "Accounts Manager").await;
    let accounts2 = person("am2@fin.test", "Accounts Manager").await;
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = uid(&t
        .insert("Item", json!({"item_code": "Paper", "item_name": "Paper", "item_group": products["id"], "stock_uom": nos["id"], "is_stock_item": false}))
        .await)
    .to_string();
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await).to_string();
    let order = |rate: &str| json!({"company": c, "supplier": acme, "items": [{"item": item, "qty": "1", "price_list_rate": rate}]});
    let status = |id| {
        let t = &t;
        async move { t.get("PurchaseOrder", id).await["docstatus"].clone() }
    };

    // Off by default: nothing is asked of a big order, as before the package shipped a policy.
    let before = uid(&t.insert("PurchaseOrder", order("90000")).await);
    t.k.submit(&admin, "PurchaseOrder", before, None).await.expect("submit");
    assert_eq!(status(before).await, json!(1));
    let settings = uid(&t.insert("PurchaseApprovalSettings", json!({"enabled": true, "first_above": "1000", "second_above": "5000"})).await);
    // Under the first band: submitted at once.
    let small = uid(&t.insert("PurchaseOrder", order("500")).await);
    t.k.submit(&admin, "PurchaseOrder", small, None).await.expect("submit");
    assert_eq!(status(small).await, json!(1));
    // Above it: waits for a Purchase Manager.
    let mid = uid(&t.insert("PurchaseOrder", order("2000")).await);
    assert_eq!(t.k.submit(&admin, "PurchaseOrder", mid, None).await.expect("asks")["docstatus"], json!(0));
    t.k.decide_approval(&buyer, "PurchaseOrder", mid, true, None).await.expect("approve");
    assert_eq!(status(mid).await, json!(1));
    // Above the second: a Purchase Manager, then an Accounts Manager.
    let big = uid(&t.insert("PurchaseOrder", order("9000")).await);
    t.k.submit(&admin, "PurchaseOrder", big, None).await.expect("asks");
    t.k.decide_approval(&buyer, "PurchaseOrder", big, true, None).await.expect("first step");
    assert_eq!(status(big).await, json!(0), "the second step is still open");
    t.k.decide_approval(&accounts, "PurchaseOrder", big, true, None).await.expect("second step");
    assert_eq!(status(big).await, json!(1));
    // The workspace moves the first band: the same order now goes through at once.
    t.k.update(&t.ctx, "PurchaseApprovalSettings", settings, obj(json!({"first_above": "3000"})), None).await.expect("move the band");
    let now_small = uid(&t.insert("PurchaseOrder", order("2000")).await);
    t.k.submit(&admin, "PurchaseOrder", now_small, None).await.expect("submit");
    assert_eq!(status(now_small).await, json!(1));

    // The person who made an order never approves it: the only Purchase Manager's own order above the band cannot wait
    // for anyone, so it is refused rather than left for them to approve themselves.
    let own = uid(&t.insert("PurchaseOrder", order("4000")).await);
    let refused = t.k.submit(&buyer, "PurchaseOrder", own, None).await;
    assert!(format!("{refused:?}").contains("0 may decide it"), "{refused:?}");
    assert_eq!(status(own).await, json!(0));

    // Payments out: off by default, then by band; the maker never approves, two above the second band.
    let supplier_bank = t.account("Primary Bank").await;
    let inr = t.find_one("Currency", json!({"code": "INR"})).await["id"].clone();
    let payment =
        |amount: &str| json!({"company": c, "payment_type": "Pay", "supplier": acme, "bank_account": supplier_bank, "currency": inr, "paid_amount": amount});
    let pay_status = |id| {
        let t = &t;
        async move { t.get("PaymentEntry", id).await["docstatus"].clone() }
    };
    let early = uid(&t.insert("PaymentEntry", payment("20000")).await);
    t.k.submit(&admin, "PaymentEntry", early, None).await.expect("submit");
    assert_eq!(pay_status(early).await, json!(1), "off by default");
    let pay_settings = uid(&t.insert("PaymentApprovalSettings", json!({"enabled": true, "first_above": "1000", "second_above": "10000"})).await);
    let small = uid(&t.insert("PaymentEntry", payment("500")).await);
    t.k.submit(&admin, "PaymentEntry", small, None).await.expect("submit");
    assert_eq!(pay_status(small).await, json!(1));
    let receipt = uid(&t.insert("PaymentEntry", json!({"company": c, "payment_type": "Receive", "customer": t.insert("Customer", json!({"customer_name": "Globex"})).await["id"], "bank_account": supplier_bank, "currency": inr, "paid_amount": "50000"})).await);
    t.k.submit(&admin, "PaymentEntry", receipt, None).await.expect("a receipt is not asked");
    assert_eq!(pay_status(receipt).await, json!(1));
    let second_accounts = person("am3@fin.test", "Accounts Manager").await;
    let mid = uid(&t.insert("PaymentEntry", payment("4000")).await);
    assert_eq!(t.k.submit(&accounts, "PaymentEntry", mid, None).await.expect("asks")["docstatus"], json!(0));
    assert!(t.k.decide_approval(&accounts, "PaymentEntry", mid, true, None).await.is_err(), "the maker does not approve their own payment");
    t.k.decide_approval(&second_accounts, "PaymentEntry", mid, true, None).await.expect("another Accounts Manager");
    assert_eq!(pay_status(mid).await, json!(1));
    let large = uid(&t.insert("PaymentEntry", payment("30000")).await);
    assert_eq!(t.k.submit(&admin, "PaymentEntry", large, None).await.expect("asks")["docstatus"], json!(0));
    t.k.decide_approval(&accounts, "PaymentEntry", large, true, None).await.expect("first");
    assert_eq!(pay_status(large).await, json!(0), "the second step is still open");
    t.k.decide_approval(&second_accounts, "PaymentEntry", large, true, None).await.expect("second, one of two");
    assert_eq!(pay_status(large).await, json!(0));
    t.k.decide_approval(&accounts2, "PaymentEntry", large, true, None).await.expect("second, two of two");
    assert_eq!(pay_status(large).await, json!(1));
    let _ = pay_settings;
}
