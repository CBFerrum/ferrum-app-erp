//! A numbering series chosen per document (`FERRUM_ERP_SERIES_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_series_test`; reset first): without a series a document is
//! numbered as before; one that names a series of its kind takes its template; a series of another kind, another
//! company's or a switched-off one is refused; a series flagged as the default takes the documents that name none
//! (one per kind and company); what the localization seam answers by name is taken in between; and a gapless series
//! stays gapless when many documents are submitted at once and some are refused after their number was taken.

mod common;
use async_trait::async_trait;
use common::*;
use ferrum_app_erp::localization::Localization;
use ferrum_kernel::{Doc, HookCtx};
use serde_json::{Value, json};
use std::sync::Arc;

/// A country's rule, for the test: a receipt to a consumer is numbered in the simplified series.
struct Receipts;

#[async_trait]
impl Localization for Receipts {
    fn package(&self) -> &'static str {
        "selling"
    }
    async fn number_series(&self, _hc: &mut HookCtx<'_>, entity: &str, doc: &Doc) -> ferrum_kernel::Result<Option<String>> {
        Ok((entity == "SalesInvoice" && doc.get("po_no").and_then(Value::as_str) == Some("RECEIPT")).then(|| "Simplified".to_string()))
    }
}

#[test]
fn numbering_series() {
    run(scenario());
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_SERIES_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_series_test".into());
    ferrum_app_erp::localization::add(Arc::new(Receipts));
    let t = Arc::new(T::boot(url, |_, _| {}).await);
    let c = t.company.clone();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = uid(&t
        .insert("Item", json!({"item_code": "Pen", "item_name": "Pen", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "10", "is_stock_item": false}))
        .await)
    .to_string();
    let acme = uid(&t.insert("Customer", json!({"customer_name": "Acme"})).await).to_string();
    let held = uid(&t.insert("Customer", json!({"customer_name": "Held", "credit_hold": true, "credit_hold_reason": "overdue"})).await).to_string();
    let invoice = |customer: &str, extra: Value| {
        let mut v = json!({"company": c, "customer": customer, "items": [{"item": item, "qty": "1"}]});
        v.as_object_mut().unwrap().extend(obj(extra));
        v
    };
    let number = |d: &Doc| d["number"].as_str().unwrap().to_string();
    let series = |name: &str, document: &str, template: &str, extra: Value| {
        let mut v = json!({"series_name": name, "document": document, "template": template});
        v.as_object_mut().unwrap().extend(obj(extra));
        v
    };

    // Nothing set up: the usual number.
    let usual = t.post("SalesInvoice", invoice(&acme, json!({}))).await;
    assert!(number(&usual).starts_with("SINV-") && number(&usual).ends_with("-00001"), "{}", number(&usual));

    // A template that cannot number is refused when the series is saved.
    assert!(t.k.insert(&t.ctx, "NumberSeries", obj(series("Bad", "SalesInvoice", "NO-NUMBER-HERE", json!({})))).await.is_err());
    let tax = uid(&t.insert("NumberSeries", series("Tax Invoice", "SalesInvoice", "TINV-{seq:4}", json!({"gapless": true}))).await).to_string();
    let simplified = uid(&t.insert("NumberSeries", series("Simplified", "SalesInvoice", "SIMP-{seq:4}", json!({}))).await).to_string();
    let order_series = uid(&t.insert("NumberSeries", series("Orders", "SalesOrder", "ORD-{seq:3}", json!({"use_by_default": true}))).await).to_string();
    // One default per kind and company.
    let second = t.k.insert(&t.ctx, "NumberSeries", obj(series("Orders 2", "SalesOrder", "ORX-{seq:3}", json!({"use_by_default": true})))).await;
    assert!(format!("{second:?}").contains("already the one used"), "{second:?}");

    // Named: that series' number; the usual series goes on from where it was.
    let a = t.post("SalesInvoice", invoice(&acme, json!({"number_series": tax}))).await;
    assert_eq!(number(&a), "TINV-0001");
    let b = t.post("SalesInvoice", invoice(&acme, json!({}))).await;
    assert!(number(&b).ends_with("-00002"), "{}", number(&b));
    // The seam answers a name when the document names none; a document that names one keeps its own.
    let r = t.post("SalesInvoice", invoice(&acme, json!({"po_no": "RECEIPT"}))).await;
    assert_eq!(number(&r), "SIMP-0001");
    let both = t.post("SalesInvoice", invoice(&acme, json!({"po_no": "RECEIPT", "number_series": tax}))).await;
    assert_eq!(number(&both), "TINV-0002");
    // The default of a kind takes the documents that name none.
    let order = t.post("SalesOrder", json!({"company": c, "customer": acme, "items": [{"item": item, "qty": "1"}]})).await;
    assert_eq!(number(&order), "ORD-001");

    // Refused at creation: a series of another kind, a switched-off one, another company's.
    let wrong = t.k.insert(&t.ctx, "SalesInvoice", obj(invoice(&acme, json!({"number_series": order_series})))).await;
    assert!(format!("{wrong:?}").contains("is a series for SalesOrder"), "{wrong:?}");
    let off = uid(&t.insert("NumberSeries", series("Retired", "SalesInvoice", "OLD-{seq:3}", json!({"disabled": true}))).await).to_string();
    assert!(format!("{:?}", t.k.insert(&t.ctx, "SalesInvoice", obj(invoice(&acme, json!({"number_series": off})))).await).contains("switched off"));
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let other = uid(&t.insert("Company", json!({"company_name": "Other Co", "abbr": "OC", "default_currency": inr["id"]})).await).to_string();
    let theirs = uid(&t.insert("NumberSeries", series("Theirs", "SalesInvoice", "OTH-{seq:3}", json!({"company": other}))).await).to_string();
    assert!(format!("{:?}", t.k.insert(&t.ctx, "SalesInvoice", obj(invoice(&acme, json!({"number_series": theirs})))).await).contains("another company"));
    let _ = simplified;

    // Gapless under concurrency: nine orders submitted at once, three more refused after they took a number (the
    // customer is on credit hold, which orders honour): the nine are 1 to 9 with nothing missing and nothing twice.
    let gapless = uid(&t.insert("NumberSeries", series("Gapless Orders", "SalesOrder", "GLO-{seq:4}", json!({"gapless": true}))).await).to_string();
    let mut drafts = Vec::new();
    for i in 0..12 {
        let customer = if i % 4 == 3 { &held } else { &acme };
        let body = json!({"company": c, "customer": customer, "number_series": gapless, "items": [{"item": item, "qty": "1"}]});
        drafts.push((customer == &held, uid(&t.insert("SalesOrder", body).await)));
    }
    let tasks: Vec<_> = drafts
        .into_iter()
        .map(|(is_held, id)| {
            let t = t.clone();
            tokio::spawn(async move { (is_held, t.k.submit(&t.ctx, "SalesOrder", id, None).await) })
        })
        .collect();
    let mut taken = Vec::new();
    for task in tasks {
        let (is_held, r) = task.await.unwrap();
        assert_eq!(r.is_err(), is_held, "{r:?}");
        if let Ok(d) = r {
            taken.push(number(&d));
        }
    }
    taken.sort();
    assert_eq!(taken, (1..=9).map(|n| format!("GLO-{n:04}")).collect::<Vec<_>>());
    // And the next one takes the next number, as if the refused ones never were.
    let next = t.post("SalesOrder", json!({"company": c, "customer": acme, "number_series": gapless, "items": [{"item": item, "qty": "1"}]})).await;
    assert_eq!(number(&next), "GLO-0010");
}
