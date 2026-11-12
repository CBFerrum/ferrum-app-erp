//! Planning by period across warehouses, against a real PostgreSQL (`FERRUM_ERP_PLANNING_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_planning_test`; reset first): a requirements run by week over two
//! warehouses moves one's spare stock to the other before buying, tells a purchase order that comes too late to be
//! brought forward and one nobody needs to be cancelled, proposes new orders in the week they are needed rounded to the
//! warehouse's order multiple, and creates the transfer it proposes.

mod common;
use common::*;
use serde_json::json;

#[test]
fn planning_by_period() {
    run(scenario());
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_PLANNING_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_planning_test".into());
    let t = T::boot_manufacturing(url).await;
    let c = t.company.clone();
    let stores = t.find_one("Warehouse", json!({"warehouse_name": "Stores", "company": c})).await["id"].as_str().unwrap().to_string();
    let east = uid(&t.insert("Warehouse", json!({"warehouse_name": "East", "company": c})).await).to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |code: &str, extra: serde_json::Value| {
        let mut v = json!({"item_code": code, "item_name": code, "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "1"});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    };
    let bolt = uid(&t.insert("Item", item("Bolt", json!({}))).await).to_string();
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await).to_string();
    let nut =
        uid(&t.insert("Item", item("Nut", json!({"mfg_planning": [{"warehouse": stores, "order_multiple": "10"}], "suppliers": [{"supplier": acme}]}))).await)
            .to_string();
    t.post(
        "StockEntry",
        json!({"company": c, "purpose": "Material Receipt", "items": [{"item": bolt, "qty": "30", "basic_rate": "1", "target_warehouse": east}]}),
    )
    .await;
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let today = t.ctx.today();
    let day = |n: i64| (today + chrono::Duration::days(n)).to_string();
    for (it, qty, when) in [(&bolt, "25", 1), (&bolt, "10", 20), (&nut, "7", 1)] {
        t.post(
            "SalesOrder",
            json!({"company": c, "customer": globex, "set_warehouse": stores, "delivery_date": day(when), "items": [{"item": it, "qty": qty}]}),
        )
        .await;
    }
    let late = t
        .post("PurchaseOrder", json!({"company": c, "supplier": acme, "set_warehouse": stores, "schedule_date": day(60), "items": [{"item": bolt, "qty": "5", "price_list_rate": "1"}]}))
        .await;
    let idle = t
        .post("PurchaseOrder", json!({"company": c, "supplier": acme, "set_warehouse": east, "schedule_date": day(10), "items": [{"item": nut, "qty": "4", "price_list_rate": "1"}]}))
        .await;

    let run = t
        .insert(
            "MrpRun",
            json!({"company": c, "warehouse": stores, "bucket": "Week", "horizon_days": 90, "warehouses": [{"warehouse": stores}, {"warehouse": east}]}),
        )
        .await;
    t.action("MrpRun", uid(&run), "run", json!({})).await.expect("planned");
    let run = t.get("MrpRun", uid(&run)).await;
    let lines = run["lines"].as_array().unwrap();
    let find = |item: &str, action: &str| lines.iter().find(|l| l["item"] == json!(item) && l["action"] == json!(action)).cloned();
    let moved = find(&bolt, "Transfer").expect("a transfer");
    assert_eq!((moved["source_warehouse"].clone(), moved["warehouse"].clone(), dec(&moved["suggested_qty"])), (json!(east), json!(stores), dec(&json!("30"))));
    assert!(find(&bolt, "Purchase").is_none(), "the late order covers the rest: {lines:?}");
    let bought = find(&nut, "Purchase").expect("a nut order");
    assert_eq!((dec(&bought["suggested_qty"]), bought["warehouse"].clone()), (dec(&json!("10")), json!(stores)), "rounded to the multiple");
    let messages = run["messages"].as_array().unwrap();
    let said = |number: &serde_json::Value| messages.iter().find(|m| m["document"] == *number).map(|m| m["message"].as_str().unwrap().to_string());
    assert_eq!(said(&late["number"]).as_deref(), Some("Bring Forward"), "{messages:?}");
    assert_eq!(said(&idle["number"]).as_deref(), Some("Cancel"), "{messages:?}");
    let moved_to = messages.iter().find(|m| m["document"] == late["number"]).unwrap()["suggested_date"].as_str().unwrap().to_string();
    assert!(moved_to < day(60), "{moved_to}");

    let made = t.action("MrpRun", uid(&run), "create_orders", json!({})).await.expect("orders");
    assert!(made["created"].as_array().unwrap().iter().any(|d| d["entity"] == "StockEntry"), "{made}");
}
