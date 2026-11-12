//! Purchase order approvals by value band, as configuration, against a real PostgreSQL
//! (`FERRUM_ERP_PO_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_po_test`; reset first):
//! the policy the README gives (docs/approvals/purchase_order.json) applied as a workspace would; an
//! order under the first band submits at once, one above it waits for a Purchase Manager, one above
//! the second band for an Accounts Manager after them; the bands are the settings record's, so moving
//! one moves which orders wait, without touching the policy.

mod common;
use common::*;
use serde_json::{Value, json};

#[test]
fn purchase_approvals() {
    run(scenario());
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_PO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_po_test".into());
    let t = T::boot(url, |_, _| {}).await;
    let c = t.company.clone();
    let policy: Value = serde_json::from_str(include_str!("../docs/approvals/purchase_order.json")).unwrap();
    let op: ferrum_kernel::customize::NewCustomization = serde_json::from_value(policy).unwrap();
    t.k.batch_customizations(&t.ctx, vec![op], false).await.map_err(|(_, e)| e).expect("the shipped policy applies");
    let settings = uid(&t.insert("PurchaseApprovalSettings", json!({"first_above": "1000", "second_above": "5000"})).await);
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
    person("am2@fin.test", "Accounts Manager").await;
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
}
