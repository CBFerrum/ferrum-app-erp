//! Stored value before an invoice exists, against a real PostgreSQL
//! (`FERRUM_ERP_SVHOLD_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_svhold_test`; reset first):
//! a kind taxed when redeemed is untaxed on the order as on its invoice; value held for an order is
//! not anyone else's to take, of two holds racing for the last of a card one wins, the order's invoice
//! redeems it once, and a cancelled order or the kind's hours release it.

mod common;
use common::*;
use ferrum_app_erp::stored_value;
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

#[test]
fn holds() {
    run(scenario());
}

fn n(s: &str) -> Decimal {
    s.parse().unwrap()
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_SVHOLD_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_svhold_test".into());
    let t = T::boot(url, |_, _| {}).await;
    let c = t.company.clone();
    let current = t.find_one("Account", json!({"account_name": "Current Liabilities", "company": c})).await["id"].clone();
    let liability =
        t.insert("Account", json!({"account_name": "Gift Cards", "company": c, "parent_account": current, "root_type": "Liability"})).await["id"].clone();
    let goodwill = t.account("Administrative Expenses").await;
    let kind = uid(&t
        .insert(
            "StoredValueKind",
            json!({"kind_name": "Gift Card", "company": c, "liability_account": liability, "adjustment_account": goodwill, "tax_charged": "When Redeemed"}),
        )
        .await);
    let output = t.account("Output Tax").await;
    let vat = t
        .insert("TaxTemplate", json!({"title": "VAT", "company": c, "kind": "Sales", "rows": [{"charge_type": "On Net Total", "account": output, "rate": "10", "description": "VAT"}]}))
        .await["id"]
        .clone();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await["id"].clone();
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await["id"].clone();
    let item = |code: &str, rate: &str, kind: Option<Uuid>| json!({"item_code": code, "item_name": code, "item_group": products, "stock_uom": nos, "standard_rate": rate, "is_stock_item": false, "stored_value_kind": kind});
    let card = uid(&t.insert("Item", item("Gift Card", "500", Some(kind))).await).to_string();
    let work = uid(&t.insert("Item", item("Work", "100", None)).await).to_string();
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();

    // Taxed when redeemed: no tax on the card's line, on the order as on the invoice.
    let cart = json!({"company": c, "customer": globex, "tax_template": vat, "items": [{"item": card, "qty": "1"}, {"item": work, "qty": "1"}]});
    let so = t.insert("SalesOrder", cart.clone()).await;
    let si = t.insert("SalesInvoice", cart).await;
    assert_eq!((dec(&so["total_taxes"]), dec(&so["grand_total"])), (n("10"), n("610")), "tax on the work only");
    assert_eq!((dec(&si["total_taxes"]), dec(&si["grand_total"])), (dec(&so["total_taxes"]), dec(&so["grand_total"])), "the cart and its invoice agree");

    // A card with 500 on it.
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    let out = stored_value::issue(&t.k, &t.ctx, &mut tx, kind, n("500"), "goodwill", None, Some("HOLD-0001-0001-0001"), None, "issue-1").await.unwrap();
    tx.commit().await.unwrap();
    let account: Uuid = out["account"].as_str().unwrap().parse().unwrap();
    let order = |qty: &str| json!({"company": c, "customer": globex, "tax_template": vat, "items": [{"item": work, "qty": qty}]});
    let hold = |order: Uuid, amount: &str, key: &str| {
        let (t, key) = (&t, key.to_string());
        let amount = n(amount);
        async move {
            let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
            let r = stored_value::hold(&t.k, &t.ctx, &mut tx, kind, "HOLD-0001-0001-0001", None, order, amount, &key).await;
            if r.is_ok() {
                tx.commit().await.unwrap();
            }
            r.map_err(|e| e.to_string())
        }
    };

    // Held for an order of 330: shown as held, and not anyone else's.
    let o1 = uid(&t.post("SalesOrder", order("3")).await);
    let h1 = hold(o1, "200", "hold-1").await.expect("hold");
    assert_eq!((dec(&h1["held"]), dec(&h1["available"])), (n("200"), n("300")));
    assert_eq!(dec(&t.get("StoredValueAccount", account).await["held"]), n("200"));
    let o2 = uid(&t.post("SalesOrder", order("4")).await);
    let e = hold(o2, "400", "hold-2").await.unwrap_err();
    assert!(e.contains("200 is held for orders"), "{e}");
    let other = uid(&t.post("SalesInvoice", order("4")).await);
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    let e =
        stored_value::redeem(&t.k, &t.ctx, &mut tx, kind, "HOLD-0001-0001-0001", None, other, Some(n("400")), None, "redeem-1").await.unwrap_err().to_string();
    drop(tx);
    assert!(e.contains("held for orders"), "a redemption cannot take what is held: {e}");
    // Two orders racing for the 300 left, 200 each: one gets it.
    let o3 = uid(&t.post("SalesOrder", order("2")).await);
    let (a, b) = tokio::join!(hold(o2, "200", "race-a"), hold(o3, "200", "race-b"));
    assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1, "{a:?} {b:?}");
    assert_eq!(dec(&t.get("StoredValueAccount", account).await["held"]), n("400"));
    let (won, lost) = if a.is_ok() { (o2, o3) } else { (o3, o2) };
    let _ = lost;

    // The order invoiced: the hold becomes one payment from the card, once.
    let inv: Uuid = t.action("SalesOrder", o1, "make_sales_invoice", json!({})).await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    let inv = t.submit("SalesInvoice", inv).await;
    assert_eq!((dec(&inv["grand_total"]), dec(&t.get("SalesInvoice", uid(&inv)).await["outstanding_amount"])), (n("330"), n("130")));
    let acc = t.get("StoredValueAccount", account).await;
    assert_eq!((dec(&acc["balance"]), dec(&acc["held"])), (n("300"), n("200")), "200 taken, the other order's 200 still held");
    let redeemed: Vec<(i64,)> = t.sql("select count(*) from stored_value_entry where account_id = $1 and entry_type = 'redeem'", account).await;
    assert_eq!(redeemed[0].0, 1);
    let states: Vec<(String,)> = t.sql("select state from stored_value_hold where order_id = $1", o1).await;
    assert_eq!(states, vec![("redeemed".to_string(),)]);

    // The other order cancelled: its hold is released.
    t.k.cancel(&t.ctx, "SalesOrder", won, None).await.expect("cancel");
    assert_eq!(dec(&t.get("StoredValueAccount", account).await["held"]), n("0"));
    // A hold whose hours have passed is released by the upkeep.
    let o4 = uid(&t.post("SalesOrder", order("1")).await);
    hold(o4, "50", "hold-4").await.expect("hold");
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    sqlx::query("update stored_value_hold set expires_at = now() - interval '1 minute' where order_id = $1").bind(o4).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    stored_value::upkeep(&t.k, &t.ctx, t.ctx.today()).await.unwrap();
    let states: Vec<(String,)> = t.sql("select state from stored_value_hold where order_id = $1", o4).await;
    assert_eq!(states, vec![("expired".to_string(),)]);
    assert_eq!(dec(&t.get("StoredValueAccount", account).await["held"]), n("0"));
    let mut tx = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    assert_eq!(stored_value::check(&t.ctx, &mut tx).await.unwrap(), Vec::<String>::new(), "the accounts still agree with their ledger");
}
