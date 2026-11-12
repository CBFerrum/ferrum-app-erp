//! Tax returns as definitions (FN-38, FN-40) against a real PostgreSQL (`FERRUM_ERP_RETURN_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_return_test`; reset first): a made-up
//! definition's boxes summed by direction, rate, place and document kind, a formula over them and a
//! box entered by hand; adjustments need a reason; filing keeps the values and closes the period to
//! invoices; a box drills to its documents; a period is filed once.

mod common;
use chrono::{Datelike, NaiveDate};
use common::*;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;

#[test]
fn tax_returns() {
    run(scenario());
}

fn boxes(r: &ferrum_kernel::Doc) -> HashMap<String, Decimal> {
    r["boxes"].as_array().unwrap().iter().map(|b| (b["code"].as_str().unwrap().to_string(), dec(&b["value"]))).collect()
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_RETURN_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_return_test".into());
    let t = T::boot(url, |_, _| {}).await;
    let c = t.company.clone();
    let output = t.account("Output Tax").await;
    let input = t.account("Input Tax").await;
    let template = |title: &str, kind: &str, account: &str, rate: &str| json!({"title": title, "company": c, "kind": kind, "rows": [{"charge_type": "On Net Total", "account": account, "description": title, "rate": rate}]});
    let full = uid(&t.insert("TaxTemplate", template("VAT 10%", "Sales", &output, "10")).await).to_string();
    let reduced = uid(&t.insert("TaxTemplate", template("VAT 5%", "Sales", &output, "5")).await).to_string();
    let bought = uid(&t.insert("TaxTemplate", template("Input VAT 10%", "Purchase", &input, "10")).await).to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let advice = uid(&t
        .insert("Item", json!({"item_code": "Advice", "item_name": "Advice", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "100", "is_stock_item": false}))
        .await)
    .to_string();
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await).to_string();
    let sale = |tpl: &str, qty: &str| json!({"company": c, "customer": globex, "tax_template": tpl, "items": [{"item": advice, "qty": qty}]});
    let si1 = uid(&t.post("SalesInvoice", sale(&full, "2")).await);
    let mut abroad = sale(&reduced, "1");
    abroad.as_object_mut().unwrap().insert("delivery_country".into(), json!("DE"));
    t.post("SalesInvoice", abroad).await;
    let mut credit = sale(&full, "-1");
    credit.as_object_mut().unwrap().extend(obj(json!({"is_return": true, "return_against": si1.to_string()})));
    let cn = uid(&t.post("SalesInvoice", credit).await);
    t.post(
        "PurchaseInvoice",
        json!({"company": c, "supplier": acme, "bill_no": "A-1", "tax_template": bought, "items": [{"item": advice, "qty": "1", "price_list_rate": "50"}]}),
    )
    .await;

    let def = uid(&t
        .insert(
            "TaxReturnDefinition",
            json!({"definition_name": "Made-up Return", "frequency": "Monthly", "boxes": [
                {"code": "OUT", "label": "Output tax", "kind": "Sum", "measure": "Tax", "direction": "Output"},
                {"code": "OUT10", "label": "Output tax at 10%", "kind": "Sum", "direction": "Output", "rate": "10"},
                {"code": "ABROAD", "label": "Sales delivered to DE", "kind": "Sum", "measure": "Base", "direction": "Output", "place": "DE"},
                {"code": "HOME", "label": "Sales delivered elsewhere", "kind": "Sum", "measure": "Base", "direction": "Output", "place": "!DE"},
                {"code": "CN", "label": "Tax on credit notes", "kind": "Sum", "direction": "Output", "document": "Credit Note"},
                {"code": "IN", "label": "Input tax", "kind": "Sum", "direction": "Input", "tax": "Input Tax"},
                {"code": "MAN", "label": "Entered by hand", "kind": "By Hand"},
                {"code": "NET", "label": "Payable", "kind": "Formula", "formula": "max(OUT - IN + MAN, 0)"}]}),
        )
        .await)
    .to_string();
    let today = t.ctx.today();
    let first = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap();
    let r = t.insert("TaxReturn", json!({"definition": def, "company": c, "from_date": first.to_string()})).await;
    let rid = uid(&r);
    assert_eq!(r["to_date"], json!(first.checked_add_months(chrono::Months::new(1)).unwrap().pred_opt().unwrap().to_string()));
    let v = boxes(&r);
    let n = |x: i64| Decimal::from(x);
    assert_eq!(
        (v["OUT"], v["OUT10"], v["ABROAD"], v["HOME"], v["CN"], v["IN"], v["MAN"], v["NET"]),
        (n(15), n(10), n(100), n(100), n(-10), n(5), n(0), n(10)),
        "20 + 5 - 10 out, 5 in; 100 abroad, 200 - 100 at home; the formula over them"
    );

    // Adjusted by hand, with a reason.
    let mut rows = r["boxes"].as_array().unwrap().clone();
    rows[6]["adjustment"] = json!("3");
    let refused = t.k.update(&t.ctx, "TaxReturn", rid, obj(json!({"boxes": rows.clone()})), None).await.unwrap_err();
    assert!(format!("{refused:?}").contains("say why"), "{refused:?}");
    rows[6]["adjustment_reason"] = json!("Margin scheme sales");
    let r = t.k.update(&t.ctx, "TaxReturn", rid, obj(json!({"boxes": rows})), None).await.expect("adjusted");
    assert_eq!((boxes(&r)["MAN"], boxes(&r)["NET"]), (n(3), n(13)));

    // A box drills to its documents.
    let drill = t.report("tax_return_documents", &[("tax_return", &rid.to_string()), ("box", "OUT10")]).await;
    let docs: Vec<String> = drill["rows"].as_array().unwrap().iter().map(|r| r["voucher_id"].as_str().unwrap().to_string()).collect();
    assert_eq!(docs, vec![si1.to_string(), cn.to_string()]);
    assert_eq!(dec(&drill["totals"]["counted"]), n(10));

    // Filed: the values are kept and the period is closed to invoices.
    let filed = t.submit("TaxReturn", rid).await;
    assert_eq!((boxes(&filed)["NET"], filed["filed_on"].clone()), (n(13), json!(today.to_string())));
    let late = t.try_post("SalesInvoice", sale(&full, "1")).await.unwrap_err();
    assert!(late.contains("is filed for"), "{late}");
    let cancel = t.k.cancel(&t.ctx, "SalesInvoice", cn, None).await.unwrap_err();
    assert!(format!("{cancel:?}").contains("is filed for"), "{cancel:?}");
    let again = t.try_post("TaxReturn", json!({"definition": def, "company": c, "from_date": first.to_string()})).await.unwrap_err();
    assert!(again.contains("is filed for this period"), "{again}");
    assert_eq!(boxes(&t.get("TaxReturn", rid).await)["NET"], n(13), "kept as filed");
}
