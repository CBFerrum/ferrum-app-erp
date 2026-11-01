//! Returns, status changes and customers' stock. A return comes back into quarantine and stays there
//! until someone decides what it is. Held stock is simply stock whose status allocation will not
//! touch, so a hold cannot be forgotten by a code path.

use crate::position::{self, Cause, Key, Kind};
use crate::posting;
use crate::slotting;
use crate::tasks::{self, NewTask};
use ferrum_domain::util::{d, elevated, id, rows, s, sys};
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

/// Takes returned goods in: ERP's stock rises at the inbound bin, and they go straight to quarantine,
/// out of allocation's reach.
pub async fn receive(k: &Kernel, ctx: &Ctx, tx: &mut Tx, rma: Uuid) -> Result<Value> {
    let doc = k.get_tx(&sys(ctx), tx, "ReturnAuthorization", rma, true).await?;
    if doc.get("status").and_then(Value::as_str) != Some("Authorized") {
        return Err(Error::invalid("this return has been received"));
    }
    let warehouse = id(&doc, "warehouse").ok_or_else(|| Error::internal("a return without a warehouse"))?;
    let w = posting::must_manage(tx, ctx.tenant, warehouse).await?;
    let (inbound, quarantine) = (w.inbound_bin.ok_or_else(|| Error::invalid("no inbound bin"))?, w.quarantine_bin.ok_or_else(|| Error::invalid("no quarantine bin"))?);
    let lines = rows(&doc, "items");
    let company = id(&doc, "company").ok_or_else(|| Error::internal("a return without a company"))?;
    let items: Vec<Value> = lines.iter().map(|l| json!({"item": l.get("item"), "qty": d(l, "qty"), "basic_rate": d(l, "rate"), "target_warehouse": warehouse, "batch_no": l.get("lot"), "serial_no": l.get("serial")})).collect();
    let elevated = elevated(ctx);
    let entry = json!({"company": company, "purpose": "Material Receipt", "items": items, "remarks": format!("Return {}", s(&doc, "number").unwrap_or_default())});
    let made = k.insert_tx(&elevated, tx, "StockEntry", entry.as_object().cloned().unwrap_or_default()).await?;
    k.submit_tx(&elevated, tx, "StockEntry", id(&made, "id").ok_or_else(|| Error::internal("no id"))?, None).await?;
    for l in &lines {
        let item = id(l, "item").ok_or_else(|| Error::internal("a line without an item"))?;
        let status: Option<String> = sqlx::query_scalar::<_, Option<String>>("select wms_receive_status from e_item where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(item).fetch_one(&mut **tx).await?;
        let from = Key { bin: inbound, item, lot: s(l, "lot").unwrap_or_default().to_string(), serial: s(l, "serial").unwrap_or_default().to_string(), status: status.unwrap_or_else(|| "Available".into()), ..Default::default() };
        let to = Key { bin: quarantine, status: "Quarantine".into(), ..from.clone() };
        position::transfer(tx, ctx, warehouse, Kind::Return, &from, &to, d(l, "qty"), Decimal::ZERO, &Cause { source: Some(("ReturnAuthorization".into(), rma)), reason: s(l, "reason").map(str::to_string), ..Default::default() }).await?;
        let mut v = Doc::new();
        v.insert("received_qty".into(), json!(d(l, "qty")));
        k.set_child_values_tx(&sys(ctx), tx, "ReturnAuthorization", "items", id(l, "id").ok_or_else(|| Error::internal("a line without an id"))?, v).await?;
    }
    let mut v = Doc::new();
    v.insert("status".into(), json!("Received"));
    k.set_values_tx(&sys(ctx), tx, "ReturnAuthorization", rma, v).await?;
    Ok(json!({"received": lines.len()}))
}

#[derive(Debug, Deserialize)]
pub struct Disposition {
    pub line: Uuid,
    pub decision: String,
    #[serde(default)]
    pub reason: String,
}

/// Decides one line. Restocking opens a putaway; scrapping leaves through ERP; repair and supplier
/// return stay on hold, off allocation, for whoever carries them out.
pub async fn dispose(k: &Kernel, ctx: &Ctx, tx: &mut Tx, rma: Uuid, c: &Disposition) -> Result<Value> {
    let doc = k.get_tx(&sys(ctx), tx, "ReturnAuthorization", rma, true).await?;
    if doc.get("status").and_then(Value::as_str) != Some("Received") {
        return Err(Error::invalid("this return is not waiting for a decision"));
    }
    let warehouse = id(&doc, "warehouse").ok_or_else(|| Error::internal("a return without a warehouse"))?;
    let w = posting::must_manage(tx, ctx.tenant, warehouse).await?;
    let quarantine = w.quarantine_bin.ok_or_else(|| Error::invalid("no quarantine bin"))?;
    let lines = rows(&doc, "items");
    let line = lines.iter().find(|l| id(l, "id") == Some(c.line)).ok_or_else(|| Error::invalid("that line is not on this return"))?;
    if !matches!(s(line, "disposition"), None | Some("Undecided" | "")) {
        return Err(Error::invalid("that line has been decided"));
    }
    let item = id(line, "item").ok_or_else(|| Error::internal("a line without an item"))?;
    let qty = d(line, "received_qty");
    let held = Key { bin: quarantine, item, lot: s(line, "lot").unwrap_or_default().to_string(), serial: s(line, "serial").unwrap_or_default().to_string(), status: "Quarantine".into(), ..Default::default() };
    let cause = Cause { source: Some(("ReturnAuthorization".into(), rma)), reason: Some(c.reason.clone()), ..Default::default() };
    let elevated = elevated(ctx);
    let mut next = Value::Null;
    match c.decision.as_str() {
        "Restock" => {
            let good = Key { status: "Available".into(), ..held.clone() };
            position::transfer(tx, ctx, warehouse, Kind::Status, &held, &good, qty, Decimal::ZERO, &cause).await?;
            let to = slotting::suggest(tx, ctx.tenant, warehouse, item, qty).await?;
            next = json!(tasks::create(k, ctx, tx, NewTask { warehouse, kind: "Putaway", item, qty, lot: held.lot.clone(), serial: held.serial.clone(), from_bin: quarantine, to_bin: to, status: "Available".into(), ..Default::default() }).await?);
        }
        "Scrap" => {
            let outbound = w.outbound_bin.ok_or_else(|| Error::invalid("no outbound bin"))?;
            let out = Key { bin: outbound, status: "Available".into(), ..held.clone() };
            position::transfer(tx, ctx, warehouse, Kind::Status, &held, &out, qty, Decimal::ZERO, &cause).await?;
            let company = id(&doc, "company").ok_or_else(|| Error::internal("a return without a company"))?;
            let entry = json!({"company": company, "purpose": "Material Issue", "items": [{"item": item, "qty": qty, "source_warehouse": warehouse, "batch_no": held.lot, "serial_no": held.serial}], "remarks": format!("Scrapped: {}", c.reason)});
            let made = k.insert_tx(&elevated, tx, "StockEntry", entry.as_object().cloned().unwrap_or_default()).await?;
            k.submit_tx(&elevated, tx, "StockEntry", id(&made, "id").ok_or_else(|| Error::internal("no id"))?, None).await?;
        }
        "Repair" | "Return to Supplier" => {
            let hold = Key { status: "Hold".into(), ..held.clone() };
            position::transfer(tx, ctx, warehouse, Kind::Status, &held, &hold, qty, Decimal::ZERO, &cause).await?;
        }
        other => return Err(Error::field("decision", format!("{other} is not a decision"))),
    }
    let mut v = Doc::new();
    v.insert("disposition".into(), json!(c.decision));
    k.set_child_values_tx(&sys(ctx), tx, "ReturnAuthorization", "items", c.line, v).await?;
    let open = lines.iter().filter(|l| id(l, "id") != Some(c.line)).any(|l| matches!(s(l, "disposition"), None | Some("Undecided" | "")));
    if !open {
        let mut v = Doc::new();
        v.insert("status".into(), json!("Dispositioned"));
        k.set_values_tx(&sys(ctx), tx, "ReturnAuthorization", rma, v).await?;
    }
    Ok(json!({"task": next}))
}

#[derive(Debug, Deserialize)]
pub struct StatusChange {
    pub position: Uuid,
    pub to: String,
    pub reason: String,
    #[serde(default)]
    pub qty: Option<Decimal>,
}

/// Changes what stock may be used for. Allocated stock is taken back first by whoever holds the
/// task: a hold must not silently strand a pick. Release from a hold is a separate privilege.
pub async fn change_status(tx: &mut Tx, ctx: &Ctx, c: &StatusChange) -> Result<()> {
    if c.reason.trim().is_empty() {
        return Err(Error::field("reason", "say why"));
    }
    if !matches!(c.to.as_str(), "Available" | "Inspection" | "Quarantine" | "Hold" | "Damaged") {
        return Err(Error::field("to", "not a status"));
    }
    #[derive(sqlx::FromRow)]
    struct P {
        warehouse_id: Uuid,
        bin_id: Uuid,
        item_id: Uuid,
        owner_id: Option<Uuid>,
        lot: String,
        serial: String,
        unit_id: Option<Uuid>,
        status: String,
        qty: Decimal,
        allocated: Decimal,
    }
    let p: Option<P> = sqlx::query_as("select warehouse_id, bin_id, item_id, owner_id, lot, serial, unit_id, status, qty, allocated from wms_position where tenant_id = $1 and id = $2 for update")
        .bind(ctx.tenant)
        .bind(c.position)
        .fetch_optional(&mut **tx)
        .await?;
    let p = p.ok_or_else(|| Error::NotFound("position".into()))?;
    if p.status != "Available" && c.to == "Available" && !ctx.is_admin && !ctx.has_role("Warehouse Manager") && !ctx.has_role("Quality Manager") {
        return Err(Error::Forbidden("releasing held stock takes a manager".into()));
    }
    let qty = c.qty.unwrap_or(p.qty);
    if qty > p.qty - p.allocated {
        return Err(Error::invalid("that much of it is promised to open tasks"));
    }
    let from = Key { bin: p.bin_id, item: p.item_id, owner: p.owner_id, lot: p.lot, serial: p.serial, unit: p.unit_id, status: p.status };
    let to = Key { status: c.to.clone(), ..from.clone() };
    position::transfer(tx, ctx, p.warehouse_id, Kind::Status, &from, &to, qty, Decimal::ZERO, &Cause::why(&c.reason)).await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ManualMove {
    pub warehouse: Uuid,
    pub item: Uuid,
    pub from_bin: String,
    pub to_bin: String,
    pub qty: Decimal,
    #[serde(default)]
    pub lot: String,
    #[serde(default)]
    pub serial: String,
    #[serde(default)]
    pub owner: Option<Uuid>,
    pub reason: String,
    #[serde(default)]
    pub status: Option<String>,
}

/// A person moved stock with no task, as people do. It is recorded with who and why, and refused if
/// it would take what a task is counting on: the task must be cancelled first.
pub async fn manual_move(tx: &mut Tx, ctx: &Ctx, m: &ManualMove) -> Result<()> {
    if m.reason.trim().is_empty() {
        return Err(Error::field("reason", "say why"));
    }
    let from = slotting::bin_by_code(tx, ctx.tenant, m.warehouse, &m.from_bin).await?;
    let to = slotting::bin_by_code(tx, ctx.tenant, m.warehouse, &m.to_bin).await?;
    slotting::check(tx, ctx.tenant, &to, m.item, m.qty).await?;
    let status = m.status.clone().unwrap_or_else(|| "Available".into());
    let source = Key { bin: from.id, item: m.item, owner: m.owner, lot: m.lot.clone(), serial: m.serial.clone(), unit: None, status };
    let dest = Key { bin: to.id, ..source.clone() };
    position::transfer(tx, ctx, m.warehouse, Kind::Move, &source, &dest, m.qty, Decimal::ZERO, &Cause::why(&m.reason)).await?;
    Ok(())
}
