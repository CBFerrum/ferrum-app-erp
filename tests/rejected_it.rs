//! Rejected goods at receipt, against a real PostgreSQL (`FERRUM_ERP_REJECTED_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_rejected_test`; reset first): a receipt's rejected quantity
//! goes into the rejected warehouse (made when the company has none) at no value, out of what can be promised or
//! delivered, the order counting only what was accepted; one action makes the return of it (never more than was
//! rejected, the order untouched) or a debit note against the invoice that billed it; an inspection that accepts a
//! line with part refused moves that part to rejected when the receipt is submitted.

mod common;
use common::*;
use rust_decimal::Decimal;
use serde_json::json;

#[test]
fn rejected_goods() {
    run(scenario());
}

fn n(s: &str) -> Decimal {
    s.parse().unwrap()
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_REJECTED_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_rejected_test".into());
    let t = T::boot(url, |packages, hooks| {
        packages.extend(ferrum_stock_ops::packages().expect("stock operations"));
        ferrum_stock_ops::register(hooks);
    })
    .await;
    let c = t.company.clone();
    let stores = t.find_one("Warehouse", json!({"warehouse_name": "Stores", "company": c})).await["id"].as_str().unwrap().to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |code: &str| json!({"item_code": code, "item_name": code, "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "10"});
    let bolt = uid(&t.insert("Item", item("Bolt")).await).to_string();
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await).to_string();
    let po = t
        .post("PurchaseOrder", json!({"company": c, "supplier": acme, "set_warehouse": stores, "items": [{"item": bolt, "qty": "10", "price_list_rate": "5"}]}))
        .await;
    let made = t.action("PurchaseOrder", uid(&po), "make_purchase_receipt", json!({})).await.expect("a receipt");
    let rid: uuid::Uuid = made["id"].as_str().unwrap().parse().unwrap();
    let mut draft = t.get("PurchaseReceipt", rid).await;
    draft["items"][0]["qty"] = json!("7");
    draft["items"][0]["rejected_qty"] = json!("3");
    t.k.update(&t.ctx, "PurchaseReceipt", rid, draft, None).await.expect("seven accepted, three refused");
    let pr = t.submit("PurchaseReceipt", rid).await;
    let rejected_wh = pr["items"][0]["rejected_warehouse"].as_str().expect("a rejected warehouse").to_string();
    assert_eq!(t.get("Warehouse", rejected_wh.parse().unwrap()).await["warehouse_type"], "Rejected");
    let bal: Vec<(Decimal, Decimal)> = t.sql("select qty, value from stock_balance where warehouse_id = $1", rejected_wh.parse().unwrap()).await;
    assert_eq!(bal, vec![(n("3"), n("0"))], "kept at no value");
    assert_eq!(dec(&t.get("PurchaseOrder", uid(&po)).await["items"][0]["received_qty"]), n("7"));
    let asked = json!({"id": c, "params": {"item": bolt, "qty": "1"}});
    let avail = t.k.invoke(&t.ctx, ferrum_kernel::capability::Invocation::new("core.company.availability", asked)).await.unwrap().result;
    let total: Decimal = avail["warehouses"].as_array().unwrap().iter().map(|w| dec(&w["on_hand"])).sum();
    assert_eq!(total, n("7"), "rejected goods are not available: {avail}");
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let refused = t
        .try_post("DeliveryNote", json!({"company": c, "customer": globex, "items": [{"item": bolt, "qty": "1", "warehouse": rejected_wh}]}))
        .await
        .unwrap_err();
    assert!(refused.contains("cannot be delivered"), "{refused}");

    // One action: the return of what was rejected, from the rejected warehouse; the order is untouched.
    let back = t.action("PurchaseReceipt", rid, "return_rejected", json!({})).await.expect("a return");
    let bid: uuid::Uuid = back["id"].as_str().unwrap().parse().unwrap();
    let mut b = t.get("PurchaseReceipt", bid).await;
    assert_eq!((b["rejected_return"].clone(), dec(&b["items"][0]["qty"]), b["items"][0]["warehouse"].clone()), (json!(true), n("3"), json!(rejected_wh)));
    b["items"][0]["qty"] = json!("4");
    t.k.update(&t.ctx, "PurchaseReceipt", bid, b, None).await.expect("saved");
    let over = t.k.submit(&t.ctx, "PurchaseReceipt", bid, None).await;
    assert!(format!("{over:?}").contains("left to send back"), "{over:?}");
    let mut b = t.get("PurchaseReceipt", bid).await;
    b["items"][0]["qty"] = json!("3");
    t.k.update(&t.ctx, "PurchaseReceipt", bid, b, None).await.expect("saved");
    t.submit("PurchaseReceipt", bid).await;
    let bal: Vec<(Decimal, Decimal)> = t.sql("select qty, value from stock_balance where warehouse_id = $1", rejected_wh.parse().unwrap()).await;
    assert_eq!(bal, vec![(n("0"), n("0"))]);
    let pr = t.get("PurchaseReceipt", rid).await;
    assert_eq!((dec(&pr["items"][0]["rejected_returned_qty"]), dec(&pr["items"][0]["returned_qty"])), (n("3"), n("0")));
    assert_eq!(dec(&t.get("PurchaseOrder", uid(&po)).await["items"][0]["received_qty"]), n("7"), "the order still counts what was accepted");
    assert!(t.action("PurchaseReceipt", rid, "return_rejected", json!({})).await.unwrap_err().contains("nothing rejected"));
    assert!(t.k.cancel(&t.ctx, "PurchaseReceipt", rid, None).await.is_err(), "its rejected goods went back");

    // Billed for everything: a debit note for what was rejected, at the invoice's price.
    let r2 = t
        .post("PurchaseReceipt", json!({"company": c, "supplier": acme, "set_warehouse": stores, "items": [{"item": bolt, "qty": "8", "rejected_qty": "2", "price_list_rate": "5"}]}))
        .await;
    let inv = t.action("PurchaseReceipt", uid(&r2), "make_purchase_invoice", json!({})).await.expect("an invoice");
    let inv: uuid::Uuid = inv["id"].as_str().unwrap().parse().unwrap();
    let mut i = t.get("PurchaseInvoice", inv).await;
    i["bill_no"] = json!("ACME-77");
    t.k.update(&t.ctx, "PurchaseInvoice", inv, i, None).await.expect("numbered");
    t.submit("PurchaseInvoice", inv).await;
    let note = t.action("PurchaseReceipt", uid(&r2), "return_rejected", json!({"make": "Debit Note"})).await.expect("a debit note");
    let note = t.get("PurchaseInvoice", note["id"].as_str().unwrap().parse().unwrap()).await;
    assert_eq!((note["is_return"].clone(), dec(&note["items"][0]["qty"]), dec(&note["items"][0]["rate"])), (json!(true), n("-2"), n("5")));

    // An inspection accepting a line with two refused moves them to rejected when the receipt is submitted.
    let checked = uid(&t.insert("Item", item("Washer")).await);
    t.k.update(&t.ctx, "Item", checked, obj(json!({"stk_inspection_required_before_purchase": true})), None).await.expect("inspected item");
    let r3 = t
        .insert(
            "PurchaseReceipt",
            json!({"company": c, "supplier": acme, "set_warehouse": stores, "items": [{"item": checked.to_string(), "qty": "10", "price_list_rate": "2"}]}),
        )
        .await;
    let line = r3["items"][0]["id"].as_str().unwrap().to_string();
    t.post(
        "QualityInspection",
        json!({"company": c, "inspection_type": "Incoming", "reference_type": "Purchase Receipt", "purchase_receipt": r3["id"], "reference_line": line,
               "item": checked.to_string(), "sample_size": "10", "manual_inspection": true, "status": "Accepted", "rejected_qty": "2"}),
    )
    .await;
    let r3 = t.submit("PurchaseReceipt", uid(&r3)).await;
    assert_eq!((dec(&r3["items"][0]["qty"]), dec(&r3["items"][0]["rejected_qty"]), dec(&r3["grand_total"])), (n("8"), n("2"), n("16")));
    let bal: Vec<(Decimal,)> = t.sql("select qty from stock_balance where item_id = $1 order by qty", checked).await;
    assert_eq!(bal, vec![(n("2"),), (n("8"),)]);
}
