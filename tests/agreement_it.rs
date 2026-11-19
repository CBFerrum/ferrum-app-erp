//! Purchase and sales agreements (erp-9) against a real PostgreSQL (`FERRUM_ERP_AGREEMENT_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_agreement_test`; reset first): an order line that names an
//! agreement takes its price and draws its quantity down, counted in stock units; never past what was agreed, outside
//! its dates or for another party; cancelling, closing and changing orders give the quantity back; Create Order orders
//! what is left; an agreement with orders cannot be cancelled; and who may close it or order from it.

mod common;

use common::{T, dec, run, uid};
use ferrum_kernel::capability::Invocation;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use uuid::Uuid;

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

#[test]
fn agreements() {
    run(scenario());
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_AGREEMENT_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_agreement_test".into());
    let t = T::boot(url, |_, _| {}).await;
    let c = t.company.clone();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let boxes = uid(&t.insert("Uom", json!({"uom_name": "Box of Ten"})).await).to_string();
    let item = |code: &'static str| {
        let (t, g, u, b) = (&t, products["id"].clone(), nos["id"].clone(), boxes.clone());
        async move {
            uid(&t
                .insert(
                    "Item",
                    json!({"item_code": code, "item_name": code, "item_group": g, "stock_uom": u, "standard_rate": "50", "is_stock_item": false,
                                       "uoms": [{"uom": b, "conversion_factor": "10"}]}),
                )
                .await)
            .to_string()
        }
    };
    let (bolt, nut) = (item("Bolt").await, item("Nut").await);
    let supplier = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await).to_string();
    let other = uid(&t.insert("Supplier", json!({"supplier_name": "Initech"})).await).to_string();
    let customer = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let today = t.ctx.today();
    let (from, to) = ((today - chrono::Days::new(30)).to_string(), (today + chrono::Days::new(30)).to_string());
    let pa = t
        .post(
            "PurchaseAgreement",
            json!({"company": c, "supplier": supplier, "from_date": from, "to_date": to, "items": [{"item": bolt, "qty": "100", "rate": "2"}, {"item": nut, "qty": "50", "rate": "1"}]}),
        )
        .await;
    let pa_id = uid(&pa);
    assert_eq!(pa["status"], json!("Open"));
    let po = |lines: Value| json!({"company": c, "supplier": supplier, "items": lines});
    let left =
        async |id: Uuid| -> Vec<Decimal> { t.get("PurchaseAgreement", id).await["items"].as_array().unwrap().iter().map(|l| dec(&l["ordered_qty"])).collect() };

    // an_order_takes_the_agreed_price_and_draws_the_agreement: three boxes of ten are 30 bolts at 2 each.
    let o1 = t.post("PurchaseOrder", po(json!([{"item": bolt, "qty": "3", "uom": boxes, "agreement": pa_id.to_string()}]))).await;
    assert_eq!((dec(&o1["items"][0]["price_list_rate"]), dec(&o1["net_total"])), (n("20"), n("60")), "the agreed price per stock unit, times ten a box");
    assert_eq!(left(pa_id).await, [n("30"), n("0")]);
    assert_eq!(dec(&t.get("PurchaseAgreement", pa_id).await["per_ordered"]), n("20"), "30 of 150");

    // an_order_past_the_agreed_quantity_is_refused, and of two racing orders only one is submitted.
    let e = t.try_post("PurchaseOrder", po(json!([{"item": bolt, "qty": "71", "agreement": pa_id.to_string()}]))).await.unwrap_err();
    assert!(e.contains("agreed 100; this would bring what is ordered to 101"), "{e}");
    let racing = |qty: &'static str| {
        let (k, ctx, v) = (t.k.clone(), t.ctx.clone(), po(json!([{"item": bolt, "qty": qty, "agreement": pa_id.to_string()}])));
        async move {
            let d = k.insert(&ctx, "PurchaseOrder", common::obj(v)).await.unwrap();
            k.submit(&ctx, "PurchaseOrder", uid(&d), None).await
        }
    };
    let (a, b) = tokio::join!(racing("40"), racing("40"));
    assert_eq!([a.is_ok(), b.is_ok()].iter().filter(|x| **x).count(), 1, "70 left: one of two orders of 40 ({a:?}, {b:?})");
    let raced = a.or(b).unwrap();
    assert_eq!(left(pa_id).await, [n("70"), n("0")]);

    // an_order_outside_the_dates_or_for_another_party_is_refused.
    let late = (today + chrono::Days::new(31)).to_string();
    let e = t
        .try_post(
            "PurchaseOrder",
            json!({"company": c, "supplier": supplier, "posting_date": late, "items": [{"item": nut, "qty": "1", "agreement": pa_id.to_string()}]}),
        )
        .await
        .unwrap_err();
    assert!(e.contains(&format!("not on {late}")), "{e}");
    let e = t
        .try_post("PurchaseOrder", json!({"company": c, "supplier": other, "items": [{"item": nut, "qty": "1", "agreement": pa_id.to_string()}]}))
        .await
        .unwrap_err();
    assert!(e.contains("with another supplier"), "{e}");

    // cancelling_closing_and_changing_an_order_gives_the_quantity_back.
    t.k.cancel(&t.ctx, "PurchaseOrder", uid(&raced), None).await.expect("cancel");
    assert_eq!(left(pa_id).await, [n("30"), n("0")], "the cancelled 40 given back");
    let o2 = t.post("PurchaseOrder", po(json!([{"item": bolt, "qty": "50", "agreement": pa_id.to_string()}]))).await;
    assert_eq!(left(pa_id).await, [n("80"), n("0")]);
    t.action("PurchaseOrder", uid(&o2), "update_items", json!({"line": 1, "qty": "45"})).await.expect("lower the quantity");
    assert_eq!(left(pa_id).await, [n("75"), n("0")], "Update Items gives 5 back");
    t.action("PurchaseOrder", uid(&o2), "close", json!({})).await.expect("close");
    assert_eq!(left(pa_id).await, [n("30"), n("0")], "closed short: nothing of it was received");
    t.post("PurchaseOrder", po(json!([{"item": bolt, "qty": "70", "agreement": pa_id.to_string()}]))).await;
    let e = t.action("PurchaseOrder", uid(&o2), "reopen", json!({})).await.unwrap_err();
    assert!(e.contains("this would bring what is ordered to 145"), "re-opening would go past what was agreed: {e}");

    // make_order_orders_what_is_left, then everything on it is ordered.
    let made = t.action("PurchaseAgreement", pa_id, "make_order", json!({})).await.expect("make order");
    let draft = t.get("PurchaseOrder", made["id"].as_str().unwrap().parse().unwrap()).await;
    let lines: Vec<(String, Decimal, Decimal)> =
        draft["items"].as_array().unwrap().iter().map(|l| (l["item"].as_str().unwrap().to_string(), dec(&l["qty"]), dec(&l["price_list_rate"]))).collect();
    assert_eq!(lines, [(nut.clone(), n("50"), n("1"))], "only the nuts are left, at their agreed price");
    t.submit("PurchaseOrder", uid(&draft)).await;
    let pa_now = t.get("PurchaseAgreement", pa_id).await;
    assert_eq!((pa_now["status"].clone(), dec(&pa_now["per_ordered"])), (json!("Fulfilled"), n("100")));
    let e = t.action("PurchaseAgreement", pa_id, "make_order", json!({})).await.unwrap_err();
    assert!(e.contains("is not available"), "a fulfilled agreement offers no Create Purchase Order: {e}");

    // an_agreement_with_orders_cannot_be_cancelled.
    let e = format!("{:?}", t.k.cancel(&t.ctx, "PurchaseAgreement", pa_id, None).await.unwrap_err());
    assert!(e.contains("cancel or change the orders that draw on it first"), "{e}");

    // close_and_reopen_need_submit, make_order_needs_create_on_the_order: an Accounts Manager reads purchase orders
    // and their agreements, nothing more.
    let user = t.k.create_user(&t.ctx, "books@fin.test", "Books", Some("Correct-Horse-9"), &["Accounts Manager".into()], false).await.expect("user");
    let books = t.k.load_ctx(t.ctx.tenant, user, None, "it".into()).await.unwrap();
    let pa2 = uid(&t
        .post(
            "PurchaseAgreement",
            json!({"company": c, "supplier": supplier, "from_date": from, "to_date": to, "items": [{"item": bolt, "qty": "10", "rate": "3"}]}),
        )
        .await);
    assert!(t.k.run_action(&books, "PurchaseAgreement", pa2, "close", json!({})).await.is_err(), "no submit right");
    assert!(t.k.run_action(&books, "PurchaseAgreement", pa2, "make_order", json!({})).await.is_err(), "no create right on purchase orders");
    assert_eq!(t.action("PurchaseAgreement", pa2, "close", json!({})).await.unwrap()["status"], json!("Closed"));
    let e = t.try_post("PurchaseOrder", po(json!([{"item": bolt, "qty": "1", "agreement": pa2.to_string()}]))).await.unwrap_err();
    assert!(e.contains("is closed"), "{e}");
    assert_eq!(t.action("PurchaseAgreement", pa2, "reopen", json!({})).await.unwrap()["status"], json!("Open"));

    // make_order_through_the_gate_repeats: the same key makes one order.
    let call = || Invocation::new("buying.purchase_agreement.make_order", json!({"id": pa2.to_string(), "params": {}})).key("pa2-order");
    let first = t.k.invoke(&t.ctx, call()).await.expect("through the gate");
    let again = t.k.invoke(&t.ctx, call()).await.expect("repeated");
    assert!(!first.repeated && again.repeated);
    let orders = t.sql::<(Uuid,)>("select distinct parent_id from e_purchase_order__items where agreement = $1", pa2).await;
    assert_eq!(orders.len(), 1, "one order made");

    // Selling: the agreed price, which a pricing rule does not discount (a_pricing_rule_does_not_discount_an_agreed_price).
    t.insert("PricingRule", json!({"title": "Bolts 10% off", "apply_on": "Item", "item": bolt, "discount_percent": "10"})).await;
    let sa = uid(&t
        .post(
            "SalesAgreement",
            json!({"company": c, "customer": customer, "from_date": from, "to_date": to, "items": [{"item": bolt, "qty": "20", "rate": "4"}]}),
        )
        .await);
    let so = t
        .post("SalesOrder", json!({"company": c, "customer": customer, "items": [{"item": bolt, "qty": "1", "uom": boxes, "agreement": sa.to_string()}]}))
        .await;
    assert_eq!((dec(&so["items"][0]["price_list_rate"]), dec(&so["items"][0]["discount_percent"]), dec(&so["net_total"])), (n("40"), n("0"), n("40")));
    assert_eq!(dec(&t.get("SalesAgreement", sa).await["items"][0]["ordered_qty"]), n("10"));
    let plain = t.insert("SalesOrder", json!({"company": c, "customer": customer, "items": [{"item": bolt, "qty": "1"}]})).await;
    assert_eq!(dec(&plain["items"][0]["discount_percent"]), n("10"), "the rule still applies without an agreement");
    let e = t
        .try_post("SalesOrder", json!({"company": c, "customer": customer, "items": [{"item": nut, "qty": "1", "agreement": sa.to_string()}]}))
        .await
        .unwrap_err();
    assert!(e.contains("has no line for"), "{e}");
}
