//! Carriers behind one interface, against a real PostgreSQL (`FERRUM_ERP_SHIPPING_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_shipping_test`; reset first) and a carrier on a local port:
//! a delivery's parcels get rates, one is chosen, the label is bought once though pressed twice at once (one call
//! to the carrier, one freight posting, one file), the invoice made from the delivery charges it, tracking moves it
//! forward when asked and when the carrier calls signed, and an unsigned call is refused and counted. A test
//! carrier answers a purchase return's calls from fixtures, and cancelling its label takes the freight back.

mod common;
use common::*;
use ferrum_kernel::{KernelSettings, Site, SiteRequest};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn carriers() {
    run(scenario());
}

/// A carrier on a local port: rates, labels (counted, refusing a call without the key), tracking, cancelling.
async fn carrier() -> (String, Arc<AtomicUsize>) {
    use axum::Json;
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    let labels = Arc::new(AtomicUsize::new(0));
    let counted = labels.clone();
    let app = axum::Router::new()
        .route(
            "/v1/rates",
            post(|Json(b): Json<Value>| async move {
                let w: f64 = b["parcels"][0]["weight"].as_str().unwrap_or("0").parse().unwrap_or(0.0);
                Json(json!({"result": {"rates": [
                    {"name": "Ground", "price": {"value": format!("{:.2}", 10.0 + w), "currency": "INR"}, "eta_days": 4, "ref": "g1"},
                    {"name": "Express", "price": {"value": "30.00", "currency": "INR"}, "eta_days": 1, "ref": "x1"}]}}))
            }),
        )
        .route(
            "/v1/labels",
            post(move |headers: HeaderMap, Json(b): Json<Value>| {
                let counted = counted.clone();
                async move {
                    if headers.get("authorization").and_then(|h| h.to_str().ok()) != Some("Bearer key-123") || headers.get("idempotency-key").is_none() {
                        return (axum::http::StatusCode::UNAUTHORIZED, Json(json!({"error": {"message": "bad key"}})));
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    let n = counted.fetch_add(1, Ordering::SeqCst) + 1;
                    assert_eq!(b["rate"], "x1");
                    (
                        axum::http::StatusCode::OK,
                        Json(json!({"label": {"tracking": format!("TRK{n}"), "file": "XlhBXkZPMTAsMTBeRkRIZWxsb15GU15YWg==", "id": format!("L{n}")}})),
                    )
                }
            }),
        )
        .route("/v1/track/{t}", get(|| async { Json(json!({"tracking": {"state": "IN_TRANSIT"}})) }))
        .route("/v1/cancel/{id}", post(|| async { Json(json!({"ok": true})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, labels)
}

fn signed(secret: &str, body: &[u8]) -> String {
    use hmac::Mac;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_SHIPPING_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_shipping_test".into());
    let settings =
        KernelSettings { secret_key: Some(ferrum_kernel::secret::SecretKey::new([7; 32])), webhooks_allow_private: true, ..KernelSettings::default() };
    let t = T::boot_with(url, settings, |_, _| {}).await;
    let (base, label_calls) = carrier().await;
    let c = t.company.clone();
    let wh = t.find_one("Warehouse", json!({"warehouse_name": "Stores", "company": c})).await["id"].as_str().unwrap().to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let item = |code: &str, stock: bool| json!({"item_code": code, "item_name": code, "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "100", "is_stock_item": stock});
    let widget = uid(&t.insert("Item", item("Widget", true)).await).to_string();
    let freight_item = uid(&t.insert("Item", item("Freight", false)).await).to_string();
    t.post(
        "StockEntry",
        json!({"company": c, "purpose": "Material Receipt", "items": [{"item": widget, "qty": "10", "basic_rate": "40", "target_warehouse": wh}]}),
    )
    .await;
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let to = |line: &str| json!({"address_title": "Dock", "address_type": "Shipping", "address_line1": line, "city": "Springfield", "country": "India"});
    let dock = uid(&t.insert("Address", to("1 Example Street")).await).to_string();
    let (freight, owed) = (t.account("Freight and Forwarding").await, t.account("Accrued Expenses").await);

    // A carrier described as calls: no carrier's name or format in the code.
    let desc = json!({
        "rate": {"url": format!("{base}/v1/rates"), "headers": {"Authorization": "Bearer {{ secret.api_key }}"},
                 "body": "{\"to\": {\"city\": {{ to.city|json }}, \"line\": {{ to.address_line1|json }}}, \"parcels\": [{% for p in shipment.packages %}{\"weight\": {{ p.weight|json }}, \"unit\": {{ weight_unit|json }}}{% if not loop.last %},{% endif %}{% endfor %}]}",
                 "rates_at": "/result/rates", "service_at": "/name", "amount_at": "/price/value", "currency_at": "/price/currency", "days_at": "/eta_days", "rate_id_at": "/ref",
                 "error_at": "/error/message"},
        "label": {"url": format!("{base}/v1/labels"), "headers": {"Authorization": "Bearer {{ secret.api_key }}", "Idempotency-Key": "{{ key }}"},
                  "body": "{\"rate\": {{ rate.id|json }}, \"reference\": {{ shipment.reference|json }}}",
                  "tracking_at": "/label/tracking", "label_at": "/label/file", "label_type": "zpl", "id_at": "/label/id", "error_at": "/error/message"},
        "track": {"url": format!("{base}/v1/track/{{{{ tracking_number }}}}"), "method": "GET", "status_at": "/tracking/state",
                  "statuses": {"IN_TRANSIT": "In Transit", "DELIVERED": "Delivered", "FAILED": "Exception"}},
        "cancel": {"url": format!("{base}/v1/cancel/{{{{ provider_id }}}}")},
        "tracking_link": "https://track.carrier.example/{{ tracking_number }}",
        "inbound": {"auth": "hmac", "header": "X-Signature", "tracking_at": "/data/tracking", "status_at": "/data/state",
                    "statuses": {"DELIVERED": "Delivered", "IN_TRANSIT": "In Transit"}}
    });
    let mut bad = desc.clone();
    bad["rate"]["amount_at"] = json!("price");
    let refused = t.k.insert(&t.ctx, "CarrierConnection", obj(json!({"connection_name": "Bad", "company": c, "description": bad}))).await;
    assert!(format!("{refused:?}").contains("not a JSON pointer"), "{refused:?}");
    let conn = t
        .insert(
            "CarrierConnection",
            json!({"connection_name": "Parcels", "company": c, "description": desc, "api_key": "key-123", "webhook_secret": "hook-456",
                   "freight": "Charged to Customer", "freight_account": freight, "freight_payable_account": owed, "freight_item": freight_item}),
        )
        .await;
    assert_eq!(conn["api_key"], json!(true), "a secret reads only as set");
    let conn_id = uid(&conn);

    let dn =
        t.post("DeliveryNote", json!({"company": c, "customer": globex, "ship_to": dock, "set_warehouse": wh, "items": [{"item": widget, "qty": "2"}]})).await;
    let made = t.action("DeliveryNote", uid(&dn), "make_carrier_shipment", json!({"weight": "2.5"})).await.expect("a shipment");
    let sid: uuid::Uuid = made["id"].as_str().unwrap().parse().unwrap();
    let rates = t.action("CarrierShipment", sid, "get_rates", json!({})).await.expect("rates");
    assert_eq!(rates["rates"][0]["service"], "Ground");
    assert_eq!(dec(&rates["rates"][0]["amount"]), dec(&json!("12.5")), "the parcel's weight reached the carrier");
    let refused = t.action("CarrierShipment", sid, "choose_rate", json!({"service": "Overnight"})).await;
    assert!(refused.unwrap_err().contains("not among the rates offered"));
    t.action("CarrierShipment", sid, "choose_rate", json!({"service": "Express"})).await.expect("chosen");

    // Pressed twice at once: one label, one call, one posting, one file.
    let (a, b) = tokio::join!(t.action("CarrierShipment", sid, "buy_label", json!({})), t.action("CarrierShipment", sid, "buy_label", json!({})));
    // The other either found the label under the lock or, coming after, was told the action is done.
    let bought: Vec<Value> = [a, b].into_iter().filter_map(|r| r.map_err(|e| assert!(e.contains("not available"), "{e}")).ok()).collect();
    assert!(bought.iter().filter(|r| r["already"] == json!(false)).count() == 1, "{bought:?}");
    assert!(bought.iter().all(|r| r["tracking_number"] == json!("TRK1")));
    assert_eq!(label_calls.load(Ordering::SeqCst), 1);
    let ship = t.get("CarrierShipment", sid).await;
    assert_eq!((ship["status"].clone(), ship["tracking_number"].clone()), (json!("Label Bought"), json!("TRK1")));
    assert_eq!(ship["tracking_url"], "https://track.carrier.example/TRK1");
    let entries: Vec<(rust_decimal::Decimal,)> =
        t.sql("select total_debit from journal_entry where source_entity = 'CarrierShipment' and source_id = $1", sid).await;
    assert_eq!(entries, vec![(dec(&json!("30")),)]);
    let files = t.k.attachments(&t.ctx, "CarrierShipment", sid).await.unwrap();
    assert_eq!(files.len(), 1, "{files:?}");
    let labels = t.k.render_documents(&t.ctx, "inventory.parcel_labels", &[sid], 1, None, false).await.expect("parcel labels");
    let labels = String::from_utf8(labels.bytes).unwrap();
    assert!(labels.contains("1 Example Street") && labels.contains("<svg") && labels.contains("1 / 1"), "{labels}");

    // The upkeep tracks a labelled shipment whose connection tracks every so many hours (6 by default), once.
    assert_eq!(ferrum_app_erp::shipping::upkeep(&t.k, &t.ctx, 100).await.expect("the upkeep's tracking query"), 1);
    assert_eq!(t.get("CarrierShipment", sid).await["status"], "In Transit");
    assert_eq!(ferrum_app_erp::shipping::upkeep(&t.k, &t.ctx, 100).await.unwrap(), 0, "tracked a moment ago: not due again");

    // The invoice made from the delivery charges the label once.
    let inv = t.action("DeliveryNote", uid(&dn), "make_sales_invoice", json!({})).await.expect("an invoice");
    let inv = t.get("SalesInvoice", inv["id"].as_str().unwrap().parse().unwrap()).await;
    let lines = inv["items"].as_array().unwrap();
    let charged: Vec<&Value> = lines.iter().filter(|l| l["carrier_shipment"] == json!(sid.to_string())).collect();
    assert_eq!((charged.len(), dec(&charged[0]["rate"])), (1, dec(&json!("30"))), "{lines:?}");
    let refused = t.action("CarrierShipment", sid, "cancel_label", json!({})).await;
    assert!(refused.unwrap_err().contains("charges this label"));

    // Tracking: asked, then told by the carrier (signed), and an unsigned call refused and counted.
    let tracked = t.action("CarrierShipment", sid, "track", json!({})).await.expect("tracked");
    assert_eq!(tracked["status"], "In Transit");
    let site = ferrum_app_erp::shipping::CarrierSite;
    let call = |body: Value, sig: Option<String>| {
        let raw = serde_json::to_vec(&body).unwrap();
        let mut headers = serde_json::Map::new();
        if let Some(s) = sig {
            headers.insert("x-signature".into(), json!(s));
        }
        SiteRequest {
            tenant: t.ctx.tenant,
            user: None,
            method: "POST".into(),
            path: format!("/carriers/{conn_id}"),
            query: Default::default(),
            headers,
            body: raw,
            client_ip: "127.0.0.1".into(),
        }
    };
    let told = json!({"data": {"tracking": "TRK1", "state": "DELIVERED"}});
    let unsigned = site.handle(&t.k, call(told.clone(), Some("sha256=00".into()))).await.unwrap();
    assert_eq!(unsigned.status, 401);
    assert_eq!(t.get("CarrierShipment", sid).await["status"], "In Transit");
    let sig = signed("hook-456", &serde_json::to_vec(&told).unwrap());
    let ok = site.handle(&t.k, call(told, Some(sig))).await.unwrap();
    assert_eq!(ok.status, 200);
    let ship = t.get("CarrierShipment", sid).await;
    assert_eq!(ship["status"], "Delivered");
    assert!(!ship["delivered_at"].is_null());
    assert_eq!(t.get("CarrierConnection", conn_id).await["refused_calls"], json!(1));

    // A test carrier answers from fixtures: a purchase return goes back, and cancelling takes the freight back.
    let acme = uid(&t.insert("Supplier", json!({"supplier_name": "Acme"})).await).to_string();
    let depot = uid(&t.insert("Address", to("9 Supplier Road")).await).to_string();
    let fixtures = json!({"rate": {"result": {"rates": [{"name": "Ground", "price": {"value": "8.40", "currency": "INR"}, "eta_days": 2, "ref": "g"}]}},
                          "label": {"label": {"tracking": "FIX1", "file": "UERG", "id": "F1"}}, "cancel": {"ok": true}});
    let test_conn = uid(&t
        .insert(
            "CarrierConnection",
            json!({"connection_name": "Test", "company": c, "test": true, "description": desc, "fixtures": fixtures,
                   "freight": "Absorbed", "freight_account": freight, "freight_payable_account": owed}),
        )
        .await);
    let pr = t
        .post("PurchaseReceipt", json!({"company": c, "supplier": acme, "supplier_address": depot, "set_warehouse": wh, "items": [{"item": widget, "qty": "3", "price_list_rate": "40"}]}))
        .await;
    let back = t.action("PurchaseReceipt", uid(&pr), "make_purchase_return", json!({})).await.expect("a return");
    let back = t.submit("PurchaseReceipt", back["id"].as_str().unwrap().parse().unwrap()).await;
    let made =
        t.action("PurchaseReceipt", uid(&back), "make_carrier_shipment", json!({"weight": "4", "carrier": test_conn.to_string()})).await.expect("a shipment");
    let rid: uuid::Uuid = made["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(t.get("CarrierShipment", rid).await["ship_to"], json!(depot));
    t.action("CarrierShipment", rid, "get_rates", json!({})).await.expect("rates");
    t.action("CarrierShipment", rid, "choose_rate", json!({"service": "Ground"})).await.expect("chosen");
    let bought = t.action("CarrierShipment", rid, "buy_label", json!({})).await.expect("bought");
    assert_eq!((bought["tracking_number"].clone(), label_calls.load(Ordering::SeqCst)), (json!("FIX1"), 1), "nobody was called");
    t.action("CarrierShipment", rid, "cancel_label", json!({})).await.expect("cancelled");
    let freight_id: uuid::Uuid = freight.parse().unwrap();
    let moved: Vec<(rust_decimal::Decimal, rust_decimal::Decimal)> = t
        .sql(
            &format!(
                "select coalesce(sum(l.debit), 0), coalesce(sum(l.credit), 0) from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
                 where e.source_entity = 'CarrierShipment' and e.source_id = $1 and l.account_id = '{freight_id}'"
            ),
            rid,
        )
        .await;
    assert_eq!(moved, vec![(dec(&json!("8.4")), dec(&json!("8.4")))], "booked, then taken back");
    assert_eq!(t.get("CarrierShipment", rid).await["status"], "Cancelled");
}
