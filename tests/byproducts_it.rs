//! By-products and co-products, against a real PostgreSQL (`FERRUM_ERP_BYPRODUCTS_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_byproducts_test`; reset first): a BOM yielding scrap at a
//! recovery value and a co-product sharing cost by standard value; producing from its work order receives all three,
//! the by-product at its value, the rest by the basis, and the values sum to the cent to what was consumed and
//! absorbed; the work order counts only its own item; a BOM without a main weight under a weighted basis is refused.

mod common;
use common::*;
use rust_decimal::Decimal;
use serde_json::json;

#[test]
fn byproducts_and_coproducts() {
    run(scenario());
}

fn n(s: &str) -> Decimal {
    s.parse().unwrap()
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_BYPRODUCTS_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_byproducts_test".into());
    let t = T::boot_manufacturing(url).await;
    let c = t.company.clone();
    let wh = |name: &str| {
        let t = &t;
        let c = c.clone();
        let name = name.to_string();
        async move { t.find_one("Warehouse", json!({"warehouse_name": name, "company": c})).await["id"].as_str().unwrap().to_string() }
    };
    let (stores, goods) = (wh("Stores").await, wh("Finished Goods").await);
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |code: &str| json!({"item_code": code, "item_name": code, "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "1", "is_manufactured": true});
    let seeds = uid(&t.insert("Item", item("Seeds")).await).to_string();
    let oil = uid(&t.insert("Item", item("Oil")).await).to_string();
    let meal = uid(&t.insert("Item", item("Meal")).await).to_string();
    let husk = uid(&t.insert("Item", item("Husk")).await).to_string();
    t.post(
        "StockEntry",
        json!({"company": c, "purpose": "Material Receipt", "items": [{"item": seeds, "qty": "100", "basic_rate": "0.97", "target_warehouse": stores}]}),
    )
    .await;
    let bom = |main_weight: Option<&str>| {
        json!({"company": c, "item": oil, "quantity": "10", "operating_cost": "3",
               "items": [{"item": seeds, "qty": "100", "source_warehouse": stores}],
               "co_product_basis": "Standard Value", "main_weight": main_weight,
               "outputs": [{"item": meal, "qty": "60", "kind": "Co-product", "rate": "0.5"}, {"item": husk, "qty": "25", "kind": "By-product", "rate": "0.07"}]})
    };
    let refused = t.k.insert(&t.ctx, "Bom", obj(bom(None))).await;
    assert!(format!("{refused:?}").contains("main_weight"), "{refused:?}");
    let b = t.post("Bom", bom(Some("4"))).await;
    let wo = t.post("WorkOrder", json!({"company": c, "item": oil, "bom": b["id"], "qty": "10", "source_warehouse": stores, "target_warehouse": goods})).await;
    let made = t.action("WorkOrder", uid(&wo), "manufacture", json!({})).await.expect("produced");
    let entry: uuid::Uuid = made["id"].as_str().unwrap().parse().unwrap();
    let got: Vec<(uuid::Uuid, Decimal, Decimal)> =
        t.sql("select item_id, qty, value from stock_move where source_id = $1 and qty > 0 order by value desc", entry).await;
    // 97.00 seeds + 3.00 operating = 100.00; husk 25 x 0.07 = 1.75; 98.25 over oil 10 x 4 = 40 and meal 60 x 0.5 = 30.
    let value = |i: &str| got.iter().find(|g| g.0.to_string() == i).map(|g| (g.1, g.2)).unwrap();
    assert_eq!(value(&husk), (n("25"), n("1.75")));
    assert_eq!(value(&oil), (n("10"), n("56.14")));
    assert_eq!(value(&meal), (n("60"), n("42.11")));
    assert_eq!(got.iter().map(|g| g.2).sum::<Decimal>(), n("100.00"), "to the cent");
    let w = t.get("WorkOrder", uid(&wo)).await;
    assert_eq!((dec(&w["produced_qty"]), w["status"].clone()), (n("10"), json!("Completed")));
}
