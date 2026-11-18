//! What ERP dates "today" is the workspace's day, never UTC's, against a real PostgreSQL
//! (`FERRUM_ERP_DAY_TEST_DATABASE_URL`, default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_day_test`; reset first).
//! No test can set the clock: the workspace moves to a zone whose day is not UTC's now, whatever the hour (Pago Pago,
//! UTC-11, before 10:00 UTC; Kiritimati, UTC+14, after), as the platform's own suites do, so a date taken from UTC is a
//! day out. Stored value issued and adjusted, a document cancelled after its month was closed, a card on its last day
//! in the daily upkeep and an order's hold on its last day all go by the workspace's day.

mod common;
use chrono::{Datelike, Duration, NaiveDate, Timelike};
use common::*;
use ferrum_app_erp::stored_value;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

#[test]
fn dates_on_the_workspaces_day() {
    run(scenario());
}

fn n(s: &str) -> Decimal {
    s.parse().unwrap()
}

fn vid(v: &Value) -> Uuid {
    v.as_str().unwrap().parse().unwrap()
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_DAY_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_day_test".into());
    let mut t = T::boot(url, |_, _| {}).await;
    let zone = if chrono::Utc::now().hour() < 10 { "Pacific/Pago_Pago" } else { "Pacific/Kiritimati" };
    t.k.update_tenant_settings(&t.ctx, &obj(json!({"timezone": zone}))).await.expect("the workspace's zone");
    t.ctx = t.k.load_ctx(t.ctx.tenant, t.ctx.user, None, "it".into()).await.expect("its context");
    let today = t.ctx.today();
    assert_ne!(today, chrono::Utc::now().date_naive(), "{zone}'s day is not UTC's");
    let (c, tenant) = (t.company.clone(), t.ctx.tenant);
    let dates = |sql: &'static str, id: Uuid| {
        let t = &t;
        async move { t.sql::<(NaiveDate,)>(sql, id).await.into_iter().map(|r| r.0).collect::<Vec<_>>() }
    };

    // ── stored value issued and adjusted by hand: its entries and journals on the workspace's day ──
    let liability = t
        .insert(
            "Account",
            json!({"account_name": "Gift Cards", "company": c, "parent_account": t.account("Current Liabilities").await, "root_type": "Liability"}),
        )
        .await;
    let goodwill = t.account("Administrative Expenses").await;
    let kind = uid(&t
        .insert("StoredValueKind", json!({"kind_name": "Gift Card", "company": c, "liability_account": liability["id"], "adjustment_account": goodwill}))
        .await);
    let issue = |amount: &'static str, key: &'static str| {
        let t = &t;
        async move {
            let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
            let out = stored_value::issue(&t.k, &t.ctx, &mut tx, kind, n(amount), "goodwill", None, None, None, key).await.expect("issued");
            tx.commit().await.unwrap();
            out
        }
    };
    let card = issue("100", "day:card").await;
    let journal = "select distinct posting_date from journal_entry where source_id = $1";
    let entries = "select distinct posting_date from stored_value_entry where op_id = $1";
    assert_eq!(dates(journal, vid(&card["entry"])).await, [today], "issued on the workspace's day");
    assert_eq!(dates(entries, vid(&card["entry"])).await, [today]);
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    let fixed = stored_value::adjust(&t.k, &t.ctx, &mut tx, vid(&card["account"]), n("-10"), "keyed twice").await.expect("adjusted");
    tx.commit().await.unwrap();
    assert_eq!((dates(journal, vid(&fixed["entry"])).await, dates(entries, vid(&fixed["entry"])).await), (vec![today], vec![today]), "adjusted on it too");

    // ── a document cancelled after its month was closed: the reversal is dated today, the workspace's ──
    let group = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = uid(&t.insert("Item", json!({"item_code": "DAY", "item_name": "Day", "item_group": group["id"], "stock_uom": nos["id"]})).await);
    let stores = uid(&t.find_one("Warehouse", json!({"warehouse_name": "Stores", "company": c})).await);
    let month = (today.with_day(1).unwrap() - Duration::days(1)).with_day(1).unwrap();
    let years = t.find("FiscalYear", json!({"company": c})).await;
    let covering =
        years.iter().find(|y| y["start_date"].as_str().unwrap() <= month.to_string().as_str() && y["end_date"].as_str().unwrap() >= month.to_string().as_str());
    let year = match covering {
        Some(y) => uid(y),
        None => uid(&t
            .insert("FiscalYear", json!({"year_name": format!("FY {}", month.year()), "company": c, "start_date": format!("{}-01-01", month.year()), "end_date": format!("{}-12-31", month.year())}))
            .await),
    };
    let receipt = json!({"company": c, "purpose": "Material Receipt", "posting_date": (month + Duration::days(4)).to_string(),
                         "items": [{"item": item.to_string(), "qty": "5", "basic_rate": "10", "target_warehouse": stores.to_string()}]});
    let receipt = uid(&t.post("StockEntry", receipt).await);
    t.action("FiscalYear", year, "close_period", json!({"period_start": month.to_string()})).await.expect("its month closed");
    t.k.cancel(&t.ctx, "StockEntry", receipt, None).await.expect("cancelled into today");
    let reversed = "select distinct posting_date from journal_entry where source_id = $1 and reversal_of is not null";
    let moved_back = "select distinct posting_date from stock_move where source_id = $1 and reversal_of is not null";
    assert_eq!((dates(reversed, receipt).await, dates(moved_back, receipt).await), (vec![today], vec![today]), "reversed on the workspace's day");

    // ── the daily upkeep: a card expires the day after its last day where the workspace is ──
    let (last_day_gone, last_day_today) = (issue("5", "day:yesterday").await, issue("5", "day:today").await);
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    for (card, last) in [(&last_day_gone, today - Duration::days(1)), (&last_day_today, today)] {
        sqlx::query("update e_stored_value_account set expires_on = $2 where id = $1").bind(vid(&card["account"])).bind(last).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    ferrum_app_erp::maintenance(&t.k, tenant).await.expect("the daily upkeep");
    let state = |card: &Value| {
        let (t, id) = (&t, vid(&card["account"]));
        async move { t.get("StoredValueAccount", id).await["state"].as_str().unwrap().to_string() }
    };
    assert_eq!((state(&last_day_gone).await, state(&last_day_today).await), ("Expired".into(), "Active".into()), "the day after its last day, not before");

    // ── an order holds its stock through its last day where the workspace is, and not after ──
    let cid: Uuid = c.parse().unwrap();
    t.k.update(&t.ctx, "Company", cid, obj(json!({"reservation_policy": "Hold", "reservation_days": 3})), None).await.expect("hold stock");
    t.post("StockEntry", json!({"company": c, "purpose": "Material Receipt", "items": [{"item": item.to_string(), "qty": "2", "basic_rate": "10", "target_warehouse": stores.to_string()}]}))
        .await;
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await);
    let order = json!({"company": c, "customer": globex.to_string(), "set_warehouse": stores.to_string(), "items": [{"item": item.to_string(), "qty": "1"}]});
    let mut lines = Vec::new();
    for _ in 0..2 {
        lines.push(vid(&t.post("SalesOrder", order.clone()).await["items"][0]["id"]));
    }
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    for (line, until) in lines.into_iter().zip([today - Duration::days(1), today]) {
        sqlx::query("update e_sales_order__items set reserved_until = $2 where id = $1").bind(line).bind(until).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let walk_in =
        |qty: &str| json!({"company": c, "customer": globex.to_string(), "items": [{"item": item.to_string(), "qty": qty, "warehouse": stores.to_string()}]});
    let both = t.try_post("DeliveryNote", walk_in("2")).await;
    assert!(both.as_ref().is_err_and(|e| e.contains("held for other orders")), "the order held until today still holds: {both:?}");
    t.try_post("DeliveryNote", walk_in("1")).await.expect("the order held until yesterday holds nothing");
}
