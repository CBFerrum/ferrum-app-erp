//! FIFO with negative stock, against a real PostgreSQL (`FERRUM_ERP_FIFO_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_fifo_test`; reset first). A company that allows negative
//! stock issues more than it holds; a receipt that comes in then first covers what is short, at its own cost
//! (the difference from what the shortfall left at goes to stock adjustment), and only what is left opens a cost
//! layer. After every step, for every FIFO item and warehouse: the open layers hold what the balance holds (none
//! at or below zero) at the balance's value, the stock moves add up to the balance, and the stock account holds
//! the balances. Moving average is untouched.

mod common;
use common::*;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

#[test]
fn fifo_with_negative_stock() {
    run(scenario());
}

fn n(s: &str) -> Decimal {
    s.parse().unwrap()
}

struct S {
    t: T,
    cid: Uuid,
}

impl S {
    async fn warehouse(&self, name: &str) -> Uuid {
        uid(&self.t.find_one("Warehouse", json!({"warehouse_name": name, "company": self.t.company})).await)
    }
    async fn item(&self, code: &str, method: &str) -> Uuid {
        let group = self.t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
        let nos = self.t.find_one("Uom", json!({"uom_name": "Nos"})).await;
        let item = json!({"item_code": code, "item_name": code, "item_group": group["id"], "stock_uom": nos["id"], "valuation_method": method});
        uid(&self.t.insert("Item", item).await)
    }
    async fn stock_entry(&self, item: Uuid, purpose: &str, mut line: Value) -> Uuid {
        line["item"] = json!(item.to_string());
        uid(&self.t.post("StockEntry", json!({"company": self.cid.to_string(), "purpose": purpose, "items": [line]})).await)
    }
    async fn receive(&self, item: Uuid, wh: Uuid, qty: &str, rate: &str) -> Uuid {
        self.stock_entry(item, "Material Receipt", json!({"qty": qty, "basic_rate": rate, "target_warehouse": wh.to_string()})).await
    }
    async fn issue(&self, item: Uuid, wh: Uuid, qty: &str) -> Uuid {
        self.stock_entry(item, "Material Issue", json!({"qty": qty, "source_warehouse": wh.to_string()})).await
    }
    async fn stock(&self, item: Uuid, wh: Uuid) -> (Decimal, Decimal) {
        let q = format!("select qty, value from stock_balance where item_id = $1 and warehouse_id = '{wh}'");
        let (qty, value) = self.t.sql::<(Decimal, Decimal)>(&q, item).await.pop().unwrap_or_default();
        (qty.normalize(), value.normalize())
    }
    /// The open layers, oldest first: (quantity left, unit cost).
    async fn layers(&self, item: Uuid, wh: Uuid) -> Vec<(Decimal, Decimal)> {
        let q = format!(
            "select qty_remaining, unit_cost from cost_layer where item_id = $1 and warehouse_id = '{wh}' and qty_remaining > 0 order by posting_ts, seq"
        );
        self.t.sql::<(Decimal, Decimal)>(&q, item).await.into_iter().map(|(a, b)| (a.normalize(), b.normalize())).collect()
    }
    /// What the engine booked to stock adjustment for `doc` (its stock value difference entries).
    async fn adjusted(&self, doc: Uuid) -> Decimal {
        let q = "select coalesce(sum(l.debit - l.credit), 0) from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
                 join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
                 where e.source_entity = 'StockValueAdjustment' and e.source_id = $1 and a.account_name = 'Stock Adjustment'";
        self.t.sql::<(Decimal,)>(q, doc).await[0].0.normalize()
    }
    async fn gl(&self, account: &str) -> Decimal {
        let q = format!(
            "select coalesce(sum(l.debit - l.credit), 0) from journal_line l join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
             where l.company_id = $1 and a.account_name = '{account}'"
        );
        self.t.sql::<(Decimal,)>(&q, self.cid).await[0].0.normalize()
    }
    /// The invariant, for every FIFO item and warehouse of the company, and the stock account against the balances.
    async fn check(&self, step: &str) {
        let q = "select i.item_code, w.warehouse_name, b.qty, b.value, \
                 coalesce((select sum(l.qty_remaining) from cost_layer l where l.tenant_id = b.tenant_id and l.item_id = b.item_id and l.warehouse_id = b.warehouse_id), 0), \
                 coalesce((select round(sum(l.qty_remaining * l.unit_cost), 2) from cost_layer l where l.tenant_id = b.tenant_id and l.item_id = b.item_id \
                   and l.warehouse_id = b.warehouse_id), 0), \
                 coalesce((select sum(m.value) from stock_move m where m.tenant_id = b.tenant_id and m.item_id = b.item_id and m.warehouse_id = b.warehouse_id), 0) \
                 from stock_balance b join e_item i on i.tenant_id = b.tenant_id and i.id = b.item_id \
                 join e_warehouse w on w.tenant_id = b.tenant_id and w.id = b.warehouse_id \
                 where b.company_id = $1 and i.valuation_method = 'FIFO' order by 1, 2";
        let rows: Vec<(String, String, Decimal, Decimal, Decimal, Decimal, Decimal)> = self.t.sql(q, self.cid).await;
        for (code, wh, qty, value, in_layers, layer_value, moved) in rows {
            let layers_ok = if qty > Decimal::ZERO { in_layers == qty && layer_value == value } else { in_layers.is_zero() };
            assert!(
                layers_ok && moved == value,
                "{step}: {code} in {wh}: the balance holds {} worth {}, the open layers {} worth {}, the moves add up to {}",
                qty.normalize(),
                value.normalize(),
                in_layers.normalize(),
                layer_value.normalize(),
                moved.normalize()
            );
        }
        let held: Vec<(Decimal,)> = self.t.sql("select coalesce(sum(value), 0) from stock_balance where company_id = $1", self.cid).await;
        assert_eq!(self.gl("Stock In Hand").await, held[0].0.normalize(), "{step}: the stock account holds the balances");
    }
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_FIFO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_fifo_test".into());
    let t = T::boot(url, |_, _| {}).await;
    let cid: Uuid = t.company.parse().unwrap();
    t.k.update(&t.ctx, "Company", cid, obj(json!({"allow_negative_stock": true})), None).await.expect("negative stock allowed");
    let s = S { t, cid };
    let (stores, goods) = (s.warehouse("Stores").await, s.warehouse("Finished Goods").await);

    // ── one receipt covers the shortfall ──
    // 5 at 10 in, 8 out: 3 short, left at 10. Then 10 come in at 12: 3 of them cover the shortfall (6 more than it
    // left at, to stock adjustment) and the other 7 are the layer.
    let one = s.item("FIFO-ONE", "FIFO").await;
    s.receive(one, stores, "5", "10").await;
    s.issue(one, stores, "8").await;
    assert_eq!(s.stock(one, stores).await, (n("-3"), n("-30")));
    s.check("short by 3").await;
    let covering = s.receive(one, stores, "10", "12").await;
    s.check("10 received into 3 short").await;
    assert_eq!(s.stock(one, stores).await, (n("7"), n("84")));
    assert_eq!(s.layers(one, stores).await, [(n("7"), n("12"))]);
    assert_eq!(s.adjusted(covering).await, n("6"), "3 left at 10 and cost 12");
    s.issue(one, stores, "7").await;
    assert_eq!((s.stock(one, stores).await, s.layers(one, stores).await), ((n("0"), n("0")), vec![]), "the 7 left take the layer's 84");
    s.check("emptied").await;

    // ── a shortfall larger than the receipt, covered by two ──
    let two = s.item("FIFO-TWO", "FIFO").await;
    s.receive(two, stores, "2", "10").await;
    s.issue(two, stores, "12").await;
    assert_eq!(s.stock(two, stores).await, (n("-10"), n("-100")));
    let first = s.receive(two, stores, "4", "12").await;
    s.check("4 received into 10 short").await;
    assert_eq!((s.stock(two, stores).await, s.layers(two, stores).await), ((n("-6"), n("-60")), vec![]), "still short: no layer opens");
    assert_eq!(s.adjusted(first).await, n("8"));
    let second = s.receive(two, stores, "10", "15").await;
    s.check("10 received into 6 short").await;
    assert_eq!((s.stock(two, stores).await, s.layers(two, stores).await), ((n("4"), n("60")), vec![(n("4"), n("15"))]));
    assert_eq!(s.adjusted(second).await, n("30"));
    // The first receipt can no longer be taken back exactly: the second covered the rest since.
    let late = s.t.k.cancel(&s.t.ctx, "StockEntry", first, None).await;
    assert!(format!("{late:?}").contains("has moved since"), "{late:?}");
    s.check("the first receipt kept").await;

    // ── a transfer into a warehouse that is short ──
    // FIFO-ONE: 1 at 18 into Finished Goods, 4 out (3 short at 18); 5 come from Stores at 12 each.
    s.receive(one, stores, "9", "12").await;
    s.receive(one, goods, "1", "18").await;
    s.issue(one, goods, "4").await;
    assert_eq!(s.stock(one, goods).await, (n("-3"), n("-54")));
    let moved =
        s.stock_entry(one, "Material Transfer", json!({"qty": "5", "source_warehouse": stores.to_string(), "target_warehouse": goods.to_string()})).await;
    s.check("5 moved into 3 short").await;
    assert_eq!((s.stock(one, stores).await, s.layers(one, stores).await), ((n("4"), n("48")), vec![(n("4"), n("12"))]));
    assert_eq!((s.stock(one, goods).await, s.layers(one, goods).await), ((n("2"), n("24")), vec![(n("2"), n("12"))]));
    assert_eq!(s.adjusted(moved).await, n("-18"), "3 left at 18 and cost 12");

    // ── a purchase receipt into a shortfall, returned in part ──
    let bought = s.item("FIFO-BOUGHT", "FIFO").await;
    let acme = uid(&s.t.insert("Supplier", json!({"supplier_name": "Acme"})).await);
    s.receive(bought, stores, "1", "10").await;
    s.issue(bought, stores, "4").await;
    let srbnb = s.gl("Stock Received But Not Billed").await;
    let line = json!({"item": bought.to_string(), "qty": "10", "price_list_rate": "11", "warehouse": stores.to_string()});
    let pr = uid(&s.t.insert("PurchaseReceipt", json!({"company": s.cid.to_string(), "supplier": acme.to_string(), "items": [line]})).await);
    s.t.submit("PurchaseReceipt", pr).await;
    s.check("bought 10 into 3 short").await;
    assert_eq!((s.stock(bought, stores).await, s.layers(bought, stores).await), ((n("7"), n("77")), vec![(n("7"), n("11"))]));
    assert_eq!((s.adjusted(pr).await, s.gl("Stock Received But Not Billed").await - srbnb), (n("3"), n("-110")), "owed at what came in");
    let back = s.t.action("PurchaseReceipt", pr, "make_purchase_return", json!({})).await.expect("a return");
    let back: Uuid = back["id"].as_str().unwrap().parse().unwrap();
    let mut items = s.t.get("PurchaseReceipt", back).await["items"].clone();
    items[0]["qty"] = json!("4");
    s.t.k.update(&s.t.ctx, "PurchaseReceipt", back, obj(json!({"items": items})), None).await.expect("4 go back");
    s.t.submit("PurchaseReceipt", back).await;
    s.check("4 returned").await;
    assert_eq!((s.stock(bought, stores).await, s.layers(bought, stores).await), ((n("3"), n("33")), vec![(n("3"), n("11"))]));

    // ── cancelling the receipt that covered the shortfall puts the layers and the ledger back ──
    let undone = s.item("FIFO-UNDONE", "FIFO").await;
    s.receive(undone, stores, "2", "10").await;
    s.issue(undone, stores, "5").await;
    let before = (s.stock(undone, stores).await, s.gl("Stock In Hand").await, s.gl("Stock Adjustment").await);
    let covering = s.receive(undone, stores, "10", "13").await;
    assert_eq!(
        (s.stock(undone, stores).await, s.layers(undone, stores).await, s.adjusted(covering).await),
        ((n("7"), n("91")), vec![(n("7"), n("13"))], n("9"))
    );
    s.t.k.cancel(&s.t.ctx, "StockEntry", covering, None).await.expect("cancelled");
    s.check("the covering receipt cancelled").await;
    assert_eq!(s.layers(undone, stores).await, vec![]);
    assert_eq!((s.stock(undone, stores).await, s.gl("Stock In Hand").await, s.gl("Stock Adjustment").await), before, "as before the receipt");
    assert_eq!(s.adjusted(covering).await, Decimal::ZERO, "the difference is taken back too");

    // ── moving average: the same sequence, the same figures as before FIFO's fix ──
    let avg = s.item("AVG", "Moving Average").await;
    s.receive(avg, stores, "5", "10").await;
    s.issue(avg, stores, "8").await;
    assert_eq!(s.stock(avg, stores).await, (n("-3"), n("-30")));
    let into = s.receive(avg, stores, "10", "12").await;
    assert_eq!((s.stock(avg, stores).await, s.adjusted(into).await), ((n("7"), n("90")), Decimal::ZERO), "pooled: 90 for 7");
    s.issue(avg, stores, "4").await;
    assert_eq!(s.stock(avg, stores).await, (n("3"), n("38.57")), "4 at 90/7");
    s.check("the end").await;
}
