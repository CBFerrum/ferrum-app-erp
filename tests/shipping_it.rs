//! Carriers behind one interface, against a real PostgreSQL (`FERRUM_ERP_SHIPPING_TEST_DATABASE_URL`, default
//! `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_shipping_test`; reset first) and a carrier on a local port:
//! a delivery's parcels get rates, one is chosen, the label is bought once though pressed twice at once (one call
//! to the carrier, one freight posting, one file), the invoice made from the delivery charges it, tracking moves it
//! forward when asked and when the carrier calls signed, and an unsigned call is refused and counted. A test
//! carrier answers a purchase return's calls from fixtures, and cancelling its label takes the freight back.

mod common;
use common::*;
use ferrum_kernel::KernelSettings;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn carriers() {
    run(scenario());
}

/// A carrier on a local port: rates, labels (counted, refusing a call without the key), tracking (counted), cancelling.
async fn carrier() -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    use axum::Json;
    use axum::http::HeaderMap;
    use axum::routing::{get, post};
    let labels = Arc::new(AtomicUsize::new(0));
    let counted = labels.clone();
    let tracks = Arc::new(AtomicUsize::new(0));
    let tracked = tracks.clone();
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
        .route(
            "/v1/track/{t}",
            get(move || {
                tracked.fetch_add(1, Ordering::SeqCst);
                async { Json(json!({"tracking": {"state": "IN_TRANSIT"}})) }
            }),
        )
        .route("/v1/cancel/{id}", post(|| async { Json(json!({"ok": true})) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, labels, tracks)
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_SHIPPING_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_shipping_test".into());
    let settings = KernelSettings { webhooks_allow_private: true, ..KernelSettings::default() };
    let t = T::boot_with(url, settings, |_, _| {}).await;
    let (base, label_calls, track_calls) = carrier().await;
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
                 "body": "{\"to\": {\"city\": {{ inputs.to.city|json }}, \"line\": {{ inputs.to.address_line1|json }}}, \"parcels\": [{% for p in inputs.shipment.packages %}{\"weight\": {{ p.weight|json }}, \"unit\": {{ inputs.weight_unit|json }}}{% if not loop.last %},{% endif %}{% endfor %}]}",
                 "rates_at": "/result/rates", "service_at": "/name", "amount_at": "/price/value", "currency_at": "/price/currency", "days_at": "/eta_days", "rate_id_at": "/ref",
                 "error_at": "/error/message"},
        "label": {"url": format!("{base}/v1/labels"), "headers": {"Authorization": "Bearer {{ secret.api_key }}", "Idempotency-Key": "{{ key }}"},
                  "body": "{\"rate\": {{ inputs.rate.id|json }}, \"reference\": {{ inputs.shipment.reference|json }}}",
                  "tracking_at": "/label/tracking", "label_at": "/label/file", "label_type": "zpl", "id_at": "/label/id", "error_at": "/error/message"},
        "track": {"url": format!("{base}/v1/track/{{{{ inputs.tracking_number }}}}"), "method": "GET", "status_at": "/tracking/state",
                  "statuses": {"IN_TRANSIT": "In Transit", "DELIVERED": "Delivered", "FAILED": "Exception"}},
        "cancel": {"url": format!("{base}/v1/cancel/{{{{ inputs.provider_id }}}}")},
        "tracking_link": "https://track.carrier.example/{{ tracking_number }}",
        "inbound": {"auth": "token", "header": "X-Hook-Token", "tracking_at": "/data/tracking", "status_at": "/data/state",
                    "statuses": {"DELIVERED": "Delivered", "IN_TRANSIT": "In Transit"}}
    });
    let mut bad = desc.clone();
    bad["rate"]["amount_at"] = json!("price");
    let refused = t.k.insert(&t.ctx, "CarrierConnection", obj(json!({"connection_name": "Bad", "company": c, "description": bad}))).await;
    assert!(format!("{refused:?}").contains("not a JSON pointer"), "{refused:?}");
    // Tracking calls that nothing would check are refused when the carrier is saved: the Inbound Secret comes in a header.
    let mut unchecked = desc.clone();
    unchecked["inbound"]["auth"] = json!("hmac");
    let refused = t.k.insert(&t.ctx, "CarrierConnection", obj(json!({"connection_name": "Unchecked", "company": c, "description": unchecked}))).await;
    assert!(format!("{refused:?}").contains("inbound.auth is token"), "{refused:?}");
    let conn = t
        .insert(
            "CarrierConnection",
            json!({"connection_name": "Parcels", "company": c, "description": desc, "api_key": "key-123", "webhook_secret": "hook-456",
                   "freight": "Charged to Customer", "freight_account": freight, "freight_payable_account": owed, "freight_item": freight_item}),
        )
        .await;
    let conn_id = uid(&conn);
    // The secrets moved into the carrier's connection, cleared from the carrier: never in two places.
    let sctx = ferrum_kernel::Ctx::system(t.ctx.tenant, t.ctx.schema.clone());
    let connection_name = t.get("CarrierConnection", conn_id).await["connection"].as_str().unwrap().to_string();
    let defs =
        || async { t.k.connection_defs(&sctx).await.unwrap().into_iter().find(|d| d["name"] == json!(connection_name)).expect("the carrier's connection") };
    let held = defs().await;
    assert_eq!((held["enabled"].clone(), held["secrets"].clone()), (json!(true), json!(["api_key", "webhook_secret"])));
    let carrier_now = t.get("CarrierConnection", conn_id).await;
    assert_eq!((carrier_now["api_key"].clone(), carrier_now["webhook_secret"].clone()), (Value::Null, Value::Null));
    // Typing one key again would drop the other: refused rather than lost.
    let partial = t.k.update(&t.ctx, "CarrierConnection", conn_id, obj(json!({"api_key": "key-999"})), None).await;
    assert!(format!("{partial:?}").contains("type every key again"), "{partial:?}");
    assert_eq!(t.get("CarrierConnection", conn_id).await["api_key"], Value::Null);

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
    // Every replica runs the upkeep: one finding the shipment due while another is tracking it leaves it to that one,
    // so the carrier is asked once. Due again; one upkeep holds it (asking the carrier) as another starts.
    let mut first = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    sqlx::query("update e_carrier_shipment set last_tracked_at = now() - interval '7 hours' where id = $1").bind(sid).execute(&mut *first).await.unwrap();
    first.commit().await.unwrap();
    let asked = track_calls.load(Ordering::SeqCst);
    let mut first = t.k.db.tenant_tx(t.ctx.tenant).await.unwrap();
    sqlx::query("select id from e_carrier_shipment where id = $1 for update").bind(sid).execute(&mut *first).await.unwrap();
    let pid: i32 = sqlx::query_scalar("select pg_backend_pid()").fetch_one(&mut *first).await.unwrap();
    let second = {
        let (k, ctx) = (t.k.clone(), t.ctx.clone());
        tokio::spawn(async move { ferrum_app_erp::shipping::upkeep(&k, &ctx, 100).await })
    };
    // Until the second waits for the first, or is done: either comes, however loaded the machine.
    for _ in 0..2400 {
        let waits: bool = sqlx::query_scalar("select exists (select 1 from pg_stat_activity where $1 = any(pg_blocking_pids(pid)))")
            .bind(pid)
            .fetch_one(&t.k.db.pool)
            .await
            .unwrap();
        if waits || second.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    // The first records the carrier's answer, as tracking does.
    sqlx::query("update e_carrier_shipment set last_tracked_at = now() where id = $1").bind(sid).execute(&mut *first).await.unwrap();
    first.commit().await.unwrap();
    assert_eq!(second.await.unwrap().expect("the second upkeep"), 0, "the second upkeep leaves what the first tracks");
    assert_eq!(track_calls.load(Ordering::SeqCst), asked, "the carrier is asked once");

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
    let inbound = |body: &Value, token: &str| {
        let (k, id) = (t.k.clone(), held["id"].as_str().unwrap().parse().unwrap());
        let (body, headers) = (serde_json::to_vec(body).unwrap(), std::collections::BTreeMap::from([("x-hook-token".to_string(), token.to_string())]));
        async move { k.connection_inbound(id, &headers, &std::collections::BTreeMap::new(), &body).await }
    };
    let told = json!({"data": {"tracking": "TRK1", "state": "DELIVERED"}});
    assert!(inbound(&told, "wrong").await.is_err());
    t.k.run_jobs_once(10).await.unwrap();
    assert_eq!(t.get("CarrierShipment", sid).await["status"], "In Transit");
    inbound(&told, "hook-456").await.expect("the carrier's own token is accepted");
    t.k.run_jobs_once(10).await.unwrap();
    let ship = t.get("CarrierShipment", sid).await;
    assert_eq!(ship["status"], "Delivered");
    assert!(!ship["delivered_at"].is_null());
    assert_eq!(defs().await["refused"], json!(1), "the connection counts the refused call");
    // The same delivery again is a replay, refused.
    assert!(inbound(&told, "hook-456").await.is_err());

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

    // ───────────── carrier templates and comparing carriers (erp-13) ─────────────
    // a_template_with_a_wrong_description_is_refused; a_connection_is_filled_from_its_template_once.
    assert!(t.k.insert(&t.ctx, "CarrierTemplate", obj(json!({"template_name": "Bad", "description": {"rate": 1}}))).await.is_err());
    let template = uid(&t.insert("CarrierTemplate", json!({"template_name": "Test Courier", "description": desc, "needs": "No key: a test carrier"})).await);
    let user = t.k.create_user(&t.ctx, "stock@fin.test", "Stock", Some("Correct-Horse-9"), &["Stock User".into()], false).await.expect("user");
    let stock_user = t.k.load_ctx(t.ctx.tenant, user, None, "it".into()).await.unwrap();
    assert!(
        t.k.insert(&stock_user, "CarrierTemplate", obj(json!({"template_name": "Mine", "description": desc}))).await.is_err(),
        "a Stock User only reads templates"
    );
    let offers = |ground: &str, express: Option<&str>, tracking: &str| {
        let mut rates = vec![json!({"name": "Ground", "price": {"value": ground, "currency": "INR"}, "eta_days": 3, "ref": "g2"})];
        if let Some(e) = express {
            rates.push(json!({"name": "Express", "price": {"value": e, "currency": "INR"}, "eta_days": 1, "ref": "e2"}));
        }
        json!({"rate": {"result": {"rates": rates}}, "label": {"label": {"tracking": tracking, "file": "UERG", "id": "F2"}}, "cancel": {"ok": true}})
    };
    let carrier = |name: &'static str, company: String, fixtures: Value| {
        let t = &t;
        async move {
            uid(&t
                .insert(
                    "CarrierConnection",
                    json!({"connection_name": name, "company": company, "test": true, "template": template.to_string(), "fixtures": fixtures}),
                )
                .await)
        }
    };
    let second = carrier("Second", c.clone(), offers("9.10", Some("12"), "FIX2")).await;
    assert_eq!(t.get("CarrierConnection", second).await["description"], desc, "the template's calls");
    t.k.update(&t.ctx, "CarrierConnection", second, obj(json!({"connection_name": "Second Courier"})), None).await.expect("renamed");
    assert_eq!(t.get("CarrierConnection", second).await["description"], desc);
    let empty = carrier("Empty", c.clone(), json!({"rate": {"result": {"rates": []}}})).await;
    let inr = t.find_one("Currency", json!({"code": "INR"})).await;
    let other_co = uid(&t.insert("Company", json!({"company_name": "Other Co", "abbr": "OC", "default_currency": inr["id"]})).await).to_string();
    let elsewhere = carrier("Elsewhere", other_co, offers("1", None, "X")).await;

    let pr = t
        .post("PurchaseReceipt", json!({"company": c, "supplier": acme, "supplier_address": depot, "set_warehouse": wh, "items": [{"item": widget, "qty": "2", "price_list_rate": "40"}]}))
        .await;
    let back = t.action("PurchaseReceipt", uid(&pr), "make_purchase_return", json!({})).await.expect("a return");
    let back = t.submit("PurchaseReceipt", back["id"].as_str().unwrap().parse().unwrap()).await;
    let made =
        t.action("PurchaseReceipt", uid(&back), "make_carrier_shipment", json!({"weight": "2", "carrier": test_conn.to_string()})).await.expect("a shipment");
    let sid2: uuid::Uuid = made["id"].as_str().unwrap().parse().unwrap();
    // two_carriers_offers_sit_side_by_side (the first through the gate), then asking_a_carrier_again_replaces_only_its_offers.
    let ask = |carrier: Option<uuid::Uuid>, key: &str| {
        ferrum_kernel::capability::Invocation::new(
            "inventory.carrier_shipment.get_rates",
            json!({"id": sid2.to_string(), "params": {"carrier": carrier.map(|c| c.to_string())}}),
        )
        .key(key)
    };
    t.k.invoke(&t.ctx, ask(None, "r1")).await.expect("the shipment's carrier");
    t.action("CarrierShipment", sid2, "get_rates", json!({"carrier": second.to_string()})).await.expect("the second");
    t.action("CarrierShipment", sid2, "get_rates", json!({"carrier": second.to_string()})).await.expect("the second again");
    let held = || {
        let t = &t;
        async move {
            let mut v: Vec<(String, String)> = t.get("CarrierShipment", sid2).await["rates"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| (r["carrier"].as_str().unwrap().to_string(), r["service"].as_str().unwrap().to_string()))
                .collect();
            v.sort();
            v
        }
    };
    let mut want = vec![(test_conn.to_string(), "Ground".to_string()), (second.to_string(), "Ground".to_string()), (second.to_string(), "Express".to_string())];
    want.sort();
    assert_eq!(held().await, want);
    // a_carrier_that_offers_nothing_leaves_the_others_offers; a_carrier_of_another_company_is_refused; rates_need_write_on_the_shipment.
    let e = t.action("CarrierShipment", sid2, "get_rates", json!({"carrier": empty.to_string()})).await.unwrap_err();
    assert!(e.contains("offered no rate"), "{e}");
    let e = t.action("CarrierShipment", sid2, "get_rates", json!({"carrier": elsewhere.to_string()})).await.unwrap_err();
    assert!(e.contains("another company's carrier"), "{e}");
    assert_eq!(held().await, want);
    let user = t.k.create_user(&t.ctx, "books@fin.test", "Books", Some("Correct-Horse-9"), &["Accounts User".into()], false).await.expect("user");
    let books = t.k.load_ctx(t.ctx.tenant, user, None, "it".into()).await.unwrap();
    assert!(t.k.run_action(&books, "CarrierShipment", sid2, "get_rates", json!({})).await.is_err());
    // choosing_another_carriers_offer_moves_the_shipment_to_it (through the gate), and the label comes from it.
    let chosen =
        t.k.invoke(
            &t.ctx,
            ferrum_kernel::capability::Invocation::new(
                "inventory.carrier_shipment.choose_rate",
                json!({"id": sid2.to_string(), "params": {"service": "Express", "carrier": second.to_string()}}),
            )
            .key("c1"),
        )
        .await
        .expect("chosen");
    assert_eq!(chosen.result["amount"], json!("12"));
    let ship = t.get("CarrierShipment", sid2).await;
    assert_eq!((ship["carrier"].clone(), ship["service"].clone()), (json!(second.to_string()), json!("Express")));
    let bought = t.action("CarrierShipment", sid2, "buy_label", json!({})).await.expect("bought");
    assert_eq!(bought["tracking_number"], json!("FIX2"), "from the chosen carrier");

    // The carrier's secrets are in no log, audit row, job or call record, nor any other table but sealed.
    for table in ["audit_event", "job", "conn_call", "conn_inbound", "e_carrier_connection", "e_carrier_shipment"] {
        let n: Vec<(i64,)> =
            t.sql(&format!("select count(*) from {table} t where $1::uuid is not null and (t::text like '%key-123%' or t::text like '%hook-456%' or t::text like '%key-999%')"), uuid::Uuid::nil()).await;
        assert_eq!(n, vec![(0,)], "a carrier secret rests in {table}");
    }
}
