//! Promotions an app already applied (a till's), passed to ERP with the sale it makes an invoice of
//! (`FERRUM_ERP_PROMOGIVEN_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_promogiven_test`; reset first): validated against the promotion
//! records (exists, in force at the sale's moment, not more than the engine gives within one unit of rounding),
//! recorded so the report and the usage limits count them once: a sale's source on one submitted document though two
//! are submitted at once, a use counted by ERP or, when the app says it counted, not at all, and given back on
//! cancelling.

mod common;
use common::*;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

#[test]
fn promotions_an_app_applied() {
    run(scenario());
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_PROMOGIVEN_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_promogiven_test".into());
    let t = T::boot(url, |_, _| {}).await;
    let c = t.company.clone();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let lamp = uid(&t
        .insert("Item", json!({"item_code": "Lamp", "item_name": "Lamp", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "100", "is_stock_item": false}))
        .await)
    .to_string();
    // An app makes the invoice as the system (a till's typed price stays): its rates are its own.
    let sctx = ferrum_kernel::Ctx::system(t.ctx.tenant, t.ctx.schema.clone());
    let try_post = |body: Value| {
        let (k, sctx) = (t.k.clone(), sctx.clone());
        async move {
            let d = k.insert(&sctx, "SalesInvoice", obj(body)).await.map_err(|e| format!("{e:?}"))?;
            k.submit(&sctx, "SalesInvoice", uid(&d), None).await.map_err(|e| format!("{e:?}"))
        }
    };
    let walk_in = uid(&t.insert("Customer", json!({"customer_name": "Walk-in"})).await).to_string();
    let today = t.ctx.today();
    let yesterday = (today - chrono::Duration::days(1)).to_string();
    let promo = |title: &str, extra: Value| {
        let mut p = json!({"title": title, "gives": "Percent Off Qualifying Lines", "percent": "20", "channels": "Point of Sale",
                           "qualifying_items": [{"item": lamp}]});
        p.as_object_mut().unwrap().extend(obj(extra));
        p
    };
    let twenty = uid(&t.insert("Promotion", promo("20% off lamps at the till", json!({"max_uses": 3}))).await).to_string();
    let expired = uid(&t.insert("Promotion", promo("Last week's", json!({"valid_upto": yesterday}))).await).to_string();
    let one_use = uid(&t.insert("Promotion", promo("One only", json!({"max_uses": 1, "priority": 9, "combinable": false}))).await).to_string();
    let post = |body: Value| {
        let f = try_post(body);
        async move { f.await.unwrap_or_else(|e| panic!("post: {e}")) }
    };
    let used = |id: &str| {
        let t = &t;
        let id = id.parse().unwrap();
        async move { t.get("Promotion", id).await["used_count"].as_i64().unwrap_or(0) }
    };

    // Two lamps at the till: 200, 20% off is 40, so the line was sold at 80 each.
    let sale = |source: &str, applied: Value, counted: bool| {
        json!({"company": c, "customer": walk_in, "channel": "Point of Sale", "items": [{"item": lamp, "qty": "2", "price_list_rate": "80"}],
               "given_promotions": {"snapshot": "v1", "source": source, "counted": counted, "applied": applied}})
    };
    let line = |promotion: &str, amount: &str| json!([{"promotion": promotion, "line": 0, "amount": amount}]);

    // Taken as it is: the rate is the till's, the promotion is recorded, ERP counts its use once on submission.
    let first = post(sale("pos:1", line(&twenty, "40"), false)).await;
    assert_eq!((dec(&first["items"][0]["rate"]), dec(&first["items"][0]["amount"])), (n("80"), n("160")));
    let rows = first["applied_promotions"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!((dec(&rows[0]["amount"]), rows[0]["promotion"].as_str(), rows[0]["source"].as_str()), (n("40"), Some(twenty.as_str()), Some("pos:1")));
    assert_eq!(used(&twenty).await, 1);
    let report = t.report("promotions", &[]).await;
    let given = |r: &Value| r["rows"].as_array().unwrap().iter().filter(|x| x["promotion_id"] == json!(twenty)).map(|x| dec(&x["discount"])).sum::<Decimal>();
    assert_eq!((given(&report), report["rows"][0]["channel"].clone()), (n("40"), json!("Point of Sale")));

    // The same sale on another document: refused, though the amounts are good.
    let again = try_post(sale("pos:1", line(&twenty, "40"), false)).await.unwrap_err();
    assert!(again.contains("already recorded"), "{again}");
    assert_eq!(used(&twenty).await, 1);

    // One unit of rounding is allowed, no more; a promotion that gives nothing there, one that is not there, one out
    // of its dates, a line that is not one.
    post(sale("pos:2", line(&twenty, "40.01"), false)).await;
    let over = try_post(sale("pos:3", line(&twenty, "40.02"), false)).await.unwrap_err();
    assert!(over.contains("the most it can there is 40"), "{over}");
    let past = try_post(sale("pos:4", line(&expired, "40"), false)).await.unwrap_err();
    assert!(past.contains("was not in force"), "{past}");
    let none = try_post(sale("pos:5", line(&uuid::Uuid::new_v4().to_string(), "40"), false)).await.unwrap_err();
    assert!(none.contains("does not exist"), "{none}");
    let nowhere = try_post(sale("pos:6", json!([{"promotion": twenty, "line": 4, "amount": "1"}]), false)).await.unwrap_err();
    assert!(nowhere.contains("is not a line"), "{nowhere}");
    // On its channel only: a sale of another channel was not in its hours.
    let mut web = sale("pos:7", line(&twenty, "40"), false);
    web["channel"] = json!("Web Shop");
    assert!(try_post(web).await.unwrap_err().contains("was not in force"));
    assert_eq!(used(&twenty).await, 2);

    // An app that counted the use itself: recorded, the report has it, ERP counts nothing and gives nothing back.
    let counted = post(sale("pos:8", line(&twenty, "40"), true)).await;
    assert_eq!(used(&twenty).await, 2);
    assert_eq!(given(&t.report("promotions", &[]).await), n("120.01"));
    t.k.cancel(&t.ctx, "SalesInvoice", uid(&counted), None).await.unwrap();
    assert_eq!(used(&twenty).await, 2);

    // Its limit: the one use of a promotion goes to one sale; the next is refused, and cancelling frees it.
    let one = post(sale("pos:9", line(&one_use, "40"), false)).await;
    assert!(try_post(sale("pos:10", line(&one_use, "40"), false)).await.unwrap_err().contains("used up"));
    t.k.cancel(&t.ctx, "SalesInvoice", uid(&one), None).await.unwrap();
    assert_eq!(used(&one_use).await, 0);
    post(sale("pos:9", line(&one_use, "40"), false)).await;

    // The same sale made twice at once: one document takes it, the other is refused, and the use counts once.
    let draft = |source: &str| {
        let (k, sctx) = (t.k.clone(), sctx.clone());
        let body = sale(source, line(&twenty, "40"), false);
        async move { uid(&k.insert(&sctx, "SalesInvoice", obj(body)).await.unwrap()) }
    };
    let (x, y) = (draft("pos:race").await, draft("pos:race").await);
    let (rx, ry) = tokio::join!(t.k.submit(&sctx, "SalesInvoice", x, None), t.k.submit(&sctx, "SalesInvoice", y, None));
    assert_eq!(rx.is_ok() as u8 + ry.is_ok() as u8, 1, "{rx:?} {ry:?}");
    let refused = format!("{:?}", if rx.is_err() { rx } else { ry });
    assert!(refused.contains("already recorded"), "{refused}");
    assert_eq!(used(&twenty).await, 3);
    // Cancelled, the sale may be made again.
    let won = if t.get("SalesInvoice", x).await["docstatus"] == json!(1) { x } else { y };
    t.k.cancel(&t.ctx, "SalesInvoice", won, None).await.unwrap();
    assert_eq!(used(&twenty).await, 2);
    post(sale("pos:race", line(&twenty, "40"), false)).await;
    assert_eq!(used(&twenty).await, 3);

    // What an app needs to price the same promotions itself is public: the conversion, the line, the weekday.
    let rec = t.get("Promotion", twenty.parse().unwrap()).await;
    let e = ferrum_app_erp::promotion::to_engine(&rec, Decimal::ONE).expect("a known kind");
    assert!(e.id == twenty && matches!(e.gives, ferrum_erp_promotions::Gives::PercentOffLines { percent } if percent == n("20")));
    let row = obj(json!({"id": "l1", "item": lamp, "qty": "2", "discount_percent": "5"}));
    let l = ferrum_app_erp::promotion::engine_line(&row, None, n("190"));
    assert_eq!((l.key.as_str(), l.qty, l.amount, l.line_discount), ("l1", n("2"), n("190"), true));
    assert_eq!(ferrum_app_erp::promotion::iso_weekday("2026-11-12"), Some(4));
}
