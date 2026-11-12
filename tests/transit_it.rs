//! Transfers through transit, against a real PostgreSQL (`FERRUM_ERP_TRANSIT_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_transit_test`; reset first): a transfer ships into a transit
//! warehouse (made when the company has none), valued, out of what can be promised, and listed as in transit from and
//! to; a receipt takes part, a second the rest with two short and claimed (written off when no claim is said), each at the value it
//! left at, so the books sum to the cent; two receipts racing for the last goods cannot both take them; cancelling
//! gives back and a shipped transfer with receipts cannot be cancelled.

mod common;
use common::*;
use rust_decimal::Decimal;
use serde_json::json;

#[test]
fn transfers_through_transit() {
    run(scenario());
}

fn n(s: &str) -> Decimal {
    s.parse().unwrap()
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_TRANSIT_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_transit_test".into());
    let t = T::boot(url, |_, _| {}).await;
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
    let widget = uid(&t
        .insert("Item", json!({"item_code": "Widget", "item_name": "Widget", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "10"}))
        .await)
    .to_string();
    t.post(
        "StockEntry",
        json!({"company": c, "purpose": "Material Receipt", "items": [{"item": widget, "qty": "9", "basic_rate": "3.33", "target_warehouse": stores}]}),
    )
    .await;

    let shipped = t
        .post(
            "StockEntry",
            json!({"company": c, "purpose": "Material Transfer", "through_transit": true,
                   "items": [{"item": widget, "qty": "9", "source_warehouse": stores, "target_warehouse": goods}]}),
        )
        .await;
    let sid = uid(&shipped);
    let shipped = t.get("StockEntry", sid).await;
    assert_eq!(shipped["transit_status"], "In Transit");
    let transit = shipped["transit_warehouse"].as_str().unwrap().to_string();
    assert_eq!(t.get("Warehouse", transit.parse().unwrap()).await["warehouse_type"], "Transit");
    let bal: Vec<(Decimal, Decimal)> = t.sql("select qty, value from stock_balance where warehouse_id = $1", transit.parse().unwrap()).await;
    assert_eq!(bal, vec![(n("9"), n("29.97"))], "valued in transit");
    let out = t.report("goods_in_transit", &[]).await;
    assert_eq!(
        (out["rows"][0]["from"].clone(), out["rows"][0]["to"].clone(), dec(&out["rows"][0]["in_transit"])),
        (json!("Stores"), json!("Finished Goods"), n("9"))
    );
    let held = t.report("stock_balance", &[]).await;
    assert!(held["rows"].as_array().unwrap().iter().any(|r| r["holds"] == "Transit" && dec(&r["qty"]) == n("9")), "{held}");
    // Nothing in transit can be promised.
    let asked = json!({"id": c, "params": {"item": widget, "qty": "1"}});
    let avail = t.k.invoke(&t.ctx, ferrum_kernel::capability::Invocation::new("core.company.availability", asked)).await.unwrap().result;
    assert!(avail["warehouses"].as_array().unwrap().iter().all(|w| w["warehouse"] != json!(transit)), "{avail}");

    // A first receipt takes 4.
    let first = t.action("StockEntry", sid, "receive_transfer", json!({})).await.expect("a receipt");
    let fid: uuid::Uuid = first["id"].as_str().unwrap().parse().unwrap();
    let mut draft = t.get("StockEntry", fid).await;
    draft["items"][0]["qty"] = json!("4");
    t.k.update(&t.ctx, "StockEntry", fid, draft, None).await.expect("four arrive");
    t.submit("StockEntry", fid).await;
    assert_eq!(t.get("StockEntry", sid).await["transit_status"], "Partly Received");
    let got: Vec<(Decimal, Decimal)> = t.sql("select qty, value from stock_balance where warehouse_id = $1", goods.parse().unwrap()).await;
    assert_eq!(got, vec![(n("4"), n("13.32"))]);

    // Two receipts of the rest race: one takes it, the other is refused.
    let a = t.action("StockEntry", sid, "receive_transfer", json!({})).await.unwrap()["id"].as_str().unwrap().parse::<uuid::Uuid>().unwrap();
    let b = t.action("StockEntry", sid, "receive_transfer", json!({})).await.unwrap()["id"].as_str().unwrap().parse::<uuid::Uuid>().unwrap();
    let claims = t.account("Accrued Income").await;
    for id in [a, b] {
        let mut d = t.get("StockEntry", id).await;
        d["items"][0]["qty"] = json!("3");
        d["items"][0]["short_qty"] = json!("2");
        d["items"][0]["short_action"] = json!("Claimed");
        d["items"][0]["short_account"] = json!(claims);
        d["items"][0]["short_reason"] = json!("Two crushed in the truck");
        t.k.update(&t.ctx, "StockEntry", id, d, None).await.expect("three arrive, two do not");
    }
    let (ra, rb) = tokio::join!(t.k.submit(&t.ctx, "StockEntry", a, None), t.k.submit(&t.ctx, "StockEntry", b, None));
    assert_eq!(ra.is_ok() as u8 + rb.is_ok() as u8, 1, "{ra:?} {rb:?}");
    let refused = if ra.is_err() { format!("{ra:?}") } else { format!("{rb:?}") };
    assert!(refused.contains("still in transit"), "{refused}");
    let done = if ra.is_ok() { a } else { b };
    assert_eq!(t.get("StockEntry", sid).await["transit_status"], "Received");
    let got: Vec<(Decimal, Decimal)> = t.sql("select qty, value from stock_balance where warehouse_id = $1", goods.parse().unwrap()).await;
    assert_eq!(got, vec![(n("7"), n("23.31"))]);
    let left: Vec<(Decimal, Decimal)> = t.sql("select qty, value from stock_balance where warehouse_id = $1", transit.parse().unwrap()).await;
    assert_eq!(left, vec![(n("0"), n("0"))], "transit empties to the cent");
    let claimed: Vec<(Decimal,)> = t
        .sql(
            &format!(
                "select coalesce(sum(debit - credit), 0) from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
                      where e.source_id = $1 and l.account_id = '{claims}'"
            ),
            done,
        )
        .await;
    assert_eq!(claimed, vec![(n("6.66"),)], "the claim is the shortage's value");
    assert!(t.k.cancel(&t.ctx, "StockEntry", sid, None).await.is_err(), "received transfers stay");

    // Cancelling the receipt gives the goods back to transit.
    t.k.cancel(&t.ctx, "StockEntry", done, None).await.expect("cancelled");
    assert_eq!(t.get("StockEntry", sid).await["transit_status"], "Partly Received");
    let left: Vec<(Decimal, Decimal)> = t.sql("select qty, value from stock_balance where warehouse_id = $1", transit.parse().unwrap()).await;
    assert_eq!(left, vec![(n("5"), n("16.65"))]);
}
