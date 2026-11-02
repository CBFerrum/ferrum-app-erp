//! Shipping. Picked stock waits at the outbound bin. Each unit is verified before it may leave,
//! the weight and label are checked against what is expected, and only what was verified goes:
//! a partial shipment is a true one, and each parcel carries only what has not gone yet. Handing over posts ERP's delivery note, which takes the stock
//! out of the bin. A carrier's failure repeats nothing, because the shipment records what went.

use crate::position::{self, Cause, Key, Kind};
use crate::tasks::Refused;
use crate::{posting, slotting};
use ferrum_domain::util::{d, elevated, id, rows, s, sys};
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Tx, audit};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

/// How far a weighed parcel may be from what it should weigh.
const WEIGHT_TOLERANCE_PERCENT: i64 = 5;

/// A shipment for what a wave allocated to one order.
pub async fn prepare(k: &Kernel, ctx: &Ctx, tx: &mut Tx, wave: &Doc, order: Uuid, warehouse: Uuid, items: Vec<Value>) -> Result<Uuid> {
    let customer: Uuid =
        sqlx::query_scalar("select customer from e_sales_order where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(order).fetch_one(&mut **tx).await?;
    let number: Option<String> =
        sqlx::query_scalar("select number from e_sales_order where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(order).fetch_one(&mut **tx).await?;
    let reference = format!("{} / {}", s(wave, "number").unwrap_or_default(), number.unwrap_or_default());
    let doc = json!({"reference": reference, "company": wave.get("company"), "warehouse": warehouse, "customer": customer, "items": items});
    let made = k.insert_tx(&elevated(ctx), tx, "Shipment", doc.as_object().cloned().unwrap_or_default()).await?;
    id(&made, "id").ok_or_else(|| Error::internal("a shipment without an id"))
}

#[derive(Debug, Deserialize)]
pub struct Verify {
    /// What was scanned: an item's barcode, or its code.
    pub code: String,
    #[serde(default)]
    pub lot: Option<String>,
    pub qty: Decimal,
}

/// A scan at packing. The wrong item is refused, with the attempt kept, and so is more than was
/// picked for this shipment.
pub async fn verify(k: &Kernel, ctx: &Ctx, tx: &mut Tx, shipment: Uuid, v: &Verify) -> Result<std::result::Result<Value, Refused>> {
    let doc = k.get_tx(&sys(ctx), tx, "Shipment", shipment, true).await?;
    let item = slotting::item_by_scan(tx, ctx.tenant, &v.code).await?;
    let lines = rows(&doc, "items");
    let line = lines.iter().find(|l| {
        item.is_some()
            && id(l, "item") == item
            && (v.lot.is_none() || s(l, "lot") == v.lot.as_deref() || s(l, "lot").unwrap_or_default().is_empty())
            && d(l, "verified_qty") < d(l, "qty")
    });
    let Some(line) = line else {
        audit::record(tx, ctx, "Shipment", shipment, "wms.wrong_scan", None, json!({"scanned": v.code, "lot": v.lot})).await?;
        return Ok(Err(Refused(format!("{} is not waiting on this shipment", v.code))));
    };
    let verified = d(line, "verified_qty") + v.qty;
    if v.qty <= Decimal::ZERO || verified > d(line, "qty") {
        return Err(Error::field("qty", format!("{} more at most", (d(line, "qty") - d(line, "verified_qty")).normalize())));
    }
    let mut values = Doc::new();
    values.insert("verified_qty".into(), json!(verified));
    let line_id = id(line, "id").ok_or_else(|| Error::internal("a line without an id"))?;
    k.set_child_values_tx(&sys(ctx), tx, "Shipment", "items", line_id, values).await?;
    let all = lines.iter().all(|l| if id(l, "id") == Some(line_id) { verified == d(l, "qty") } else { d(l, "verified_qty") == d(l, "qty") });
    // What this parcel should weigh: what is verified and has not gone in an earlier one.
    let weight: Option<Decimal> = sqlx::query_scalar(
        "select sum((coalesce(l.verified_qty, 0) - coalesce(l.handed_over_qty, 0)) * coalesce(i.wms_weight_kg, 0)) from e_shipment__items l \
         join e_item i on i.tenant_id = l.tenant_id and i.id = l.item where l.tenant_id = $1 and l.parent_id = $2",
    )
    .bind(ctx.tenant)
    .bind(shipment)
    .fetch_one(&mut **tx)
    .await?;
    let mut done = Doc::new();
    if all {
        done.insert("status".into(), json!("Packed"));
    }
    done.insert("expected_weight_kg".into(), json!(weight.unwrap_or_default()));
    k.set_values_tx(&sys(ctx), tx, "Shipment", shipment, done).await?;
    Ok(Ok(json!({"verified": verified, "complete": all})))
}

#[derive(Debug, Deserialize)]
pub struct Handover {
    pub carrier: String,
    pub tracking: String,
    /// The label scanned on the parcel, which must read the same as the tracking number.
    pub label: String,
    #[serde(default)]
    pub weighed_kg: Option<Decimal>,
}

/// What on a line has been verified and has not gone yet.
fn unsent(line: &Doc) -> Decimal {
    d(line, "verified_qty") - d(line, "handed_over_qty")
}

fn weight_off(expected: Decimal, weighed: Decimal) -> bool {
    expected > Decimal::ZERO && (weighed - expected).abs() * Decimal::from(100) > expected * Decimal::from(WEIGHT_TOLERANCE_PERCENT)
}

/// What is verified leaves. ERP's delivery note follows from what actually went.
pub async fn hand_over(k: &Kernel, ctx: &Ctx, tx: &mut Tx, shipment: Uuid, h: &Handover) -> Result<Value> {
    let doc = k.get_tx(&sys(ctx), tx, "Shipment", shipment, true).await?;
    let status = doc.get("status").and_then(Value::as_str).unwrap_or("Picking");
    if matches!(status, "Handed Over" | "Cancelled") {
        return Err(Error::invalid(format!("this shipment is {status}")));
    }
    if h.carrier.trim().is_empty() || h.tracking.trim().is_empty() {
        return Err(Error::field("tracking", "a carrier and a tracking number are required"));
    }
    if h.label.trim() != h.tracking.trim() {
        return Err(Error::invalid("the label on the parcel is not this shipment's"));
    }
    let lines: Vec<Doc> = rows(&doc, "items");
    let going: Vec<(&Doc, Decimal)> = lines.iter().map(|l| (l, unsent(l))).filter(|(_, q)| *q > Decimal::ZERO).collect();
    if going.is_empty() {
        return Err(Error::invalid("nothing on this shipment has been verified since it last left"));
    }
    if let Some(weighed) = h.weighed_kg
        && weight_off(d(&doc, "expected_weight_kg"), weighed)
    {
        return Err(Error::invalid(format!(
            "it weighs {} kg and should weigh {} kg: open it and check",
            weighed.normalize(),
            d(&doc, "expected_weight_kg").normalize()
        )));
    }
    let warehouse = id(&doc, "warehouse").ok_or_else(|| Error::internal("a shipment without a warehouse"))?;
    let w = posting::must_manage(tx, ctx.tenant, warehouse).await?;
    let outbound = w.outbound_bin.ok_or_else(|| Error::invalid("this warehouse has no outbound bin"))?;
    let mut delivery = None;
    if let Some(owner) = id(&doc, "owner") {
        // A customer's own stock is not this company's to deliver in ERP.
        for (l, qty) in &going {
            let key = Key {
                bin: outbound,
                item: id(l, "item").unwrap_or_default(),
                owner: Some(owner),
                lot: s(l, "lot").unwrap_or_default().to_string(),
                status: "Available".into(),
                ..Default::default()
            };
            position::depart(tx, ctx, warehouse, Kind::Issue, &key, *qty, &Cause::source("Shipment", shipment)).await?;
        }
    } else {
        delivery = Some(post_delivery(k, ctx, tx, shipment, &going).await?);
    }
    for (l, _) in &going {
        let line = id(l, "id").ok_or_else(|| Error::internal("a line without an id"))?;
        let mut gone = Doc::new();
        gone.insert("handed_over_qty".into(), json!(d(l, "verified_qty")));
        k.set_child_values_tx(&sys(ctx), tx, "Shipment", "items", line, gone).await?;
    }
    let whole = lines.iter().all(|l| d(l, "verified_qty") == d(l, "qty"));
    // One shipment can leave in several parcels, and its fields say the last: the audit keeps each.
    audit::record(tx, ctx, "Shipment", shipment, "wms.handed_over", None, json!({"carrier": h.carrier, "tracking": h.tracking, "weighed_kg": h.weighed_kg, "lines": going.iter().map(|(l, q)| json!({"item": l.get("item"), "lot": l.get("lot"), "qty": q})).collect::<Vec<_>>()})).await?;
    let mut v = Doc::new();
    v.insert("status".into(), json!(if whole { "Handed Over" } else { "Partially Handed Over" }));
    v.insert("carrier".into(), json!(h.carrier));
    v.insert("tracking".into(), json!(h.tracking));
    v.insert("weighed_kg".into(), json!(h.weighed_kg));
    v.insert("expected_weight_kg".into(), json!(Decimal::ZERO));
    v.insert("label_checked".into(), json!(true));
    v.insert("handed_over_at".into(), json!(chrono::Utc::now().to_rfc3339()));
    v.insert("delivery_note".into(), json!(delivery));
    k.set_values_tx(&sys(ctx), tx, "Shipment", shipment, v).await?;
    Ok(json!({"delivery_note": delivery, "whole": whole}))
}

/// ERP's delivery note for what left, through the order's own mapping, cut down to what went.
async fn post_delivery(k: &Kernel, ctx: &Ctx, tx: &mut Tx, shipment: Uuid, going: &[(&Doc, Decimal)]) -> Result<Uuid> {
    let elevated = elevated(ctx);
    let mut by_order: BTreeMap<Uuid, Vec<(&Doc, Decimal)>> = BTreeMap::new();
    for &(l, qty) in going {
        by_order.entry(id(l, "sales_order").ok_or_else(|| Error::invalid("this line came from no order"))?).or_default().push((l, qty));
    }
    let mut last = None;
    for (order, lines) in by_order {
        let made = k.run_action_tx(&elevated, tx, "SalesOrder", order, "make_delivery_note", &json!({})).await?;
        let note: Uuid = made["id"].as_str().and_then(|v| v.parse().ok()).ok_or_else(|| Error::internal("the order made no delivery note"))?;
        let draft = k.get_tx(&elevated, tx, "DeliveryNote", note, true).await?;
        let mut items = Vec::new();
        for row in rows(&draft, "items") {
            let so_line = s(&row, "so_line").unwrap_or_default().to_string();
            for (l, qty) in lines.iter().filter(|(l, _)| s(l, "so_line") == Some(so_line.as_str())) {
                let mut r = row.clone();
                r.remove("id");
                r.insert("qty".into(), json!(qty));
                r.insert("batch_no".into(), l.get("lot").cloned().unwrap_or(Value::Null));
                r.insert("serial_no".into(), l.get("serial").cloned().unwrap_or(Value::Null));
                items.push(Value::Object(r));
            }
        }
        let mut patch = Doc::new();
        patch.insert("items".into(), Value::Array(items));
        k.update_tx(&elevated, tx, "DeliveryNote", note, patch, None).await?;
        k.submit_tx(&elevated, tx, "DeliveryNote", note, None).await?;
        audit::record(tx, ctx, "Shipment", shipment, "wms.delivered", None, json!({"delivery_note": note})).await?;
        last = Some(note);
    }
    last.ok_or_else(|| Error::invalid("nothing to deliver"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_parcel_may_be_a_little_off_but_not_a_unit_off() {
        let n = |v: i64| Decimal::from(v);
        assert!(!weight_off(n(100), n(104)));
        assert!(weight_off(n(100), n(106)));
        assert!(weight_off(n(100), n(90)));
        assert!(!weight_off(n(0), n(90)), "nothing to compare with");
    }
}
