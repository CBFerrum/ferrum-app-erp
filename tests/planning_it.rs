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

    // ───────────── demand forecast (erp-12) ─────────────
    // a_forecast_is_worked_out_from_what_was_delivered: from West, 10, 20 (another 10 cancelled) and 30 (5 returned) in
    // the three months before this one.
    let west = uid(&t.insert("Warehouse", json!({"warehouse_name": "West", "company": c})).await).to_string();
    let gear = uid(&t.insert("Item", item("Gear", json!({"suppliers": [{"supplier": acme}]}))).await).to_string();
    let month = |back: u32, day: u32| {
        use chrono::Datelike;
        (today.with_day(1).unwrap() - chrono::Months::new(back)).with_day(day).unwrap().to_string()
    };
    t.post(
        "StockEntry",
        json!({"company": c, "purpose": "Material Receipt", "posting_date": month(3, 1), "items": [{"item": gear, "qty": "60", "basic_rate": "1", "target_warehouse": west}]}),
    )
    .await;
    let deliver =
        |qty: &str, on: String| json!({"company": c, "customer": globex, "posting_date": on, "items": [{"item": gear, "qty": qty, "warehouse": west}]});
    t.post("DeliveryNote", deliver("10", month(3, 10))).await;
    t.post("DeliveryNote", deliver("20", month(2, 10))).await;
    let cancelled = t.post("DeliveryNote", deliver("10", month(2, 12))).await;
    t.k.cancel(&t.ctx, "DeliveryNote", uid(&cancelled), None).await.expect("cancel");
    let october = t.post("DeliveryNote", deliver("30", month(1, 10))).await;
    let back = t.action("DeliveryNote", uid(&october), "make_sales_return", json!({})).await.expect("return");
    let back = t.get("DeliveryNote", back["id"].as_str().unwrap().parse().unwrap()).await;
    t.k.update(
        &t.ctx,
        "DeliveryNote",
        uid(&back),
        obj(json!({"posting_date": month(1, 20), "items": [{"id": back["items"][0]["id"], "item": gear, "qty": "5", "warehouse": west}]})),
        None,
    )
    .await
    .expect("five back");
    t.submit("DeliveryNote", uid(&back)).await;
    let forecast =
        uid(&t.insert("DemandForecast", json!({"company": c, "warehouse": west, "method": "Moving Average", "history_months": 3, "periods": 2})).await);
    let first = t.action("DemandForecast", forecast, "compute", json!({})).await.expect("compute");
    assert_eq!((first["items"].clone(), first["lines"].clone()), (json!(1), json!(2)), "{first}");
    let lines = t.get("DemandForecast", forecast).await["lines"].as_array().unwrap().clone();
    assert!(
        lines.iter().all(|l| dec(&l["forecast_qty"]) == dec(&json!("18.333")) && dec(&l["history_avg"]) == dec(&json!("18.333"))),
        "(10 + 20 + 25) / 3: {lines:?}"
    );
    // A figure a person changed is kept by a recompute.
    let mut changed = lines.clone();
    changed[0]["planned_qty"] = json!("40");
    t.k.update(&t.ctx, "DemandForecast", forecast, obj(json!({"lines": changed})), None).await.expect("plan for 40");
    t.action("DemandForecast", forecast, "compute", json!({})).await.expect("again");
    let planned: Vec<_> = t.get("DemandForecast", forecast).await["lines"].as_array().unwrap().iter().map(|l| dec(&l["planned_qty"])).collect();
    assert_eq!(planned, [dec(&json!("40")), dec(&json!("18.333"))]);

    // a_run_plans_for_the_forecast_less_open_orders: 58.333 forecast, 5 in stock.
    let gear_buy = |run: &ferrum_kernel::Doc| {
        run["lines"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| l["item"] == json!(gear) && l["action"] == json!("Purchase"))
            .map(|l| dec(&l["suggested_qty"]))
            .sum::<rust_decimal::Decimal>()
    };
    let plan = |extra: serde_json::Value| {
        let t = &t;
        let mut v = json!({"company": c, "warehouse": west, "forecast": forecast.to_string()});
        for (k, x) in extra.as_object().unwrap() {
            v[k] = x.clone();
        }
        async move {
            let r = uid(&t.insert("MrpRun", v).await);
            t.action("MrpRun", r, "run", json!({})).await.map(|_| r)
        }
    };
    let r = plan(json!({})).await.expect("planned with the forecast");
    assert_eq!(gear_buy(&t.get("MrpRun", r).await), dec(&json!("53.333")));
    // By month: the first month's 40 and the second's 18.333.
    let r = plan(json!({"bucket": "Month", "horizon_days": 90, "warehouses": [{"warehouse": west}]})).await.expect("by month");
    assert_eq!(gear_buy(&t.get("MrpRun", r).await), dec(&json!("53.333")));
    // An order larger than the forecast: nothing added.
    t.post(
        "SalesOrder",
        json!({"company": c, "customer": globex, "set_warehouse": west, "delivery_date": today.to_string(), "items": [{"item": gear, "qty": "70"}]}),
    )
    .await;
    let r = plan(json!({})).await.expect("planned");
    assert_eq!(gear_buy(&t.get("MrpRun", r).await), dec(&json!("65")), "70 ordered, 5 in stock");

    // a_forecast_of_another_warehouse_is_refused.
    let e = plan(json!({"warehouse": stores})).await.unwrap_err();
    assert!(e.contains("another warehouse's forecast"), "{e}");

    // compute_needs_write, compute_through_the_gate_repeats.
    let user = t.k.create_user(&t.ctx, "maker@fin.test", "Maker", Some("Correct-Horse-9"), &["Manufacturing User".into()], false).await.expect("user");
    let maker = t.k.load_ctx(t.ctx.tenant, user, None, "it".into()).await.unwrap();
    assert!(t.k.run_action(&maker, "DemandForecast", forecast, "compute", json!({})).await.is_err());
    let call =
        || ferrum_kernel::capability::Invocation::new("manufacturing.demand_forecast.compute", json!({"id": forecast.to_string(), "params": {}})).key("fc-1");
    assert!(!t.k.invoke(&t.ctx, call()).await.expect("through the gate").repeated);
    assert!(t.k.invoke(&t.ctx, call()).await.expect("repeated").repeated);
}
