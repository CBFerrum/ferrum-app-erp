//! Promotions on sales documents, end to end against a real PostgreSQL
//! (`FERRUM_ERP_PROMO_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_promo_test`; reset first): a document that
//! no promotion fits prices as before; a code, free delivery and a free item on one order, after a
//! pricing rule, kept line by line; a code counted once on submission, refused when used up exactly
//! as when unknown, the last use won by one of two orders at once and given back on cancelling; an
//! invoice from the order and a credit note for half a line giving back half; the report by
//! promotion, month and channel; and the contracted query pricing lines without saving anything.

use ferrum_kernel::capability::Invocation;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Result};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap()
}

fn dec(v: &Value) -> Decimal {
    match v {
        Value::String(s) => Decimal::from_str(s).unwrap_or_else(|_| panic!("not a decimal: {s}")),
        Value::Number(n) => Decimal::from_str(&n.to_string()).unwrap(),
        Value::Null => Decimal::ZERO,
        other => panic!("not a decimal: {other}"),
    }
}

fn n(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn uid(doc: &Doc) -> Uuid {
    doc["id"].as_str().unwrap().parse().unwrap()
}

fn message<T: std::fmt::Debug>(r: Result<T>) -> String {
    match &r {
        Err(Error::Validation(e)) => e.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        Err(Error::Invalid(m)) => m.clone(),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

struct T {
    k: Arc<Kernel>,
    ctx: Ctx,
}

impl T {
    async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.k.insert(&self.ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.ctx, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let page =
            self.k.list(&self.ctx, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() }).await.unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
    async fn used(&self, entity: &str, id: Uuid) -> i64 {
        self.get(entity, id).await["used_count"].as_i64().unwrap_or(0)
    }
}

/// Each line's amount, by item name.
fn amounts(doc: &Doc) -> Vec<(String, Decimal)> {
    doc["items"].as_array().unwrap().iter().map(|l| (l["item_name"].as_str().unwrap_or("").to_string(), dec(&l["amount"]))).collect()
}

#[test]
fn promotions_on_sales_documents() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_PROMO_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_promo_test".into());
    let db = Db::connect(DbConfig { url, max_connections: 10, ..Default::default() }).await.expect("connect");
    sqlx::raw_sql("drop schema if exists public cascade; create schema public;").execute(&db.pool).await.expect("reset");
    let mut packages = ferrum_domain::packages().expect("the shared packages");
    packages.extend(ferrum_app_erp::packages().expect("ERP's packages"));
    let mut hooks = HookRegistry::default();
    ferrum_domain::register(&mut hooks);
    ferrum_app_erp::register(&mut hooks);
    for source in ferrum_app_erp::reports() {
        ferrum_domain::reports::add_source(source);
    }
    let k: Arc<Kernel> = Kernel::boot(db, Catalog::from_packages(packages).unwrap(), hooks, KernelSettings::default(), None).await.expect("boot");
    ferrum_domain::maintenance(&k).await.expect("maintenance");
    ferrum_app_erp::maintenance(&k).await.expect("partitions");
    let tenant = k
        .create_tenant(NewTenant {
            slug: "promo".into(),
            name: "Promo Co".into(),
            admin_email: "admin@promo.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling"].iter().map(|s| s.to_string()).collect(),
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("promo", "admin@promo.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), ctx };

    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let company = uid(&t.insert("Company", json!({"company_name": "Promo Co", "abbr": "PC", "default_currency": inr["id"]})).await);
    let c = company.to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |name: &str, rate: &str, delivery: bool| {
        json!({"item_code": name, "item_name": name, "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": rate, "is_stock_item": false,
               "is_delivery_charge": delivery})
    };
    let a = uid(&t.insert("Item", item("Lamp", "100", false)).await).to_string();
    let b = uid(&t.insert("Item", item("Shade", "50", false)).await).to_string();
    let ship = uid(&t.insert("Item", item("Delivery", "10", true)).await).to_string();
    let gift = uid(&t.insert("Item", item("Bulb", "20", false)).await).to_string();
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let output = t.find_one("Account", json!({"account_name": "Output Tax", "company": c})).await["id"].clone();
    let vat = uid(&t
        .insert("TaxTemplate", json!({"title": "VAT 10%", "company": c, "kind": "Sales", "rows": [{"charge_type": "On Net Total", "account": output, "description": "VAT", "rate": "10"}]}))
        .await)
    .to_string();
    // An item-level pricing rule: it comes first, promotions take off what it leaves.
    t.insert("PricingRule", json!({"title": "Shades 10% off", "apply_on": "Item", "item": b, "discount_percent": "10"})).await;
    let save10 = uid(&t
        .insert("Promotion", json!({"title": "10 off with a code", "gives": "Amount Off the Document", "amount": "10", "requires_code": true, "priority": 3}))
        .await);
    let free_ship =
        uid(&t.insert("Promotion", json!({"title": "Free delivery online", "gives": "Free Delivery", "channels": "Web Shop", "priority": 2})).await);
    let bulb = uid(&t
        .insert("Promotion", json!({"title": "A bulb over 200", "gives": "Free Item", "free_item": gift, "free_qty": "1", "min_amount": "200", "priority": 1}))
        .await);
    let code = uid(&t.insert("PromotionCode", json!({"code": " save10 ", "promotion": save10.to_string(), "max_uses": 1})).await);
    assert_eq!(t.get("PromotionCode", code).await["code"], json!("SAVE10"), "kept in capitals");
    let order = |lines: Value, extra: Value| {
        let mut o = json!({"company": c, "customer": globex, "tax_template": vat, "items": lines});
        o.as_object_mut().unwrap().extend(obj(extra));
        o
    };

    // No promotion fits: priced exactly as before (the rule's 10% on the shade, VAT on top).
    let plain = t.insert("SalesOrder", order(json!([{"item": a, "qty": "1"}, {"item": b, "qty": "1"}]), json!({}))).await;
    assert_eq!(amounts(&plain), [("Lamp".to_string(), n("100")), ("Shade".to_string(), n("45"))]);
    assert_eq!((dec(&plain["net_total"]), dec(&plain["grand_total"])), (n("145"), n("159.5")));
    assert!(plain["applied_promotions"].as_array().is_none_or(Vec::is_empty));
    assert!(plain["items"].as_array().unwrap().iter().all(|l| l["promotion_percent"].is_null()));
    // No country's add-on: no tax registrations to print.
    assert_eq!(plain["tax_registrations"], json!({"seller": [], "buyer": [], "buyer_registered": false}));

    // Online with the code: 10 off the goods spread 200 : 45, delivery free, and the bulb (the
    // spend after the 10 off is 235) added as a line and taken whole.
    let lines = json!([{"item": a, "qty": "2"}, {"item": b, "qty": "1"}, {"item": ship, "qty": "1"}]);
    let web = t.insert("SalesOrder", order(lines.clone(), json!({"channel": "Web Shop", "promotion_codes": "save10"}))).await;
    let so = uid(&web);
    assert_eq!(
        amounts(&web),
        [("Lamp".to_string(), n("191.84")), ("Shade".to_string(), n("43.16")), ("Delivery".to_string(), n("0")), ("Bulb".to_string(), n("0"))]
    );
    assert_eq!((dec(&web["net_total"]), dec(&web["total_taxes"]), dec(&web["grand_total"])), (n("235"), n("23.5"), n("258.5")));
    let applied: Vec<(String, Decimal, Value)> = web["applied_promotions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["title"].as_str().unwrap().to_string(), dec(&r["amount"]), r["code"].clone()))
        .collect();
    assert_eq!(
        applied,
        [
            ("10 off with a code".to_string(), n("8.16"), json!("SAVE10")),
            ("10 off with a code".to_string(), n("1.84"), json!("SAVE10")),
            ("Free delivery online".to_string(), n("10"), Value::Null),
            ("A bulb over 200".to_string(), n("20"), Value::Null),
        ]
    );
    // Saved again, nothing doubles: the same lines, the same bulb.
    let again = t.k.update(&t.ctx, "SalesOrder", so, obj(json!({"po_no": "WEB-1"})), None).await.unwrap();
    assert_eq!(amounts(&again), amounts(&web));
    t.submit("SalesOrder", so).await;
    assert_eq!(
        (t.used("PromotionCode", code).await, t.used("Promotion", save10).await, t.used("Promotion", free_ship).await, t.used("Promotion", bulb).await),
        (1, 1, 1, 1)
    );

    // Used up, or never made: the same answer.
    let used_up = message(t.k.insert(&t.ctx, "SalesOrder", obj(order(lines.clone(), json!({"promotion_codes": "SAVE10"})))).await);
    let unknown = message(t.k.insert(&t.ctx, "SalesOrder", obj(order(lines.clone(), json!({"promotion_codes": "NOPE10"})))).await);
    assert_eq!(used_up.replace("SAVE10", "X"), unknown.replace("NOPE10", "X"));
    assert!(used_up.contains("SAVE10 cannot be used"), "{used_up}");

    // The last use of a code, taken by two orders at once: one wins, and cancelling gives it back.
    let last = uid(&t.insert("PromotionCode", json!({"code": "LAST-1", "promotion": save10.to_string(), "max_uses": 1})).await);
    let one = uid(&t.insert("SalesOrder", order(json!([{"item": a, "qty": "1"}]), json!({"promotion_codes": "LAST-1"}))).await);
    let two = uid(&t.insert("SalesOrder", order(json!([{"item": a, "qty": "1"}]), json!({"promotion_codes": "LAST-1"}))).await);
    let (r1, r2) = tokio::join!(t.k.submit(&t.ctx, "SalesOrder", one, None), t.k.submit(&t.ctx, "SalesOrder", two, None));
    assert_eq!(r1.is_ok() as u8 + r2.is_ok() as u8, 1, "{r1:?} {r2:?}");
    assert!(message(if r1.is_err() { r1 } else { r2 }).contains("LAST-1 cannot be used"));
    assert_eq!(t.used("PromotionCode", last).await, 1);
    let won = if t.get("SalesOrder", one).await["docstatus"] == json!(1) { one } else { two };
    t.k.cancel(&t.ctx, "SalesOrder", won, None).await.expect("cancel");
    assert_eq!((t.used("PromotionCode", last).await, t.used("Promotion", save10).await), (0, 1));

    // The invoice from the order keeps each line's share (the order has the promotions); a credit
    // note for one of the two lamps gives back half the lamp line, and half its tax.
    let si: Uuid = t.k.run_action(&t.ctx, "SalesOrder", so, "make_sales_invoice", json!({})).await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    let inv = t.submit("SalesInvoice", si).await;
    assert_eq!(amounts(&inv), amounts(&web));
    assert_eq!(dec(&inv["grand_total"]), n("258.5"));
    assert!(inv["applied_promotions"].as_array().is_none_or(Vec::is_empty), "counted on the order, not again");
    assert_eq!(t.used("PromotionCode", code).await, 1);
    let cn: Uuid = t.k.run_action(&t.ctx, "SalesInvoice", si, "make_credit_note", json!({})).await.unwrap()["id"].as_str().unwrap().parse().unwrap();
    let cn_doc = t.get("SalesInvoice", cn).await;
    let lamp = cn_doc["items"].as_array().unwrap().iter().find(|l| l["item"] == json!(a)).unwrap().clone();
    let cn_doc = t.k.update(&t.ctx, "SalesInvoice", cn, obj(json!({"items": [{"id": lamp["id"], "qty": "-1"}]})), None).await.unwrap();
    assert_eq!((dec(&cn_doc["net_total"]), dec(&cn_doc["total_taxes"]), dec(&cn_doc["grand_total"])), (n("-95.92"), n("-9.59"), n("-105.51")));

    // What was given, by promotion, month and channel: the order, once.
    let day = inv["posting_date"].as_str().unwrap().to_string();
    let raw = [("company", c.clone()), ("from_date", day.clone()), ("to_date", day.clone())].into_iter().map(|(a, b)| (a.to_string(), b)).collect();
    let report = ferrum_domain::reports::run(&t.k, &t.ctx, "promotions", &raw).await.expect("promotions report");
    let rows: Vec<(String, String, Decimal)> = report["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["promotion"].as_str().unwrap().to_string(), r["channel"].as_str().unwrap().to_string(), dec(&r["discount"])))
        .collect();
    assert_eq!(
        rows,
        [
            ("10 off with a code".to_string(), "Web Shop".to_string(), n("10")),
            ("A bulb over 200".to_string(), "Web Shop".to_string(), n("20")),
            ("Free delivery online".to_string(), "Web Shop".to_string(), n("10")),
        ]
    );
    assert_eq!(dec(&report["totals"]["discount"]), n("40"));

    // The contracted query: priced, nothing saved, no code counted; a used-up code comes back
    // rejected, like an unknown one.
    let orders_before = t.k.list(&t.ctx, "SalesOrder", &ListQuery::default()).await.unwrap().rows.len();
    let params = json!({"lines": serde_json::to_string(&lines).unwrap(), "customer": globex, "channel": "Web Shop", "codes": "SAVE10 nope10"});
    let receipt = t.k.invoke(&t.ctx, Invocation::new("core.company.price_lines", json!({"id": c, "params": params}))).await.expect("price_lines");
    let out = receipt.result;
    assert_eq!(out["rejected_codes"], json!(["SAVE10", "NOPE10"]));
    let titles: Vec<&str> = out["promotions"].as_array().unwrap().iter().map(|p| p["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["Free delivery online", "A bulb over 200"]);
    assert_eq!(out["lines"].as_array().unwrap().len(), 4, "the bulb added");
    assert_eq!(dec(&out["net_total"]), n("245"));
    assert_eq!(t.k.list(&t.ctx, "SalesOrder", &ListQuery::default()).await.unwrap().rows.len(), orders_before);
    assert_eq!(t.used("PromotionCode", code).await, 1);
}
