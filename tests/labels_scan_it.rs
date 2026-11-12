//! Labels and scanning, against a real PostgreSQL (`FERRUM_ERP_SCAN_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_scan_test`; reset first): item, batch, parcel and serial
//! labels print for many records in one call, with barcodes; the company's `scan` says what a scanned text is
//! (an item by barcode or code, a batch, a serial, a document number, a GS1 element string with its item,
//! batch, expiry and quantity), with the unit; two items under one text are both answered as ambiguous, and an
//! unknown text answers nothing.

mod common;
use common::*;
use ferrum_kernel::capability::Invocation;
use serde_json::{Value, json};

#[test]
fn labels_and_scans() {
    run(scenario());
}

async fn scenario() {
    let url = std::env::var("FERRUM_ERP_SCAN_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_scan_test".into());
    let t = T::boot(url, |packages, hooks| {
        packages.extend(ferrum_stock_ops::packages().expect("stock operations"));
        ferrum_stock_ops::register(hooks);
    })
    .await;
    let c = t.company.clone();
    let wh = t.find_one("Warehouse", json!({"warehouse_name": "Stores", "company": c})).await["id"].as_str().unwrap().to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |code: &str, barcode: Option<&str>, serial: bool| {
        json!({"item_code": code, "item_name": format!("{code} item"), "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "10",
               "barcode": barcode, "has_serial_no": serial})
    };
    let tea = uid(&t.insert("Item", item("TEA", Some("4006381333931"), false)).await).to_string();
    let cup = uid(&t.insert("Item", item("CUP", Some("12345670"), false)).await).to_string();
    let phone = uid(&t.insert("Item", item("PHONE", None, true)).await).to_string();
    // Another item whose code is the first's barcode: one text, two items.
    let clash = uid(&t.insert("Item", item("4006381333931", None, false)).await).to_string();
    let batch = t.insert("Batch", json!({"batch_id": "LOT-7", "item": tea, "expiry_date": "2027-03-31"})).await;
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await).to_string();
    let pr = t
        .post(
            "PurchaseReceipt",
            json!({"company": c, "supplier": acme, "set_warehouse": wh, "items": [
                {"item": phone, "qty": "2", "price_list_rate": "300", "serial_no": "SN-1\nSN-2"},
                {"item": tea, "qty": "5", "price_list_rate": "4", "batch_no": "LOT-7"}]}),
        )
        .await;

    // Labels for many records in one call.
    let ids: Vec<uuid::Uuid> = [&tea, &cup].iter().map(|i| i.parse().unwrap()).collect();
    let page = t.k.render_documents(&t.ctx, "inventory.item_label", &ids, 2, None, false).await.expect("item labels");
    let html = String::from_utf8(page.bytes).unwrap();
    assert_eq!(html.matches("class=\"l\"").count(), 4, "two copies of two items");
    assert_eq!(html.matches("<svg").count(), 4, "each with its barcode");
    let text = t.k.render_documents(&t.ctx, "inventory.item_label_printer", &ids, 1, None, false).await.expect("printer labels");
    let text = String::from_utf8(text.bytes).unwrap();
    assert!(text.contains("^FD4006381333931^FS") && text.contains("^FD12345670^FS"), "{text}");
    let serials = t.k.render_documents(&t.ctx, "buying.serial_labels", &[uid(&pr)], 1, None, false).await.expect("serial labels");
    assert_eq!(String::from_utf8(serials.bytes).unwrap().matches("<svg").count(), 2);
    let batches = t.k.render_documents(&t.ctx, "stock_ops.batch_label", &[uid(&batch)], 1, None, false).await.expect("batch label");
    assert!(String::from_utf8(batches.bytes).unwrap().contains("<svg"));

    let scan = |text: &str| {
        let ask = json!({"id": c, "params": {"text": text}});
        let k = t.k.clone();
        let ctx = t.ctx.clone();
        async move { k.invoke(&ctx, Invocation::new("core.company.scan", ask)).await.expect("scan").result }
    };
    let kinds = |v: &Value| v["matches"].as_array().unwrap().iter().map(|m| m["kind"].as_str().unwrap().to_string()).collect::<Vec<_>>();

    let by_barcode = scan("12345670").await;
    assert_eq!(
        (kinds(&by_barcode), by_barcode["matches"][0]["item"].clone(), by_barcode["ambiguous"].clone()),
        (vec!["item".to_string()], json!(cup), json!(false))
    );
    assert_eq!(by_barcode["matches"][0]["uom"], nos["id"]);
    let both = scan("4006381333931").await;
    assert_eq!(both["ambiguous"], json!(true));
    let items: Vec<&Value> = both["matches"].as_array().unwrap().iter().map(|m| &m["item"]).collect();
    assert!(items.contains(&&json!(tea)) && items.contains(&&json!(clash)), "{both}");
    let lot = scan("LOT-7").await;
    assert_eq!(
        (kinds(&lot), lot["matches"][0]["expiry"].clone(), lot["matches"][0]["item"].clone()),
        (vec!["batch".to_string()], json!("2027-03-31"), json!(tea))
    );
    let sn = scan("SN-2").await;
    assert_eq!((kinds(&sn), sn["matches"][0]["item"].clone(), sn["matches"][0]["warehouse"].clone()), (vec!["serial".to_string()], json!(phone), json!(wh)));
    let number = t.get("PurchaseReceipt", uid(&pr)).await["number"].as_str().unwrap().to_string();
    let doc = scan(&number).await;
    assert_eq!((kinds(&doc), doc["matches"][0]["entity"].clone()), (vec!["document".to_string()], json!("PurchaseReceipt")));
    // GS1: the GTIN's leading zero dropped to find the item's barcode, its batch, expiry and quantity.
    let g = scan("]C101040063813339311727033110LOT-7\u{1d}3012").await;
    let first = &g["matches"][0];
    assert_eq!(
        (first["kind"].clone(), first["item"].clone(), first["batch_no"].clone(), first["expiry"].clone(), first["qty"].clone()),
        (json!("gs1"), json!(tea), json!("LOT-7"), json!("2027-03-31"), json!("12")),
        "{g}"
    );
    let none = scan("NOTHING-HERE").await;
    assert_eq!((none["matches"].as_array().unwrap().len(), none["ambiguous"].clone(), none["gs1"].clone()), (0, json!(false), Value::Null));
    let long = t.k.invoke(&t.ctx, Invocation::new("core.company.scan", json!({"id": c, "params": {"text": "x".repeat(201)}}))).await;
    assert!(long.is_err(), "a scan is bounded");
}
