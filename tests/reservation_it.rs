//! What can be promised and stock held for orders, against a real PostgreSQL
//! (`FERRUM_ERP_RESERVE_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_reserve_test`; reset first):
//! a draft line shows what is available and the day it can be promised (now, when a purchase comes
//! in, or after the item's lead time); the company's capability answers the same without saving; under
//! the Hold policy an order holds what is free and back-orders the rest, of two orders racing for the
//! last stock one holds it, no other delivery may take it, its own delivery uses it up, and releasing,
//! cancelling or the time limit gives it back. Count Only keeps today's behaviour.

mod common;
use common::*;
use ferrum_kernel::capability::Invocation;
use rust_decimal::Decimal;
use serde_json::json;

#[test]
fn reservations() {
    run(scenario());
}

fn n(s: &str) -> Decimal {
    s.parse().unwrap()
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_RESERVE_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_reserve_test".into());
    let t = T::boot(url, |_, _| {}).await;
    let c = t.company.clone();
    let cid: uuid::Uuid = c.parse().unwrap();
    let wh = t.find_one("Warehouse", json!({"warehouse_name": "Stores", "company": c})).await["id"].as_str().unwrap().to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let widget = uid(&t
        .insert("Item", json!({"item_code": "Widget", "item_name": "Widget", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "100", "lead_time_days": 14}))
        .await)
    .to_string();
    t.post(
        "StockEntry",
        json!({"company": c, "purpose": "Material Receipt", "items": [{"item": widget, "qty": "10", "basic_rate": "40", "target_warehouse": wh}]}),
    )
    .await;
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await).to_string();
    let today = t.ctx.today();
    let order = |qty: &str| json!({"company": c, "customer": globex, "set_warehouse": wh, "items": [{"item": widget, "qty": qty}]});

    // Promised at entry: 6 of 10 now; 14 when 5 more come on a purchase in 7 days; 30 after the lead time.
    let po_day = (today + chrono::Duration::days(7)).to_string();
    t.post(
        "PurchaseOrder",
        json!({"company": c, "supplier": acme, "set_warehouse": wh, "schedule_date": po_day, "items": [{"item": widget, "qty": "5", "price_list_rate": "40"}]}),
    )
    .await;
    let line = |doc: &ferrum_kernel::Doc| doc["items"][0].clone();
    let six = t.insert("SalesOrder", order("6")).await;
    assert_eq!((dec(&line(&six)["available_qty"]), line(&six)["promised_date"].clone()), (n("10"), json!(today.to_string())));
    let more = t.insert("SalesOrder", order("14")).await;
    assert_eq!(line(&more)["promised_date"], json!(po_day));
    let lots = t.insert("SalesOrder", order("30")).await;
    assert_eq!(line(&lots)["promised_date"], json!((today + chrono::Duration::days(14)).to_string()));
    let asked = json!({"id": c, "params": {"item": widget, "qty": "12", "warehouse": wh}});
    let out = t.k.invoke(&t.ctx, Invocation::new("core.company.availability", asked)).await.expect("availability").result;
    assert_eq!((dec(&out["warehouses"][0]["available_now"]), out["promise_date"].clone()), (n("10"), json!(po_day)), "{out}");
    for d in [&six, &more, &lots] {
        t.k.delete(&t.ctx, "SalesOrder", uid(d), None).await.expect("drop the draft");
    }

    // Count Only (the default): an order counts but holds nothing.
    let counted = t.post("SalesOrder", order("4")).await;
    assert_eq!(dec(&line(&counted)["stock_reserved_qty"]), n("0"));
    t.k.cancel(&t.ctx, "SalesOrder", uid(&counted), None).await.unwrap();

    // Hold: 6 held, then two orders race for the 4 left: one holds 4, the other back-orders it all.
    t.k.update(&t.ctx, "Company", cid, obj(json!({"reservation_policy": "Hold", "reservation_days": 3})), None).await.expect("hold stock");
    let first = t.post("SalesOrder", order("6")).await;
    assert_eq!((dec(&line(&first)["stock_reserved_qty"]), dec(&line(&first)["backorder_qty"])), (n("6"), n("0")));
    assert_eq!(line(&first)["reserved_until"], json!((today + chrono::Duration::days(3)).to_string()));
    let a = uid(&t.insert("SalesOrder", order("4")).await);
    let b = uid(&t.insert("SalesOrder", order("4")).await);
    let (ra, rb) = tokio::join!(t.k.submit(&t.ctx, "SalesOrder", a, None), t.k.submit(&t.ctx, "SalesOrder", b, None));
    ra.unwrap();
    rb.unwrap();
    let (la, lb) = (line(&t.get("SalesOrder", a).await), line(&t.get("SalesOrder", b).await));
    let mut held = [dec(&la["stock_reserved_qty"]), dec(&lb["stock_reserved_qty"])];
    held.sort();
    assert_eq!(held, [n("0"), n("4")], "one holds the last four: {la} {lb}");
    let (winner, loser) = if dec(&la["stock_reserved_qty"]) == n("4") { (a, b) } else { (b, a) };
    assert_eq!(dec(&line(&t.get("SalesOrder", loser).await)["backorder_qty"]), n("4"));

    // No delivery outside these orders may take what they hold; the loser's own delivery may not either.
    let walk_in =
        t.try_post("DeliveryNote", json!({"company": c, "customer": globex, "items": [{"item": widget, "qty": "1", "warehouse": wh}]})).await.unwrap_err();
    assert!(walk_in.contains("held for other orders"), "{walk_in}");
    let dn: uuid::Uuid = t.action("SalesOrder", loser, "make_delivery_note", json!({})).await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    let e = t.k.submit(&t.ctx, "DeliveryNote", dn, None).await.unwrap_err();
    assert!(format!("{e:?}").contains("held for other orders"), "{e:?}");
    t.k.delete(&t.ctx, "DeliveryNote", dn, None).await.unwrap();
    // The winner's delivery uses its own reservation.
    let dn: uuid::Uuid = t.action("SalesOrder", winner, "make_delivery_note", json!({})).await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    t.submit("DeliveryNote", dn).await;

    // Released by hand, the stock is the loser's when it asks again; cancelling frees it too.
    t.action("SalesOrder", uid(&first), "release_stock", json!({})).await.expect("release");
    let again = t.action("SalesOrder", loser, "reserve_stock", json!({})).await.expect("reserve again");
    assert_eq!(dec(&again["lines"][0]["reserved"]), n("4"), "{again}");
    t.k.cancel(&t.ctx, "SalesOrder", loser, None).await.unwrap();
    t.post("DeliveryNote", json!({"company": c, "customer": globex, "items": [{"item": widget, "qty": "6", "warehouse": wh}]})).await;

    // A reservation past its time limit holds nothing.
    t.post(
        "StockEntry",
        json!({"company": c, "purpose": "Material Receipt", "items": [{"item": widget, "qty": "2", "basic_rate": "40", "target_warehouse": wh}]}),
    )
    .await;
    let late = t.post("SalesOrder", order("2")).await;
    assert_eq!(dec(&line(&late)["stock_reserved_qty"]), n("2"));
    let lid: uuid::Uuid = line(&late)["id"].as_str().unwrap().parse().unwrap();
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    sqlx::query("update e_sales_order__items set reserved_until = current_date - 1 where id = $1").bind(lid).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    t.post("DeliveryNote", json!({"company": c, "customer": globex, "items": [{"item": widget, "qty": "2", "warehouse": wh}]})).await;
}
