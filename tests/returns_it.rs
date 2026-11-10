//! Returns, credit and debit notes and refunds, end to end against a real PostgreSQL
//! (`FERRUM_ERP_RETURNS_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_returns_test`; reset first).
//!
//! Every correction a company moving from other books expects: a delivery, receipt or invoice
//! returned in part and in full, cancelled and returned again; a credit of price alone; a note with
//! no original; goods back at the cost they left at; serial numbers; a debit note to a customer;
//! a credit given back as money. Each step checks the ledger lines and what the party is owed, not
//! only statuses.

use chrono::Datelike;
use ferrum_kernel::tenant::NewTenant;
use ferrum_kernel::{Ctx, Db, DbConfig, Doc, Error, HookRegistry, Kernel, KernelSettings, ListQuery, Result};
use ferrum_meta::Catalog;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::BTreeMap;
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

fn vid(v: &Value) -> Uuid {
    v["id"].as_str().unwrap().parse().unwrap()
}

/// The refusal's words contain `words`.
fn refused<T: std::fmt::Debug>(r: Result<T>, words: &str) {
    let text = match &r {
        Err(Error::Invalid(m)) => m.clone(),
        Err(Error::Validation(e)) => e.iter().map(|f| format!("{}: {}", f.field, f.message)).collect::<Vec<_>>().join("; "),
        other => panic!("expected a refusal with {words:?}, got {other:?}"),
    };
    assert!(text.contains(words), "{words:?} not in {text:?}");
}

struct T {
    k: Arc<Kernel>,
    ctx: Ctx,
}

impl T {
    async fn insert(&self, entity: &str, v: Value) -> Doc {
        self.k.insert(&self.ctx, entity, obj(v)).await.unwrap_or_else(|e| panic!("insert {entity}: {e:?}"))
    }
    async fn update(&self, entity: &str, id: Uuid, v: Value) -> Doc {
        self.k.update(&self.ctx, entity, id, obj(v), None).await.unwrap_or_else(|e| panic!("update {entity}: {e:?}"))
    }
    async fn submit(&self, entity: &str, id: Uuid) -> Doc {
        self.k.submit(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("submit {entity}: {e:?}"))
    }
    async fn cancel(&self, entity: &str, id: Uuid) {
        self.k.cancel(&self.ctx, entity, id, None).await.unwrap_or_else(|e| panic!("cancel {entity}: {e:?}"));
    }
    async fn get(&self, entity: &str, id: Uuid) -> Doc {
        self.k.get(&self.ctx, entity, id, false).await.unwrap_or_else(|e| panic!("get {entity}: {e:?}"))
    }
    async fn action(&self, entity: &str, id: Uuid, action: &str, params: Value) -> Uuid {
        let v = self.k.run_action(&self.ctx, entity, id, action, params).await.unwrap_or_else(|e| panic!("{entity}.{action}: {e:?}"));
        vid(&v)
    }
    async fn find_one(&self, entity: &str, filters: Value) -> Doc {
        let page =
            self.k.list(&self.ctx, entity, &ListQuery { filters: ferrum_kernel::query::parse_filters(&filters).unwrap(), ..Default::default() }).await.unwrap();
        assert_eq!(page.rows.len(), 1, "{entity} {filters}");
        page.rows[0].clone()
    }
    async fn scalar(&self, sql: &'static str, a: Uuid) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        let v: Option<Decimal> = sqlx::query_scalar(sql).bind(self.ctx.tenant).bind(a).fetch_one(&mut *tx).await.unwrap();
        v.unwrap_or_default()
    }
    /// Debit minus credit on an account (by name), all postings.
    async fn gl(&self, account: &str) -> Decimal {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        sqlx::query_scalar(
            "select coalesce(sum(l.debit - l.credit), 0) from journal_line l join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
             where l.tenant_id = $1 and a.account_name = $2",
        )
        .bind(self.ctx.tenant)
        .bind(account)
        .fetch_one(&mut *tx)
        .await
        .unwrap()
    }
    /// What a document posted, by account name: debit minus credit, company currency, reversals included.
    async fn posted(&self, source: Uuid) -> BTreeMap<String, Decimal> {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        let rows: Vec<(String, Decimal)> = sqlx::query_as(
            "select a.account_name, sum(l.debit - l.credit) from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
             join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id where l.tenant_id = $1 and e.source_id = $2 group by 1 having sum(l.debit - l.credit) <> 0",
        )
        .bind(self.ctx.tenant)
        .bind(source)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        rows.into_iter().collect()
    }
    /// What a customer owes (positive) or is owed (negative): the outstanding of their open items.
    async fn owes(&self, customer: Uuid) -> Decimal {
        self.scalar("select sum(outstanding) from open_item where tenant_id = $1 and customer_id = $2 and status <> 'cancelled'", customer).await
    }
    async fn owed_supplier(&self, supplier: Uuid) -> Decimal {
        self.scalar("select sum(outstanding) from open_item where tenant_id = $1 and supplier_id = $2 and status <> 'cancelled'", supplier).await
    }
    async fn stock(&self, item: Uuid, wh: Uuid) -> (Decimal, Decimal) {
        let mut tx = self.k.db.tenant_tx(self.ctx.tenant).await.unwrap();
        sqlx::query_as::<_, (Decimal, Decimal)>("select qty, value from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = $3")
            .bind(self.ctx.tenant)
            .bind(item)
            .bind(wh)
            .fetch_optional(&mut *tx)
            .await
            .unwrap()
            .unwrap_or_default()
    }
    async fn moves(&self, source: Uuid) -> Decimal {
        self.scalar("select count(*)::numeric from stock_move where tenant_id = $1 and source_id = $2", source).await
    }
    fn line(&self, doc: &Doc, i: usize) -> Doc {
        doc["items"][i].as_object().cloned().unwrap()
    }
}

fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, Decimal> {
    pairs.iter().map(|(a, v)| (a.to_string(), n(v))).collect()
}

#[test]
fn returns_and_refunds() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(16 << 20).build().unwrap();
    rt.block_on(async { tokio::spawn(scenario()).await.unwrap() });
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_RETURNS_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_returns_test".into());
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
            slug: "rr".into(),
            name: "Returns Co".into(),
            admin_email: "admin@rr.test".into(),
            admin_password: "Correct-Horse-9".into(),
            admin_name: None,
            packages: ["core", "accounting", "inventory", "selling", "buying"].iter().map(|s| s.to_string()).collect(),
            settings: None,
        })
        .await
        .expect("tenant");
    let session = k.login("rr", "admin@rr.test", "Correct-Horse-9").await.unwrap();
    let ctx = k.load_ctx(tenant, session.user, Some(session.token_version), "it".into()).await.unwrap();
    let t = T { k: k.clone(), ctx };

    // ───────────── a company, its parties, items and taxes ─────────────
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let usd = t.find_one("Currency", json!({"code": "USD"})).await;
    let cid = uid(&t.insert("Company", json!({"company_name": "Returns Co", "abbr": "RC", "default_currency": inr["id"]})).await);
    let co = t.get("Company", cid).await;
    let bank = co["default_bank_account"].as_str().unwrap().to_string();
    let c = cid.to_string();
    let stores = uid(&t.find_one("Warehouse", json!({"warehouse_name": "Stores", "company": c})).await);
    let wh = stores.to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |code: &str, more: Value| {
        let mut v = json!({"item_code": code, "item_name": code, "item_group": products["id"], "stock_uom": nos["id"]});
        v.as_object_mut().unwrap().extend(more.as_object().cloned().unwrap());
        v
    };
    let widget = uid(&t.insert("Item", item("Widget", json!({"standard_rate": "150"}))).await);
    let advice = uid(&t.insert("Item", item("Advice", json!({"standard_rate": "100", "is_stock_item": false}))).await);
    let phone = uid(&t.insert("Item", item("Phone", json!({"standard_rate": "900", "has_serial_no": true}))).await);
    let steel = uid(&t.insert("Item", item("Steel", json!({}))).await);
    // Sales prices come from price lists and items: these, at 83 and 83.17 to the dollar.
    let export_item = uid(&t.insert("Item", item("Export", json!({"standard_rate": "12450", "is_stock_item": false}))).await);
    let mut odd = Vec::new();
    for (code, rate) in [("Odd A", "2772.06"), ("Odd B", "1080.38"), ("Odd C", "5.82")] {
        odd.push(uid(&t.insert("Item", item(code, json!({"standard_rate": rate, "is_stock_item": false}))).await).to_string());
    }
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await);
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await);
    let account = |name: &'static str| {
        let t = &t;
        let c = c.clone();
        async move { t.find_one("Account", json!({"account_name": name, "company": c})).await["id"].clone() }
    };
    let output = account("Output Tax").await;
    let vat = uid(&t
        .insert(
            "TaxTemplate",
            json!({"title": "VAT 10%", "company": c, "kind": "Sales", "rows": [{"charge_type": "On Net Total", "account": output, "description": "VAT", "rate": "10"}]}),
        )
        .await)
    .to_string();
    let receipt = |item: Uuid, qty: &str, rate: &str, serials: Option<&str>| json!({"company": c, "purpose": "Material Receipt", "items": [{"item": item.to_string(), "qty": qty, "basic_rate": rate, "target_warehouse": wh, "serial_no": serials}]});
    let se = t.insert("StockEntry", receipt(widget, "100", "100", None)).await;
    t.submit("StockEntry", uid(&se)).await;
    let se = t.insert("StockEntry", receipt(phone, "3", "500", Some("SN-1\nSN-2\nSN-3"))).await;
    t.submit("StockEntry", uid(&se)).await;

    // ───────────── a delivery returned in part, refused beyond it, cancelled and returned again ─────────────
    let so = t
        .insert("SalesOrder", json!({"company": c, "customer": globex.to_string(), "set_warehouse": wh, "items": [{"item": widget.to_string(), "qty": "10"}]}))
        .await;
    t.submit("SalesOrder", uid(&so)).await;
    let dn = t.action("SalesOrder", uid(&so), "make_delivery_note", json!({})).await;
    let dn_doc = t.submit("DeliveryNote", dn).await;
    assert_eq!(dec(&t.get("SalesOrder", uid(&so)).await["per_delivered"]), n("100"));
    // Valuation moves after the delivery: 10 more come in at 200 (90 @ 100 + 10 @ 200 = 110 each).
    let se = t.insert("StockEntry", receipt(widget, "10", "200", None)).await;
    t.submit("StockEntry", uid(&se)).await;
    assert_eq!(t.stock(widget, stores).await, (n("100"), n("11000")));
    let r1 = t.action("DeliveryNote", dn, "make_sales_return", json!({})).await;
    let r1_doc = t.get("DeliveryNote", r1).await;
    let line = t.line(&r1_doc, 0);
    assert_eq!((r1_doc["is_return"].clone(), r1_doc["return_against"].clone()), (json!(true), json!(dn.to_string())));
    assert_eq!(
        (dec(&line["qty"]), line["return_line"].clone(), line["so_line"].clone()),
        (n("10"), dn_doc["items"][0]["id"].clone(), dn_doc["items"][0]["so_line"].clone())
    );
    t.update("DeliveryNote", r1, json!({"items": [{"id": line["id"], "qty": "4"}]})).await;
    t.submit("DeliveryNote", r1).await;
    assert_eq!(t.stock(widget, stores).await, (n("104"), n("11400")), "4 back at the 100 they left at, not today's 110");
    assert_eq!(t.posted(r1).await, map(&[("Stock In Hand", "400"), ("Cost of Goods Sold", "-400")]));
    assert_eq!(dec(&t.get("DeliveryNote", dn).await["items"][0]["returned_qty"]), n("4"), "the original shows what came back");
    let order = t.get("SalesOrder", uid(&so)).await;
    assert_eq!((dec(&order["per_delivered"]), order["status"].clone()), (n("60"), json!("To Deliver and Bill")));
    let r2 = t.action("DeliveryNote", dn, "make_sales_return", json!({})).await;
    let r2_line = t.line(&t.get("DeliveryNote", r2).await, 0);
    assert_eq!(dec(&r2_line["qty"]), n("6"), "the draft offers what is left");
    t.update("DeliveryNote", r2, json!({"items": [{"id": r2_line["id"], "qty": "7"}]})).await;
    refused(t.k.submit(&t.ctx, "DeliveryNote", r2, None).await, "Widget: 6 of the 10 on DN-");
    refused(t.k.cancel(&t.ctx, "DeliveryNote", dn, None).await, "returns are made against this document");
    t.cancel("DeliveryNote", r1).await;
    assert_eq!(dec(&t.get("DeliveryNote", dn).await["items"][0]["returned_qty"]), Decimal::ZERO, "a cancelled return gives its quantity back");
    assert_eq!(t.stock(widget, stores).await, (n("100"), n("11000")));
    t.submit("DeliveryNote", r2).await;
    let r3 = t.action("DeliveryNote", dn, "make_sales_return", json!({})).await;
    assert_eq!(dec(&t.line(&t.get("DeliveryNote", r3).await, 0)["qty"]), n("3"));
    t.submit("DeliveryNote", r3).await;
    assert_eq!(dec(&t.get("DeliveryNote", dn).await["items"][0]["returned_qty"]), n("10"));
    assert_eq!(t.stock(widget, stores).await, (n("110"), n("12000")), "all ten back at 100");
    refused(t.k.run_action(&t.ctx, "DeliveryNote", dn, "make_sales_return", json!({})).await, "everything on DN-");
    // A return of a return, and a return against a draft or cancelled delivery, are refused.
    refused(t.k.run_action(&t.ctx, "DeliveryNote", r2, "make_sales_return", json!({})).await, "is not available");
    let of_return = t
        .k
        .insert(&t.ctx, "DeliveryNote", obj(json!({"company": c, "customer": globex.to_string(), "is_return": true, "return_against": r2.to_string(), "items": [{"item": widget.to_string(), "qty": "1", "warehouse": wh}]})))
        .await;
    refused(of_return, "itself a return");
    let draft = uid(&t
        .insert("DeliveryNote", json!({"company": c, "customer": globex.to_string(), "items": [{"item": widget.to_string(), "qty": "1", "warehouse": wh}]}))
        .await);
    let against = |orig: Uuid| json!({"company": c, "customer": globex.to_string(), "is_return": true, "return_against": orig.to_string(), "items": [{"item": widget.to_string(), "qty": "1", "warehouse": wh}]});
    refused(t.k.insert(&t.ctx, "DeliveryNote", obj(against(draft))).await, "is a draft");
    t.submit("DeliveryNote", draft).await;
    t.cancel("DeliveryNote", draft).await;
    refused(t.k.insert(&t.ctx, "DeliveryNote", obj(against(draft))).await, "is cancelled");
    let wrong_item = t
        .k
        .insert(&t.ctx, "DeliveryNote", obj(json!({"company": c, "customer": globex.to_string(), "is_return": true, "return_against": dn.to_string(), "items": [{"item": advice.to_string(), "qty": "1", "warehouse": wh}]})))
        .await;
    refused(wrong_item, "Advice is not on DN-");

    // ───────────── serial numbers: only what the delivery took can come back ─────────────
    let sdn = uid(&t
        .insert("DeliveryNote", json!({"company": c, "customer": globex.to_string(), "items": [{"item": phone.to_string(), "qty": "2", "serial_no": "SN-1\nSN-2", "warehouse": wh}]}))
        .await);
    t.submit("DeliveryNote", sdn).await;
    let sret = t.action("DeliveryNote", sdn, "make_sales_return", json!({})).await;
    let sline = t.line(&t.get("DeliveryNote", sret).await, 0);
    t.update("DeliveryNote", sret, json!({"items": [{"id": sline["id"], "qty": "1", "serial_no": "SN-3"}]})).await;
    refused(t.k.submit(&t.ctx, "DeliveryNote", sret, None).await, "serial SN-3 of Phone never left on DN-");
    t.update("DeliveryNote", sret, json!({"items": [{"id": sline["id"], "qty": "1", "serial_no": "SN-2"}]})).await;
    t.submit("DeliveryNote", sret).await;
    assert_eq!(t.stock(phone, stores).await, (n("2"), n("1000")), "SN-2 back at its 500");

    // ───────────── a credit note with goods back, on an invoice whose goods left on a delivery ─────────────
    let dn5 = uid(&t
        .insert(
            "DeliveryNote",
            json!({"company": c, "customer": globex.to_string(), "tax_template": vat, "items": [{"item": widget.to_string(), "qty": "5", "warehouse": wh}]}),
        )
        .await);
    t.submit("DeliveryNote", dn5).await;
    let se = t.insert("StockEntry", receipt(widget, "45", "300", None)).await;
    t.submit("StockEntry", uid(&se)).await; // 105 @ 100 + 45 @ 300 = 150 each now
    let si5 = t.action("DeliveryNote", dn5, "make_sales_invoice", json!({})).await;
    let si5_doc = t.submit("SalesInvoice", si5).await;
    assert_eq!(dec(&si5_doc["grand_total"]), n("825"), "5 × 150 + 10% VAT");
    let owed = t.owes(globex).await;
    let cn = t.action("SalesInvoice", si5, "make_credit_note", json!({})).await;
    let cn_doc = t.get("SalesInvoice", cn).await;
    let cn_line = t.line(&cn_doc, 0);
    assert_eq!((dec(&cn_line["qty"]), cn_doc["tax_template"].clone(), dec(&cn_doc["grand_total"])), (n("-5"), json!(vat), n("-825")));
    t.update("SalesInvoice", cn, json!({"update_stock": true, "items": [{"id": cn_line["id"], "qty": "-2", "warehouse": wh}]})).await;
    let (q0, v0) = t.stock(widget, stores).await;
    let left_at = t.posted(dn5).await["Cost of Goods Sold"] / n("5");
    assert!(v0 / q0 - left_at > n("10"), "the valuation has moved well above the delivery's cost");
    let cost = (left_at * n("2")).round_dp(2);
    let cn_doc = t.submit("SalesInvoice", cn).await;
    assert_eq!(t.stock(widget, stores).await, (q0 + n("2"), v0 + cost), "at the delivery's cost, not today's");
    let mut expected = map(&[("Debtors", "-330"), ("Sales", "300"), ("Output Tax", "30")]);
    expected.insert("Stock In Hand".into(), cost);
    expected.insert("Cost of Goods Sold".into(), -cost);
    assert_eq!(t.posted(cn).await, expected);
    assert_eq!(dec(&cn_doc["outstanding_amount"]), Decimal::ZERO, "it settles the invoice");
    assert_eq!(dec(&t.get("SalesInvoice", si5).await["outstanding_amount"]), n("495"));
    assert_eq!(t.owes(globex).await, owed - n("330"));
    let si5_line = t.line(&t.get("SalesInvoice", si5).await, 0);
    assert_eq!((dec(&si5_line["returned_qty"]), dec(&si5_line["credited_amount"])), (n("2"), n("300")));
    let more = t.action("SalesInvoice", si5, "make_credit_note", json!({})).await;
    let more_line = t.line(&t.get("SalesInvoice", more).await, 0);
    assert_eq!(dec(&more_line["qty"]), n("-3"));
    t.update("SalesInvoice", more, json!({"items": [{"id": more_line["id"], "qty": "-4"}]})).await;
    refused(t.k.submit(&t.ctx, "SalesInvoice", more, None).await, "Widget: 3 of the 5 on SINV-");
    t.k.delete(&t.ctx, "SalesInvoice", more, None).await.unwrap();
    refused(t.k.cancel(&t.ctx, "SalesInvoice", si5, None).await, "returns are made against this document");

    // ───────────── a credit of price alone ─────────────
    let si10 = uid(&t
        .insert(
            "SalesInvoice",
            json!({"company": c, "customer": globex.to_string(), "tax_template": vat, "items": [{"item": advice.to_string(), "qty": "10"}]}),
        )
        .await);
    t.submit("SalesInvoice", si10).await;
    let pc = t.action("SalesInvoice", si10, "make_credit_note", json!({"credit_percent": "10"})).await;
    let pc_doc = t.submit("SalesInvoice", pc).await;
    assert_eq!((pc_doc["price_only"].clone(), dec(&pc_doc["grand_total"])), (json!(true), n("-110")), "10 × 10 off, and its VAT");
    assert_eq!(t.moves(pc).await, Decimal::ZERO, "nothing moves");
    assert_eq!(t.posted(pc).await, map(&[("Debtors", "-110"), ("Sales", "100"), ("Output Tax", "10")]));
    let l = t.line(&t.get("SalesInvoice", si10).await, 0);
    assert_eq!((dec(&l["returned_qty"]), dec(&l["credited_amount"])), (Decimal::ZERO, n("100")));
    let too_much = t.action("SalesInvoice", si10, "make_credit_note", json!({"credit_percent": "95"})).await;
    refused(t.k.submit(&t.ctx, "SalesInvoice", too_much, None).await, "Advice: 900 of the 1000 billed on SINV-");
    t.k.delete(&t.ctx, "SalesInvoice", too_much, None).await.unwrap();
    // The goods back at the full price would give back more than was billed; at the lower price, not.
    let back = t.action("SalesInvoice", si10, "make_credit_note", json!({})).await;
    refused(t.k.submit(&t.ctx, "SalesInvoice", back, None).await, "can still be given back");
    let bl = t.line(&t.get("SalesInvoice", back).await, 0);
    t.update("SalesInvoice", back, json!({"items": [{"id": bl["id"], "discount_percent": "10"}]})).await;
    assert_eq!(dec(&t.submit("SalesInvoice", back).await["grand_total"]), n("-990"));
    assert_eq!(dec(&t.get("SalesInvoice", si10).await["outstanding_amount"]), Decimal::ZERO, "1100 billed, 110 + 990 credited");
    refused(t.k.run_action(&t.ctx, "SalesInvoice", si10, "make_credit_note", json!({"credit_percent": "150"})).await, "at most 100");

    // ───────────── paid, then credited, then refunded ─────────────
    let paid = uid(&t
        .insert("SalesInvoice", json!({"company": c, "customer": globex.to_string(), "tax_template": vat, "items": [{"item": advice.to_string(), "qty": "3"}]}))
        .await);
    t.submit("SalesInvoice", paid).await;
    let pe = t.action("SalesInvoice", paid, "make_payment", json!({})).await;
    t.submit("PaymentEntry", pe).await;
    let owed = t.owes(globex).await;
    let cn3 = t.action("SalesInvoice", paid, "make_credit_note", json!({})).await;
    let cn3_doc = t.submit("SalesInvoice", cn3).await;
    assert_eq!(dec(&cn3_doc["outstanding_amount"]), n("-330"), "the invoice was paid: a credit for the customer");
    assert_eq!(t.owes(globex).await, owed - n("330"));
    let bank_before = t.gl("Primary Bank").await;
    let refund = t.action("SalesInvoice", cn3, "make_refund", json!({})).await;
    let rf = t.get("PaymentEntry", refund).await;
    assert_eq!(
        (rf["payment_type"].clone(), rf["is_refund"].clone(), rf["customer"].clone(), dec(&rf["paid_amount"]), dec(&rf["references"][0]["outstanding"])),
        (json!("Pay"), json!(true), json!(globex.to_string()), n("330"), n("330"))
    );
    // Never more than the credit, and all of it allocated.
    let line = rf["references"][0]["id"].clone();
    let over =
        t.k.update(&t.ctx, "PaymentEntry", refund, obj(json!({"paid_amount": "400", "references": [{"id": line, "allocated_amount": "400"}]})), None).await;
    refused(over, "more than the credit left on SINV-");
    let short = t.k.update(&t.ctx, "PaymentEntry", refund, obj(json!({"paid_amount": "400"})), None).await;
    refused(short, "70 is not allocated");
    let to_invoice =
        t.k.update(&t.ctx, "PaymentEntry", refund, obj(json!({"references": [{"id": line, "invoice": si5.to_string(), "allocated_amount": "330"}]})), None)
            .await;
    refused(to_invoice, "is owed by the party, not a credit to give back");
    t.submit("PaymentEntry", refund).await;
    assert_eq!(t.posted(refund).await, map(&[("Debtors", "330"), ("Primary Bank", "-330")]), "debit receivable, credit bank");
    assert_eq!(t.gl("Primary Bank").await - bank_before, n("-330"));
    assert_eq!(dec(&t.get("SalesInvoice", cn3).await["outstanding_amount"]), Decimal::ZERO);
    assert_eq!(t.owes(globex).await, owed, "a refunded credit is no longer open");
    refused(t.k.run_action(&t.ctx, "SalesInvoice", cn3, "make_refund", json!({})).await, "is not available");
    t.cancel("PaymentEntry", refund).await;
    assert_eq!(dec(&t.get("SalesInvoice", cn3).await["outstanding_amount"]), n("-330"), "cancelling the refund restores the credit");
    // In part: 100 now, the rest later.
    let part = t.action("SalesInvoice", cn3, "make_refund", json!({})).await;
    let pl = t.get("PaymentEntry", part).await["references"][0]["id"].clone();
    t.update("PaymentEntry", part, json!({"paid_amount": "100", "references": [{"id": pl, "allocated_amount": "100"}]})).await;
    t.submit("PaymentEntry", part).await;
    assert_eq!(dec(&t.get("SalesInvoice", cn3).await["outstanding_amount"]), n("-230"));
    let rest = t.action("SalesInvoice", cn3, "make_refund", json!({})).await;
    assert_eq!(dec(&t.get("PaymentEntry", rest).await["paid_amount"]), n("230"));
    t.submit("PaymentEntry", rest).await;
    assert_eq!(t.owes(globex).await, owed);

    // ───────────── the party follows Receive or Pay; a refund is a deliberate tick ─────────────
    let switched = uid(&t
        .insert(
            "PaymentEntry",
            json!({"company": c, "payment_type": "Receive", "customer": globex.to_string(), "bank_account": bank, "currency": inr["id"], "paid_amount": "25"}),
        )
        .await);
    let changed = t.update("PaymentEntry", switched, json!({"payment_type": "Pay", "supplier": acme.to_string()})).await;
    assert_eq!((changed["customer"].clone(), changed["supplier"].clone(), changed["is_refund"].clone()), (Value::Null, json!(acme.to_string()), json!(false)));
    let owe_before = t.owed_supplier(acme).await;
    t.submit("PaymentEntry", switched).await;
    assert_eq!(t.posted(switched).await, map(&[("Creditors", "25"), ("Primary Bank", "-25")]), "the supplier is paid");
    assert_eq!(t.owed_supplier(acme).await, owe_before - n("25"));
    let untick = |payment_type: &str, party: &str, who: Uuid| json!({"company": c, "payment_type": payment_type, party: who.to_string(), "bank_account": bank, "currency": inr["id"], "paid_amount": "10"});
    refused(t.k.insert(&t.ctx, "PaymentEntry", obj(untick("Pay", "customer", globex))).await, "a payment to a customer gives back their credit: tick Refund");
    refused(t.k.insert(&t.ctx, "PaymentEntry", obj(untick("Receive", "supplier", acme))).await, "money from a supplier gives back their credit: tick Refund");

    // ───────────── an overpayment: part applied, the rest given back ─────────────
    let small =
        uid(&t.insert("SalesInvoice", json!({"company": c, "customer": globex.to_string(), "items": [{"item": advice.to_string(), "qty": "2"}]})).await);
    t.submit("SalesInvoice", small).await;
    let owed = t.owes(globex).await;
    let over = uid(&t
        .insert(
            "PaymentEntry",
            json!({"company": c, "payment_type": "Receive", "customer": globex.to_string(), "bank_account": bank, "currency": inr["id"], "paid_amount": "500",
                   "references": [{"invoice": small.to_string(), "allocated_amount": "200"}]}),
        )
        .await);
    t.submit("PaymentEntry", over).await;
    assert_eq!(t.owes(globex).await, owed - n("500"), "300 paid beyond the invoice");
    let back = uid(&t
        .insert("PaymentEntry", json!({"company": c, "payment_type": "Pay", "is_refund": true, "customer": globex.to_string(), "bank_account": bank, "currency": inr["id"], "paid_amount": "0"}))
        .await);
    let offered = t.k.run_action(&t.ctx, "PaymentEntry", back, "get_outstanding_invoices", json!({})).await.unwrap();
    let offer: Vec<(String, Decimal)> =
        offered["references"].as_array().unwrap().iter().map(|r| (r["invoice"].as_str().unwrap().to_string(), dec(&r["allocated_amount"]))).collect();
    assert_eq!(offer, [(over.to_string(), n("300"))], "the customer's credit is offered, as the credit left");
    assert_eq!(dec(&offered["paid_amount"]), n("300"));
    t.submit("PaymentEntry", back).await;
    assert_eq!(t.posted(back).await, map(&[("Debtors", "300"), ("Primary Bank", "-300")]));
    assert_eq!(t.owes(globex).await, owed - n("200"));
    refused(t.k.cancel(&t.ctx, "PaymentEntry", over, None).await, "payments are allocated to this document");

    // ───────────── foreign currency: credited at the invoice's rate, refunded at today's ─────────────
    t.insert("ExchangeRate", json!({"from_currency": usd["id"], "to_currency": inr["id"], "rate": "84", "valid_from": "2000-01-01"})).await;
    let abroad = uid(&t.insert("Customer", json!({"customer_name": "Overseas", "default_currency": usd["id"]})).await);
    let export = uid(&t
        .insert(
            "SalesInvoice",
            json!({"company": c, "customer": abroad.to_string(), "conversion_rate": "83", "items": [{"item": export_item.to_string(), "qty": "1"}]}),
        )
        .await);
    let export_doc = t.submit("SalesInvoice", export).await;
    assert_eq!((dec(&export_doc["conversion_rate"]), dec(&export_doc["base_grand_total"])), (n("83"), n("12450")));
    let usd_paid = t.action("SalesInvoice", export, "make_payment", json!({})).await;
    t.update("PaymentEntry", usd_paid, json!({"exchange_rate": "83"})).await;
    t.submit("PaymentEntry", usd_paid).await;
    let fx_cn = t.action("SalesInvoice", export, "make_credit_note", json!({})).await;
    let fx_cn_doc = t.submit("SalesInvoice", fx_cn).await;
    assert_eq!((dec(&fx_cn_doc["conversion_rate"]), dec(&fx_cn_doc["base_grand_total"])), (n("83"), n("-12450")), "the invoice's rate");
    let loss_before = t.gl("Exchange Gain or Loss").await;
    let fx_refund = t.action("SalesInvoice", fx_cn, "make_refund", json!({})).await;
    let fx_doc = t.submit("PaymentEntry", fx_refund).await;
    assert_eq!((dec(&fx_doc["exchange_rate"]), dec(&fx_doc["paid_amount"])), (n("84"), n("150")));
    assert_eq!(
        t.posted(fx_refund).await,
        map(&[("Debtors", "12450"), ("Primary Bank", "-12600"), ("Exchange Gain or Loss", "150")]),
        "150 dollars cost 12,600 rupees today: a loss of 150"
    );
    assert_eq!(t.gl("Exchange Gain or Loss").await - loss_before, n("150"));
    assert_eq!(t.owes(abroad).await, Decimal::ZERO);

    // ───────────── a full credit note mirrors its invoice to the cent ─────────────
    let input = account("Input Tax").await;
    let freight = account("Other Income").await;
    let inclusive = uid(&t
        .insert(
            "TaxTemplate",
            json!({"title": "Inclusive", "company": c, "kind": "Sales", "rows": [
                {"charge_type": "On Net Total", "account": output, "description": "VAT", "rate": "12.5", "included_in_price": true},
                {"charge_type": "On Previous Row Total", "account": input, "description": "Cess", "rate": "2", "row_ref": 1, "included_in_price": true}]}),
        )
        .await)
    .to_string();
    let compound = uid(&t
        .insert(
            "TaxTemplate",
            json!({"title": "Compound", "company": c, "kind": "Sales", "rows": [
                {"charge_type": "On Net Total", "account": output, "description": "VAT", "rate": "7.25"},
                {"charge_type": "On Previous Row Total", "account": input, "description": "Surcharge", "rate": "1.5", "row_ref": 1},
                {"charge_type": "Actual", "account": freight, "description": "Freight", "amount": "25.55"}]}),
        )
        .await)
    .to_string();
    for (rounding, step) in [("Per Document", Value::Null), ("Per Line", json!("0.05"))] {
        t.update("Company", cid, json!({"tax_rounding": rounding, "round_total_to": step})).await;
        for template in [&inclusive, &compound] {
            let si = uid(&t
                .insert(
                    "SalesInvoice",
                    json!({"company": c, "customer": abroad.to_string(), "conversion_rate": "83.17", "tax_template": template, "discount_percent": "3.3", "items": [
                        {"item": odd[0], "qty": "3", "discount_percent": "7.5"},
                        {"item": odd[1], "qty": "7"},
                        {"item": odd[2], "qty": "1"}]}),
                )
                .await);
            let si_doc = t.submit("SalesInvoice", si).await;
            let cn = t.action("SalesInvoice", si, "make_credit_note", json!({})).await;
            let cn_doc = t.submit("SalesInvoice", cn).await;
            for f in
                ["net_total", "total_taxes", "grand_total", "rounding_adjustment", "rounded_total", "base_net_total", "base_grand_total", "base_rounded_total"]
            {
                assert_eq!(dec(&cn_doc[f]), -dec(&si_doc[f]), "{rounding} {f}");
            }
            for (a, b) in si_doc["taxes"].as_array().unwrap().iter().zip(cn_doc["taxes"].as_array().unwrap()) {
                assert_eq!(dec(&b["base_tax_amount"]), -dec(&a["base_tax_amount"]), "{rounding}: {}", a["description"]);
            }
            let (orig, credit) = (t.posted(si).await, t.posted(cn).await);
            assert_eq!(credit, orig.iter().map(|(a, v)| (a.clone(), -*v)).collect::<BTreeMap<_, _>>(), "{rounding}: the ledger mirrors");
            assert_eq!(dec(&t.get("SalesInvoice", si).await["outstanding_amount"]), Decimal::ZERO);
        }
    }
    t.update("Company", cid, json!({"tax_rounding": "Per Document", "round_total_to": null})).await;

    // ───────────── a credit note without its original: with a reason, and stock only when asked ─────────────
    let owed = t.owes(globex).await;
    let goodwill = uid(&t
        .insert("SalesInvoice", json!({"company": c, "customer": globex.to_string(), "is_return": true, "items": [{"item": advice.to_string(), "qty": "-2"}]}))
        .await);
    refused(t.k.submit(&t.ctx, "SalesInvoice", goodwill, None).await, "needs a reason");
    t.update("SalesInvoice", goodwill, json!({"return_reason": "Goodwill: late delivery"})).await;
    t.submit("SalesInvoice", goodwill).await;
    assert_eq!(t.posted(goodwill).await, map(&[("Debtors", "-200"), ("Sales", "200")]), "taxes as entered: none");
    assert_eq!(t.owes(globex).await, owed - n("200"));
    let opening = uid(&t
        .insert(
            "SalesInvoice",
            json!({"company": c, "customer": globex.to_string(), "is_return": true, "return_reason": "Returned before these books", "update_stock": true,
                   "items": [{"item": widget.to_string(), "qty": "-3", "warehouse": wh, "incoming_rate": "80"}]}),
        )
        .await);
    let (q0, v0) = t.stock(widget, stores).await;
    t.submit("SalesInvoice", opening).await;
    assert_eq!(t.stock(widget, stores).await, (q0 + n("3"), v0 + n("240")), "at the cost stated");
    assert_eq!(t.posted(opening).await, map(&[("Debtors", "-450"), ("Sales", "450"), ("Stock In Hand", "240"), ("Cost of Goods Sold", "-240")]));

    // ───────────── a debit note to a customer ─────────────
    let base = uid(&t
        .insert(
            "SalesInvoice",
            json!({"company": c, "customer": globex.to_string(), "tax_template": vat, "items": [{"item": advice.to_string(), "qty": "10"}]}),
        )
        .await);
    t.submit("SalesInvoice", base).await;
    let owed = t.owes(globex).await;
    let dnote = t.action("SalesInvoice", base, "make_debit_note", json!({"charge_percent": "5"})).await;
    let dnote_doc = t.get("SalesInvoice", dnote).await;
    assert_eq!(
        (dnote_doc["is_debit_note"].clone(), dnote_doc["debit_note_against"].clone(), dnote_doc["is_return"].clone()),
        (json!(true), json!(base.to_string()), json!(false))
    );
    let dnote_doc = t.submit("SalesInvoice", dnote).await;
    assert_eq!((dec(&dnote_doc["grand_total"]), dnote_doc["status"].clone()), (n("55"), json!("Unpaid")), "5% more on 1,000, and its VAT");
    assert_eq!(t.posted(dnote).await, map(&[("Debtors", "55"), ("Sales", "-50"), ("Output Tax", "-5")]));
    assert_eq!(t.owes(globex).await, owed + n("55"));
    assert_eq!(dec(&t.get("SalesInvoice", base).await["debit_noted"]), n("55"), "shown on the original");
    refused(t.k.cancel(&t.ctx, "SalesInvoice", base, None).await, "debit notes are made against this invoice");
    refused(t.k.run_action(&t.ctx, "SalesInvoice", cn3, "make_debit_note", json!({})).await, "is not available");
    t.cancel("SalesInvoice", dnote).await;
    assert_eq!(dec(&t.get("SalesInvoice", base).await["debit_noted"]), Decimal::ZERO);
    // A debit note is an invoice: it counts towards the credit limit.
    t.update("Customer", globex, json!({"credit_limit": "1"})).await;
    let capped = t.action("SalesInvoice", base, "make_debit_note", json!({"charge_percent": "5"})).await;
    refused(t.k.submit(&t.ctx, "SalesInvoice", capped, None).await, "credit limit exceeded");
    t.update("Customer", globex, json!({"credit_limit": "0"})).await;

    // ───────────── buying: a receipt returned at its own cost, a debit note against a receipt's bill ─────────────
    let po = uid(&t.insert("PurchaseOrder", json!({"company": c, "supplier": acme.to_string(), "set_warehouse": wh, "items": [{"item": steel.to_string(), "qty": "10", "price_list_rate": "10"}]})).await);
    t.submit("PurchaseOrder", po).await;
    let pr = t.action("PurchaseOrder", po, "make_purchase_receipt", json!({})).await;
    t.submit("PurchaseReceipt", pr).await;
    let pr2 = uid(&t
        .insert(
            "PurchaseReceipt",
            json!({"company": c, "supplier": acme.to_string(), "items": [{"item": steel.to_string(), "qty": "10", "price_list_rate": "20", "warehouse": wh}]}),
        )
        .await);
    t.submit("PurchaseReceipt", pr2).await;
    assert_eq!(t.stock(steel, stores).await, (n("20"), n("300")), "15 each on average");
    let prr = t.action("PurchaseReceipt", pr, "make_purchase_return", json!({})).await;
    let prl = t.line(&t.get("PurchaseReceipt", prr).await, 0);
    assert_eq!((dec(&prl["qty"]), prl["po_line"].is_string()), (n("10"), true));
    t.update("PurchaseReceipt", prr, json!({"items": [{"id": prl["id"], "qty": "4"}]})).await;
    t.submit("PurchaseReceipt", prr).await;
    assert_eq!(t.stock(steel, stores).await, (n("16"), n("260")), "4 go back at the 10 they came in at");
    assert_eq!(t.posted(prr).await, map(&[("Stock In Hand", "-40"), ("Stock Received But Not Billed", "40")]));
    assert_eq!(dec(&t.get("PurchaseOrder", po).await["per_received"]), n("60"));
    let pi = t.action("PurchaseReceipt", pr2, "make_purchase_invoice", json!({})).await;
    t.update("PurchaseInvoice", pi, json!({"bill_no": "ACME/1"})).await;
    t.submit("PurchaseInvoice", pi).await;
    let owe_acme = t.owed_supplier(acme).await;
    let debit = t.action("PurchaseInvoice", pi, "make_debit_note", json!({})).await;
    let dl = t.line(&t.get("PurchaseInvoice", debit).await, 0);
    t.update("PurchaseInvoice", debit, json!({"bill_no": "ACME/1-DN", "update_stock": true, "items": [{"id": dl["id"], "qty": "-2", "warehouse": wh}]})).await;
    t.submit("PurchaseInvoice", debit).await;
    assert_eq!(t.stock(steel, stores).await, (n("14"), n("220")), "2 go back at the receipt's 20, not the average of 16.25");
    assert_eq!(t.posted(debit).await, map(&[("Creditors", "40"), ("Stock In Hand", "-40")]));
    assert_eq!(t.owed_supplier(acme).await, owe_acme - n("40"));
    // A lower price agreed afterwards on the 8 kept: a price variance, no goods move.
    let lower = t.action("PurchaseInvoice", pi, "make_debit_note", json!({"credit_percent": "10"})).await;
    t.update("PurchaseInvoice", lower, json!({"bill_no": "ACME/1-PRICE"})).await;
    let lower_doc = t.submit("PurchaseInvoice", lower).await;
    assert_eq!((dec(&lower_doc["grand_total"]), t.moves(lower).await), (n("-16"), Decimal::ZERO));
    assert_eq!(t.posted(lower).await, map(&[("Creditors", "16"), ("Stock Adjustment", "-16")]));
    // A debit note without its original needs a reason; then it is the supplier's to pay back.
    let rebate = uid(&t
        .insert("PurchaseInvoice", json!({"company": c, "supplier": acme.to_string(), "bill_no": "ACME/REBATE", "is_return": true, "items": [{"item": advice.to_string(), "qty": "-1", "price_list_rate": "60"}]}))
        .await);
    refused(t.k.submit(&t.ctx, "PurchaseInvoice", rebate, None).await, "needs a reason");
    t.update("PurchaseInvoice", rebate, json!({"return_reason": "Volume rebate"})).await;
    t.submit("PurchaseInvoice", rebate).await;
    let paid_in = t.action("PurchaseInvoice", rebate, "make_refund", json!({})).await;
    let pin = t.get("PaymentEntry", paid_in).await;
    assert_eq!(
        (pin["payment_type"].clone(), pin["is_refund"].clone(), pin["supplier"].is_string(), dec(&pin["paid_amount"])),
        (json!("Receive"), json!(true), true, n("60"))
    );
    t.submit("PaymentEntry", paid_in).await;
    assert_eq!(t.posted(paid_in).await, map(&[("Creditors", "-60"), ("Primary Bank", "60")]), "the supplier pays back: debit bank, credit payable");
    assert_eq!(dec(&t.get("PurchaseInvoice", rebate).await["outstanding_amount"]), Decimal::ZERO);

    // ───────────── what the books report with returns and refunds in them ─────────────
    let ar = ferrum_domain::reports::run(&t.k, &t.ctx, "receivables", &[("company".to_string(), c.clone())].into_iter().collect()).await.unwrap();
    let open_base = t
        .scalar("select sum(base_outstanding) from open_item where tenant_id = $1 and company_id = $2 and customer_id is not null and status = 'open'", cid)
        .await;
    assert_eq!(dec(&ar["totals"]["outstanding_base"]), open_base, "ageing agrees with the open items, refunded credits gone");
    assert_eq!(
        t.gl("Debtors").await,
        t.scalar("select sum(base_outstanding) from open_item where tenant_id = $1 and company_id = $2 and customer_id is not null", cid).await
    );

    // ───────────── a closed period: returns and refunds dated in it are refused ─────────────
    let today = chrono::Utc::now().date_naive();
    let month = (today - chrono::Duration::days(62)).with_day(1).unwrap();
    let year = month.year();
    let covered =
        t.k.list(&t.ctx, "FiscalYear", &ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"company": c})).unwrap(), ..Default::default() })
            .await
            .unwrap()
            .rows
            .into_iter()
            .find(|f| f["start_date"].as_str().unwrap() <= month.to_string().as_str() && f["end_date"].as_str().unwrap() >= month.to_string().as_str());
    let fy = match covered {
        Some(f) => uid(&f),
        None => uid(&t
            .insert(
                "FiscalYear",
                json!({"year_name": format!("FY {year}"), "company": c, "start_date": format!("{year}-01-01"), "end_date": format!("{year}-12-31")}),
            )
            .await),
    };
    let day = (month + chrono::Duration::days(4)).to_string();
    let old = uid(&t
        .insert("SalesInvoice", json!({"company": c, "customer": globex.to_string(), "posting_date": day, "items": [{"item": advice.to_string(), "qty": "1"}]}))
        .await);
    t.submit("SalesInvoice", old).await;
    let pay = t.action("SalesInvoice", old, "make_payment", json!({})).await;
    t.submit("PaymentEntry", pay).await;
    t.k.run_action(&t.ctx, "FiscalYear", fy, "close_period", json!({"period_start": month.to_string()})).await.expect("close");
    let late = t.action("SalesInvoice", old, "make_credit_note", json!({})).await;
    t.update("SalesInvoice", late, json!({"posting_date": day})).await;
    refused(t.k.submit(&t.ctx, "SalesInvoice", late, None).await, "closed");
    t.update("SalesInvoice", late, json!({"posting_date": today.to_string()})).await;
    t.submit("SalesInvoice", late).await;
    let late_refund = t.action("SalesInvoice", late, "make_refund", json!({})).await;
    t.update("PaymentEntry", late_refund, json!({"posting_date": day})).await;
    refused(t.k.submit(&t.ctx, "PaymentEntry", late_refund, None).await, "closed");
    t.k.run_action(&t.ctx, "FiscalYear", fy, "reopen_period", json!({"period_start": month.to_string()})).await.expect("reopen");
    t.submit("PaymentEntry", late_refund).await;

    // ───────────── returns made before 0.15.0: the migration counts them ─────────────
    let counted = "select coalesce(sum(returned_qty), 0) + coalesce(sum(credited_amount), 0) from ( \
         select id, returned_qty, null::numeric as credited_amount from e_delivery_note__items where tenant_id = $1 and $2 = $2 union all \
         select id, returned_qty, credited_amount from e_sales_invoice__items where tenant_id = $1 union all \
         select id, returned_qty, null from e_purchase_receipt__items where tenant_id = $1 union all \
         select id, returned_qty, credited_amount from e_purchase_invoice__items where tenant_id = $1) x";
    let before = t.scalar(counted, cid).await;
    assert!(before > Decimal::ZERO);
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    // As 0.14 left them: no line of the original named, nothing kept on the originals.
    for table in ["e_delivery_note__items", "e_sales_invoice__items", "e_purchase_receipt__items", "e_purchase_invoice__items"] {
        let blank = format!("update {table} set return_line = null where tenant_id = $1 and return_line is not null");
        sqlx::query(sqlx::AssertSqlSafe(blank)).bind(tenant).execute(&mut *tx).await.unwrap();
        let zero = if table.ends_with("invoice__items") { ", credited_amount = 0" } else { "" };
        let reset = format!("update {table} set returned_qty = 0{zero} where tenant_id = $1");
        sqlx::query(sqlx::AssertSqlSafe(reset)).bind(tenant).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    assert_eq!(t.scalar(counted, cid).await, Decimal::ZERO);
    // Uncounted, the delivery would offer its ten again.
    let again = t.action("DeliveryNote", dn, "make_sales_return", json!({})).await;
    assert_eq!(dec(&t.line(&t.get("DeliveryNote", again).await, 0)["qty"]), n("10"));
    t.k.delete(&t.ctx, "DeliveryNote", again, None).await.unwrap();
    let mut tx = t.k.db.tenant_tx(tenant).await.unwrap();
    sqlx::raw_sql(include_str!("../packages/selling/sql/001_returns.sql")).execute(&mut *tx).await.expect("selling's migration");
    sqlx::raw_sql(include_str!("../packages/buying/sql/001_returns.sql")).execute(&mut *tx).await.expect("buying's migration");
    tx.commit().await.unwrap();
    assert_eq!(t.scalar(counted, cid).await, before, "every return counted again, as the engine counts them");
    assert_eq!(dec(&t.get("DeliveryNote", dn).await["items"][0]["returned_qty"]), n("10"));
    refused(t.k.run_action(&t.ctx, "DeliveryNote", dn, "make_sales_return", json!({})).await, "everything on DN-");
}
