//! Drop shipment (erp-10) against a real PostgreSQL (`FERRUM_ERP_DROP_SHIP_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_drop_ship_test`; reset first): a sales order line the supplier
//! delivers is ordered from them, received without a stock move, booked to cost of sales and marked delivered; billed
//! at another price; refused on a delivery note; never ordered twice; holding and planning nothing; returned.

mod common;

use common::{T, dec, run, uid};
use ferrum_kernel::capability::Invocation;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::str::FromStr;
use uuid::Uuid;

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

#[test]
fn drop_shipment() {
    run(scenario());
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_DROP_SHIP_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_drop_ship_test".into());
    let t = T::boot_manufacturing(url).await;
    let c = t.company.clone();
    let cid: Uuid = c.parse().unwrap();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let stores = t.find_one("Warehouse", json!({"company": c, "warehouse_name": "Stores"})).await["id"].as_str().unwrap().to_string();
    let item = |code: &'static str| {
        let (t, g, u) = (&t, products["id"].clone(), nos["id"].clone());
        async move {
            uid(&t
                .insert("Item", json!({"item_code": code, "item_name": code, "item_group": g, "stock_uom": u, "standard_rate": "100", "is_stock_item": true}))
                .await)
            .to_string()
        }
    };
    let (sofa, lamp) = (item("Sofa").await, item("Lamp").await);
    t.post(
        "StockEntry",
        json!({"company": c, "purpose": "Material Receipt", "items": [{"item": lamp, "qty": "10", "basic_rate": "20", "target_warehouse": stores}]}),
    )
    .await;
    let maker = uid(&t.insert("Supplier", json!({"supplier_name": "Sofa Maker"})).await).to_string();
    let customer = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let address = uid(&t
        .insert("Address", json!({"address_type": "Shipping", "address_line1": "1 Elm St", "city": "Pune", "country": "India", "customer": customer}))
        .await)
    .to_string();
    let moves = |item: String| {
        let t = &t;
        async move { t.sql::<(Uuid,)>("select id from stock_move where item_id = $1", item.parse().unwrap()).await.len() }
    };
    let balance = |name: &'static str| {
        let t = &t;
        async move {
            let acc: Uuid = t.account(name).await.parse().unwrap();
            t.sql::<(Decimal,)>("select coalesce(sum(debit - credit), 0) from journal_line where account_id = $1", acc).await[0].0
        }
    };
    let stock_before = balance("Stock In Hand").await;

    // A mixed order: the lamp from the warehouse, two sofas from the supplier.
    let so = t
        .post(
            "SalesOrder",
            json!({"company": c, "customer": customer, "ship_to": address, "items": [
                {"item": lamp, "qty": "1", "warehouse": stores},
                {"item": sofa, "qty": "2", "rate": "500", "price_list_rate": "500", "drop_ship": true, "drop_ship_supplier": maker}]}),
        )
        .await;
    let so_id = uid(&so);

    // a_drop_ship_line_reserves_and_plans_nothing.
    let run = t.insert("MrpRun", json!({"company": c, "warehouse": stores})).await;
    t.action("MrpRun", uid(&run), "run", json!({})).await.expect("planned");
    let lines = t.get("MrpRun", uid(&run)).await["lines"].as_array().cloned().unwrap_or_default();
    assert!(!lines.iter().any(|l| l["item"] == json!(sofa)), "no demand for the sofas: {lines:?}");

    // make_drop_ship_order_needs_create_on_purchase_orders.
    let user = t.k.create_user(&t.ctx, "sales@fin.test", "Sales", Some("Correct-Horse-9"), &["Sales User".into()], false).await.expect("user");
    let seller = t.k.load_ctx(t.ctx.tenant, user, None, "it".into()).await.unwrap();
    assert!(t.k.run_action(&seller, "SalesOrder", so_id, "make_drop_ship_order", json!({})).await.is_err(), "no create right on purchase orders");

    // make_drop_ship_order_through_the_gate_repeats: one purchase order for the maker, to the customer's address.
    let call = || Invocation::new("selling.sales_order.make_drop_ship_order", json!({"id": so_id.to_string(), "params": {}})).key("so-drop");
    let made = t.k.invoke(&t.ctx, call()).await.expect("order from the supplier");
    assert!(t.k.invoke(&t.ctx, call()).await.expect("repeated").repeated);
    let orders = made.result["orders"].as_array().unwrap().clone();
    assert_eq!(orders.len(), 1);
    let first: Uuid = orders[0]["id"].as_str().unwrap().parse().unwrap();
    let po = t.get("PurchaseOrder", first).await;
    assert_eq!((po["supplier"].clone(), po["ship_to"].clone(), po["drop_ship_customer"].clone()), (json!(maker), json!(address), json!(customer)));
    assert_eq!((po["items"][0]["item"].clone(), dec(&po["items"][0]["qty"]), po["items"][0]["drop_ship"].clone()), (json!(sofa), n("2"), json!(true)));
    // suppliers_are_not_ordered_more_than_was_sold: nothing is ordered until a purchase order is submitted, so a second
    // draft is made for the same line; the two race to be submitted and only one is.
    let second = t.action("SalesOrder", so_id, "make_drop_ship_order", json!({})).await.expect("a second draft");
    let second: Uuid = second["orders"][0]["id"].as_str().unwrap().parse().unwrap();
    for po_id in [first, second] {
        let po = t.get("PurchaseOrder", po_id).await;
        t.k.update(
            &t.ctx,
            "PurchaseOrder",
            po_id,
            common::obj(json!({"items": [{"id": po["items"][0]["id"], "item": sofa, "qty": "2", "price_list_rate": "300"}]})),
            None,
        )
        .await
        .expect("price");
    }
    let racing = |po_id: Uuid| {
        let (k, ctx) = (t.k.clone(), t.ctx.clone());
        async move { k.submit(&ctx, "PurchaseOrder", po_id, None).await }
    };
    let (a, b) = tokio::join!(racing(first), racing(second));
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|x| **x).count(), 1, "one of two orders for the same two sofas: {a:?} {b:?}");
    let refused = format!("{:?}", a.as_ref().err().or(b.as_ref().err()).unwrap());
    assert!(refused.contains("this would order 4 from suppliers"), "{refused}");
    let po_id = if a.is_ok() { first } else { second };
    let so_now = t.get("SalesOrder", so_id).await;
    assert_eq!(dec(&so_now["items"][1]["drop_ship_ordered_qty"]), n("2"));
    let e = t.action("SalesOrder", so_id, "make_drop_ship_order", json!({})).await.unwrap_err();
    assert!(e.contains("no line the supplier delivers is left to order"), "{e}");
    // Two submits also wait on the order number's counter (numbered on submit), so they cannot show the sales order's
    // row lock. An Update Items that starts while another order's submit is not yet committed can: it must wait for the
    // sales order and then count that order. A second sale of two sofas: one ordered, then one more twice.
    let so2 = uid(&t
        .post(
            "SalesOrder",
            json!({"company": c, "customer": customer, "ship_to": address, "items": [
                {"item": sofa, "qty": "2", "rate": "500", "price_list_rate": "500", "drop_ship": true, "drop_ship_supplier": maker}]}),
        )
        .await);
    let order_one = async || {
        let made = t.action("SalesOrder", so2, "make_drop_ship_order", json!({})).await.expect("order from the supplier");
        let po_id: Uuid = made["orders"][0]["id"].as_str().unwrap().parse().unwrap();
        let po = t.get("PurchaseOrder", po_id).await;
        t.k.update(
            &t.ctx,
            "PurchaseOrder",
            po_id,
            common::obj(json!({"items": [{"id": po["items"][0]["id"], "item": sofa, "qty": "1", "price_list_rate": "300"}]})),
            None,
        )
        .await
        .expect("one sofa");
        po_id
    };
    let p1 = order_one().await;
    t.submit("PurchaseOrder", p1).await;
    let p2 = order_one().await;
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    t.k.submit_tx(&t.ctx, &mut tx, "PurchaseOrder", p2, None).await.expect("submitted, not yet committed");
    let (k, ctx) = (t.k.clone(), t.ctx.clone());
    let change = tokio::spawn(async move { k.run_action(&ctx, "PurchaseOrder", p1, "update_items", json!({"line": 1, "qty": "2"})).await });
    assert!(common::holds_up(&t.k, &mut tx, &change).await, "the change waits for the sale");
    tx.commit().await.unwrap();
    let changed = change.await.unwrap().map_err(|e| format!("{e:?}"));
    assert!(changed.as_ref().is_err_and(|e| e.contains("this would order 3 from suppliers")), "it counts the order it waited for: {changed:?}");
    for o in [p1, p2] {
        t.k.cancel(&t.ctx, "PurchaseOrder", o, None).await.expect("cancel");
    }
    t.k.cancel(&t.ctx, "SalesOrder", so2, None).await.expect("cancel the second sale");

    // a_delivery_note_for_a_drop_ship_line_is_refused; a mixed order delivers its stock line from the warehouse.
    let dn = t.action("SalesOrder", so_id, "make_delivery_note", json!({})).await.expect("delivery note");
    let dn = t.get("DeliveryNote", dn["id"].as_str().unwrap().parse().unwrap()).await;
    assert_eq!(dn["items"].as_array().unwrap().iter().map(|l| l["item"].clone()).collect::<Vec<_>>(), [json!(lamp)], "only the lamp");
    t.submit("DeliveryNote", uid(&dn)).await;
    // A delivery note's link to an order line is read-only, so only code makes one: the hooks refuse a drop-ship line
    // there too (crate::drop_ship::refuse_delivery).
    let e = t.action("SalesOrder", so_id, "make_delivery_note", json!({})).await.unwrap_err();
    assert!(e.contains("order them from the supplier"), "{e}");
    let hand = uid(&t.insert("DeliveryNote", json!({"company": c, "customer": customer, "items": [{"item": sofa, "qty": "1", "warehouse": stores}]})).await);
    let sofa_line = so_now["items"][1]["id"].as_str().unwrap().to_string();
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("update e_delivery_note__items set sales_order = '{so_id}', so_line = '{sofa_line}' where parent_id = '{hand}'")))
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let e = format!("{:?}", t.k.submit(&t.ctx, "DeliveryNote", hand, None).await.unwrap_err());
    assert!(e.contains("the supplier delivers this line"), "a line set to the drop-ship line by code: {e}");

    // the_receipt_marks_the_sale_delivered_and_books_cost_of_sales; cancelling it undoes both.
    let pr = t.action("PurchaseOrder", po_id, "make_purchase_receipt", json!({})).await.expect("receipt");
    let pr_id: Uuid = pr["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(t.get("PurchaseReceipt", pr_id).await["items"][0]["drop_ship"], json!(true), "travels from the order");
    // A warehouse typed on the drop-ship line changes nothing: it moves no stock.
    let mut typed = t.get("PurchaseReceipt", pr_id).await["items"][0].clone();
    typed["warehouse"] = json!(stores);
    t.k.update(&t.ctx, "PurchaseReceipt", pr_id, common::obj(json!({"items": [typed]})), None).await.expect("a warehouse typed");
    t.submit("PurchaseReceipt", pr_id).await;
    assert_eq!(moves(sofa.clone()).await, 0, "a drop-ship receipt moves no stock, warehouse or not");
    let posted = |source: Uuid| {
        let t = &t;
        async move {
            let rows: Vec<(String, Decimal)> = t
                .sql("select a.account_name, sum(l.debit - l.credit) from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
                      join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id where e.source_id = $1 group by 1 having sum(l.debit - l.credit) <> 0", source)
                .await;
            rows.into_iter().collect::<HashMap<_, _>>()
        }
    };
    assert_eq!(posted(pr_id).await, HashMap::from([("Cost of Goods Sold".to_string(), n("600")), ("Stock Received But Not Billed".to_string(), n("-600"))]));
    assert_eq!(dec(&t.get("SalesOrder", so_id).await["items"][1]["delivered_qty"]), n("2"));
    t.k.cancel(&t.ctx, "PurchaseReceipt", pr_id, None).await.expect("cancel the receipt");
    assert!(posted(pr_id).await.is_empty(), "reversed");
    assert_eq!(dec(&t.get("SalesOrder", so_id).await["items"][1]["delivered_qty"]), n("0"));
    let pr = t.action("PurchaseOrder", po_id, "make_purchase_receipt", json!({})).await.expect("receipt again");
    let pr_id: Uuid = pr["id"].as_str().unwrap().parse().unwrap();
    t.submit("PurchaseReceipt", pr_id).await;

    // a_bill_at_another_price_puts_the_difference_in_cost_of_sales, and "received but not billed" ends at zero.
    let bill = t.action("PurchaseReceipt", pr_id, "make_purchase_invoice", json!({})).await.expect("bill");
    let bill_id: Uuid = bill["id"].as_str().unwrap().parse().unwrap();
    let draft = t.get("PurchaseInvoice", bill_id).await;
    t.k.update(&t.ctx, "PurchaseInvoice", bill_id, common::obj(json!({"bill_no": "SM-1", "items": [{"id": draft["items"][0]["id"], "item": sofa, "qty": "2", "price_list_rate": "310", "drop_ship": true,
                "purchase_receipt": pr_id.to_string(), "pr_line": draft["items"][0]["pr_line"], "purchase_order": po_id.to_string(), "po_line": draft["items"][0]["po_line"]}]})), None)
        .await
        .expect("another price");
    t.submit("PurchaseInvoice", bill_id).await;
    let billed = posted(bill_id).await;
    assert_eq!((billed["Stock Received But Not Billed"], billed["Cost of Goods Sold"], billed["Creditors"]), (n("600"), n("20"), n("-620")));

    // The customer's invoice: revenue as always, no stock and no second delivery, even where the item lets twice what
    // was ordered be delivered.
    t.k.update(&t.ctx, "Item", sofa.parse().unwrap(), common::obj(json!({"over_delivery_allowance": "100"})), None).await.expect("an allowance");
    let si = t.action("SalesOrder", so_id, "make_sales_invoice", json!({})).await.expect("invoice");
    let si_id: Uuid = si["id"].as_str().unwrap().parse().unwrap();
    // The sofas alone (the lamp went on the delivery note), with Update Stock.
    let sofa_line: Vec<Value> = t.get("SalesInvoice", si_id).await["items"].as_array().unwrap().iter().filter(|l| l["item"] == json!(sofa)).cloned().collect();
    t.k.update(&t.ctx, "SalesInvoice", si_id, common::obj(json!({"update_stock": true, "items": sofa_line})), None).await.expect("update stock");
    t.submit("SalesInvoice", si_id).await;
    assert_eq!(dec(&t.get("SalesOrder", so_id).await["items"][1]["delivered_qty"]), n("2"), "not counted twice");

    // a_drop_ship_sale_never_touches_stock.
    assert_eq!(moves(sofa.clone()).await, 0, "no stock move for the sofa");
    assert_eq!(balance("Stock In Hand").await, stock_before - n("20"), "only the lamp left the warehouse");
    assert_eq!(balance("Stock Received But Not Billed").await, n("0"));
    let _ = cid;

    // a_return_from_the_customer_to_the_supplier: the receipt's return takes cost of sales back and reopens the line.
    let ret = t.action("PurchaseReceipt", pr_id, "make_purchase_return", json!({})).await.expect("return");
    let ret_id: Uuid = ret["id"].as_str().unwrap().parse().unwrap();
    // A second return drafted before the first is submitted...
    let again = t.action("PurchaseReceipt", pr_id, "make_purchase_return", json!({})).await.expect("a second draft");
    let again: Uuid = again["id"].as_str().unwrap().parse().unwrap();
    t.submit("PurchaseReceipt", ret_id).await;
    assert_eq!(posted(ret_id).await["Cost of Goods Sold"], n("-600"));
    assert_eq!(dec(&t.get("SalesOrder", so_id).await["items"][1]["delivered_qty"]), n("0"));
    assert_eq!(moves(sofa).await, 0);
    // ...comes back no more.
    let e = format!("{:?}", t.k.submit(&t.ctx, "PurchaseReceipt", again, None).await.unwrap_err());
    assert!(e.contains("can still be returned"), "a second return of the same sofas: {e}");
}
