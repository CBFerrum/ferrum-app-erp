//! Receiving. What was expected, what arrived and in what state are kept apart until a person
//! closes the shipment: scans are repeatable, over-receipt needs a manager, damaged goods go to
//! quarantine and unknown goods stay out of stock. Closing posts ERP's purchase receipt, which puts
//! the stock at the inbound bin, and opens putaway.

use crate::position::{self, Cause, Key, Kind};
use crate::posting;
use crate::slotting;
use crate::tasks::{self, NewTask};
use ferrum_domain::util::{d, elevated, id, rows, s, sys};
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Tx, audit};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct Scan {
    /// A repeated scan of this id does nothing.
    pub scan_id: Uuid,
    /// What was scanned: a lot, an item's barcode, or its code.
    pub code: String,
    #[serde(default)]
    pub qty: Decimal,
    #[serde(default)]
    pub damaged: Decimal,
}

#[derive(Debug, Deserialize)]
pub struct Unknown {
    pub description: String,
    pub qty: Decimal,
}

async fn over_allowed(tx: &mut Tx, tenant: Uuid, warehouse: Uuid) -> Result<Decimal> {
    let pct: Option<Option<Decimal>> = sqlx::query_scalar("select over_receipt_percent from e_managed_warehouse where tenant_id = $1 and warehouse = $2").bind(tenant).bind(warehouse).fetch_optional(&mut **tx).await?;
    Ok(pct.flatten().unwrap_or_default())
}

/// The line a scan means. A lot names its own line, and an item the first of its lines still waiting.
fn line_for<'a>(lines: &'a [Doc], item: Option<Uuid>, code: &str) -> Option<&'a Doc> {
    let lot = lines.iter().find(|l| s(l, "lot").is_some_and(|x| x.eq_ignore_ascii_case(code)));
    let of_item = || lines.iter().filter(|l| item.is_some() && id(l, "item") == item);
    lot.or_else(|| of_item().find(|l| d(l, "received_qty") + d(l, "damaged_qty") < d(l, "expected_qty"))).or_else(|| of_item().next())
}

/// Adds a scan to a line. Repeats are harmless, so a scanner may resend after a lost answer.
pub async fn scan(k: &Kernel, ctx: &Ctx, tx: &mut Tx, shipment: Uuid, s: &Scan) -> Result<Value> {
    if s.qty < Decimal::ZERO || s.damaged < Decimal::ZERO || (s.qty + s.damaged).is_zero() {
        return Err(Error::field("qty", "scan a quantity"));
    }
    let doc = k.get_tx(&sys(ctx), tx, "InboundShipment", shipment, true).await?;
    let status = doc.get("status").and_then(Value::as_str).unwrap_or("Expected");
    if !matches!(status, "Expected" | "Receiving") {
        return Err(Error::invalid(format!("this shipment is {status}")));
    }
    let first = sqlx::query("insert into wms_scan (tenant_id, id) values ($1, $2) on conflict do nothing").bind(ctx.tenant).bind(s.scan_id).execute(&mut **tx).await?.rows_affected() > 0;
    if !first {
        return Ok(json!({"repeated": true}));
    }
    let warehouse = id(&doc, "warehouse").ok_or_else(|| Error::internal("a shipment without a warehouse"))?;
    let item = slotting::item_by_scan(tx, ctx.tenant, &s.code).await?;
    let lines = rows(&doc, "items");
    let line = line_for(&lines, item, &s.code).ok_or_else(|| Error::invalid(format!("{} is not on this shipment", s.code)))?;
    let line_id = id(line, "id").ok_or_else(|| Error::internal("a line without an id"))?;
    let (received, damaged) = (d(line, "received_qty") + s.qty, d(line, "damaged_qty") + s.damaged);
    let limit = d(line, "expected_qty") * (Decimal::ONE + over_allowed(tx, ctx.tenant, warehouse).await? / Decimal::from(100));
    if received + damaged > limit && !ctx.is_admin && !ctx.has_role("Warehouse Manager") {
        return Err(Error::Forbidden(format!("{} is more than was expected: a manager has to accept it", (received + damaged).normalize())));
    }
    let mut v = Doc::new();
    v.insert("received_qty".into(), json!(received));
    v.insert("damaged_qty".into(), json!(damaged));
    k.set_child_values_tx(&sys(ctx), tx, "InboundShipment", "items", line_id, v).await?;
    let mut v = Doc::new();
    v.insert("status".into(), json!("Receiving"));
    k.set_values_tx(&sys(ctx), tx, "InboundShipment", shipment, v).await?;
    Ok(json!({"received": received, "damaged": damaged, "expected": d(line, "expected_qty")}))
}

/// Goods nobody expected and nobody can name stay off the books: they cannot be stock until someone
/// says what they are. Receiving carries on, and closing the shipment leaves it as a difference.
pub async fn unknown(k: &Kernel, ctx: &Ctx, tx: &mut Tx, shipment: Uuid, u: &Unknown) -> Result<()> {
    if u.description.trim().is_empty() {
        return Err(Error::field("description", "say what it looks like"));
    }
    let doc = k.get_tx(&sys(ctx), tx, "InboundShipment", shipment, true).await?;
    let notes = s(&doc, "notes").unwrap_or_default();
    let line = format!("Unknown goods: {} × {}", u.qty.normalize(), u.description.trim());
    let mut v = Doc::new();
    v.insert("notes".into(), json!(if notes.is_empty() { line.clone() } else { format!("{notes}\n{line}") }));
    k.set_values_tx(&sys(ctx), tx, "InboundShipment", shipment, v).await?;
    audit::record(tx, ctx, "InboundShipment", shipment, "wms.unknown_goods", None, json!({"description": u.description, "qty": u.qty})).await
}

/// A batch record for a lot, if the receipt is the first anyone has heard of it.
async fn ensure_batch(k: &Kernel, ctx: &Ctx, tx: &mut Tx, item: Uuid, lot: &str, expiry: Option<&Value>) -> Result<()> {
    let have: Option<i32> = sqlx::query_scalar("select 1 from e_batch where tenant_id = $1 and item = $2 and batch_id = $3").bind(ctx.tenant).bind(item).bind(lot).fetch_optional(&mut **tx).await?;
    if have.is_none() {
        let mut b = Doc::new();
        b.insert("batch_id".into(), json!(lot));
        b.insert("item".into(), json!(item));
        if let Some(e) = expiry.filter(|e| !e.is_null()) {
            b.insert("expiry_date".into(), e.clone());
        }
        k.insert_tx(&elevated(ctx), tx, "Batch", b).await?;
    }
    Ok(())
}

/// Closes a shipment: ERP receives what came, damaged stock goes to quarantine, and what is good
/// is sent to its bin. For a customer's stock nothing reaches ERP: it is theirs.
pub async fn close(k: &Kernel, ctx: &Ctx, tx: &mut Tx, shipment: Uuid) -> Result<Value> {
    let doc = k.get_tx(&sys(ctx), tx, "InboundShipment", shipment, true).await?;
    // Closed once, whatever it closed as: closing again would receive it all a second time.
    let status = doc.get("status").and_then(Value::as_str).unwrap_or("Expected");
    if !matches!(status, "Expected" | "Receiving") {
        return Err(Error::invalid("this shipment is already received"));
    }
    let warehouse = id(&doc, "warehouse").ok_or_else(|| Error::internal("a shipment without a warehouse"))?;
    let w = posting::must_manage(tx, ctx.tenant, warehouse).await?;
    let inbound = w.inbound_bin.ok_or_else(|| Error::invalid("this warehouse has no inbound bin"))?;
    let lines = rows(&doc, "items");
    let owner = id(&doc, "owner");
    let mut short = false;
    let mut erp_items = Vec::new();
    for l in &lines {
        let qty = d(l, "received_qty") + d(l, "damaged_qty");
        short |= qty < d(l, "expected_qty");
        if qty.is_zero() {
            continue;
        }
        let item = id(l, "item").ok_or_else(|| Error::internal("a line without an item"))?;
        let lot = s(l, "lot").unwrap_or_default().to_string();
        if !lot.is_empty() {
            ensure_batch(k, ctx, tx, item, &lot, l.get("expiry")).await?;
        }
        if owner.is_some() {
            let status = "Available".to_string();
            let key = Key { bin: inbound, item, owner, lot: lot.clone(), status, ..Default::default() };
            let expiry = l.get("expiry").and_then(Value::as_str).and_then(|e| e.parse().ok());
            position::arrive(tx, ctx, warehouse, Kind::Receive, &key, qty, expiry, &Cause::source("InboundShipment", shipment)).await?;
        } else {
            erp_items.push(json!({"item": item, "qty": qty, "rate": d(l, "rate"), "warehouse": warehouse, "batch_no": lot, "purchase_order": l.get("purchase_order"), "po_line": l.get("po_line")}));
        }
    }
    let mut receipt_id = None;
    if !erp_items.is_empty() {
        let supplier = id(&doc, "supplier").ok_or_else(|| Error::field("supplier", "a company's receipt needs its supplier"))?;
        let company = id(&doc, "company").ok_or_else(|| Error::internal("a shipment without a company"))?;
        let receipt = json!({"company": company, "supplier": supplier, "set_warehouse": warehouse, "supplier_delivery_note": doc.get("reference"), "items": erp_items});
        let elevated = elevated(ctx);
        let made = k.insert_tx(&elevated, tx, "PurchaseReceipt", receipt.as_object().cloned().unwrap_or_default()).await?;
        let made_id = id(&made, "id").ok_or_else(|| Error::internal("a receipt without an id"))?;
        k.submit_tx(&elevated, tx, "PurchaseReceipt", made_id, None).await?;
        receipt_id = Some(made_id);
    }
    // The mirror placed everything at the inbound bin. Damaged stock leaves it now.
    for l in &lines {
        let damaged = d(l, "damaged_qty");
        if damaged.is_zero() {
            continue;
        }
        let item = id(l, "item").ok_or_else(|| Error::internal("a line without an item"))?;
        let quarantine = w.quarantine_bin.ok_or_else(|| Error::invalid("this warehouse has no quarantine bin"))?;
        let status = if owner.is_some() { "Available".to_string() } else { sqlx::query_scalar::<_, Option<String>>("select wms_receive_status from e_item where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(item).fetch_one(&mut **tx).await?.unwrap_or_else(|| "Available".into()) };
        let from = Key { bin: inbound, item, owner, lot: s(l, "lot").unwrap_or_default().to_string(), status, ..Default::default() };
        let to = Key { bin: quarantine, status: "Damaged".into(), ..from.clone() };
        position::transfer(tx, ctx, warehouse, Kind::Status, &from, &to, damaged, Decimal::ZERO, &Cause { reason: Some("Damaged on receipt".into()), source: Some(("InboundShipment".into(), shipment)), ..Default::default() }).await?;
    }
    // What is good goes to a bin. What must be inspected first waits where it is.
    let mut made: BTreeMap<String, u32> = BTreeMap::new();
    for l in &lines {
        let good = d(l, "received_qty");
        if good.is_zero() {
            continue;
        }
        let item = id(l, "item").ok_or_else(|| Error::internal("a line without an item"))?;
        let status: Option<String> = sqlx::query_scalar::<_, Option<String>>("select wms_receive_status from e_item where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(item).fetch_one(&mut **tx).await?;
        let status = if owner.is_some() { "Available".to_string() } else { status.unwrap_or_else(|| "Available".into()) };
        if status != "Available" {
            *made.entry("waiting".into()).or_default() += 1;
            continue;
        }
        let to = slotting::suggest(tx, ctx.tenant, warehouse, item, good).await?;
        let lot = s(l, "lot").unwrap_or_default().to_string();
        tasks::create(k, ctx, tx, NewTask { warehouse, kind: "Putaway", item, qty: good, lot, owner, from_bin: inbound, to_bin: to, status, ..Default::default() }).await?;
        *made.entry(if to.is_some() { "putaway" } else { "no_bin" }.to_string()).or_default() += 1;
    }
    let notes = doc.get("notes").and_then(Value::as_str).unwrap_or_default();
    let status = if short || notes.contains("Unknown goods") { "Discrepancy" } else { "Received" };
    let mut v = Doc::new();
    v.insert("status".into(), json!(status));
    if let Some(r) = receipt_id {
        v.insert("purchase_receipt".into(), json!(r));
    }
    k.set_values_tx(&sys(ctx), tx, "InboundShipment", shipment, v).await?;
    Ok(json!({"status": status, "purchase_receipt": receipt_id, "tasks": made}))
}

/// A manager says what became of a difference. The stock was settled at closing: this is the record
/// that somebody looked, and what they found.
pub async fn settle(k: &Kernel, ctx: &Ctx, tx: &mut Tx, shipment: Uuid, note: &str) -> Result<()> {
    if note.trim().is_empty() {
        return Err(Error::field("note", "say what became of it"));
    }
    let doc = k.get_tx(&sys(ctx), tx, "InboundShipment", shipment, true).await?;
    if s(&doc, "status") != Some("Discrepancy") {
        return Err(Error::invalid("this shipment has no difference to settle"));
    }
    let notes = s(&doc, "notes").unwrap_or_default();
    let line = format!("Settled: {}", note.trim());
    let mut v = Doc::new();
    v.insert("notes".into(), json!(if notes.is_empty() { line.clone() } else { format!("{notes}\n{line}") }));
    v.insert("status".into(), json!("Received"));
    k.set_values_tx(&sys(ctx), tx, "InboundShipment", shipment, v).await?;
    audit::record(tx, ctx, "InboundShipment", shipment, "wms.difference_settled", None, json!({"note": note})).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(item: Uuid, lot: &str, expected: i64, received: i64) -> Doc {
        json!({"item": item, "lot": lot, "expected_qty": expected, "received_qty": received}).as_object().cloned().unwrap()
    }

    #[test]
    fn a_scan_finds_the_line_it_means() {
        let (a, b) = (Uuid::now_v7(), Uuid::now_v7());
        let lines = [line(a, "M-1", 10, 10), line(a, "M-2", 10, 0), line(b, "", 5, 0)];
        assert_eq!(s(line_for(&lines, None, "m-2").unwrap(), "lot"), Some("M-2"), "a lot, however it was typed");
        assert_eq!(s(line_for(&lines, Some(a), "BOLT").unwrap(), "lot"), Some("M-2"), "an item goes to its first line still waiting");
        let full = [line(a, "", 10, 10)];
        assert!(line_for(&full, Some(a), "BOLT").is_some(), "more of what already came is still that line, for a manager to accept");
        assert!(line_for(&lines, None, "NUT").is_none(), "a scan matches or it does not: part of a name is not a match");
    }
}
