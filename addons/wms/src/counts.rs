//! Counting. A count freezes its bins, counters do not see what is expected when it is blind, a
//! difference is counted twice, and someone other than the counter approves it. Original facts
//! stay: an approved difference becomes a new move, and the books follow through ERP.

use crate::position::{self, Cause, Key, Kind};
use crate::posting;
use ferrum_domain::util::{d, elevated, id, rows, s, sys};
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

async fn status(doc: &Doc) -> &str {
    doc.get("status").and_then(Value::as_str).unwrap_or("Draft")
}

/// Starts a count: the bins of its zone are frozen and what they hold now is written down.
pub async fn start(k: &Kernel, ctx: &Ctx, tx: &mut Tx, count: Uuid) -> Result<Value> {
    let doc = k.get_tx(&sys(ctx), tx, "CycleCount", count, true).await?;
    if status(&doc).await != "Draft" {
        return Err(Error::invalid("this count has started"));
    }
    let warehouse = id(&doc, "warehouse").ok_or_else(|| Error::internal("a count without a warehouse"))?;
    posting::must_manage(tx, ctx.tenant, warehouse).await?;
    let zone = id(&doc, "zone").ok_or_else(|| Error::field("zone", "choose what to count"))?;
    let busy: i64 = sqlx::query_scalar(
        "select count(*) from wms_position p join e_storage_bin b on b.tenant_id = p.tenant_id and b.id = p.bin_id \
         where p.tenant_id = $1 and b.zone = $2 and p.allocated > 0",
    )
    .bind(ctx.tenant)
    .bind(zone)
    .fetch_one(&mut **tx)
    .await?;
    if busy > 0 {
        return Err(Error::invalid(format!("{busy} positions there are promised to open tasks: finish those first")));
    }
    sqlx::query("update e_storage_bin set frozen_by = $3 where tenant_id = $1 and zone = $2 and frozen_by is null")
        .bind(ctx.tenant)
        .bind(zone)
        .bind(count)
        .execute(&mut **tx)
        .await?;
    let lines: Vec<Value> = sqlx::query_scalar(
        "select jsonb_build_object('bin', p.bin_id, 'item', p.item_id, 'lot', p.lot, 'serial', p.serial, 'owner', p.owner_id, 'expected_qty', sum(p.qty)) \
         from wms_position p join e_storage_bin b on b.tenant_id = p.tenant_id and b.id = p.bin_id \
         where p.tenant_id = $1 and b.zone = $2 and p.qty > 0 group by p.bin_id, p.item_id, p.lot, p.serial, p.owner_id order by p.bin_id",
    )
    .bind(ctx.tenant)
    .bind(zone)
    .fetch_all(&mut **tx)
    .await?;
    let n = lines.len();
    let mut patch = Doc::new();
    patch.insert("lines".into(), Value::Array(lines));
    k.update_tx(&elevated(ctx), tx, "CycleCount", count, patch, None).await?;
    let mut v = Doc::new();
    v.insert("status".into(), json!("Counting"));
    k.set_values_tx(&sys(ctx), tx, "CycleCount", count, v).await?;
    Ok(json!({"lines": n}))
}

#[derive(Debug, Deserialize)]
pub struct Entered {
    pub line: Uuid,
    pub qty: Decimal,
    /// The second count of a difference.
    #[serde(default)]
    pub recount: bool,
}

/// A counter's figure. What they are told back depends on the count: blind counts tell nothing.
pub async fn enter(k: &Kernel, ctx: &Ctx, tx: &mut Tx, count: Uuid, e: &Entered) -> Result<Value> {
    let doc = k.get_tx(&sys(ctx), tx, "CycleCount", count, true).await?;
    if status(&doc).await != "Counting" {
        return Err(Error::invalid("this count is not open"));
    }
    if e.qty < Decimal::ZERO {
        return Err(Error::field("qty", "cannot be negative"));
    }
    let lines = rows(&doc, "lines");
    let line = lines.iter().find(|l| id(l, "id") == Some(e.line)).ok_or_else(|| Error::invalid("that line is not on this count"))?;
    let mut v = Doc::new();
    v.insert(if e.recount { "recount_qty" } else { "counted_qty" }.to_string(), json!(e.qty));
    k.set_child_values_tx(&sys(ctx), tx, "CycleCount", "lines", e.line, v).await?;
    let mut who = Doc::new();
    who.insert("counted_by".into(), json!(ctx.email));
    k.set_values_tx(&sys(ctx), tx, "CycleCount", count, who).await?;
    let blind = doc.get("blind").and_then(Value::as_bool).unwrap_or(true);
    Ok(if blind { json!({"recorded": true}) } else { json!({"recorded": true, "expected": d(line, "expected_qty")}) })
}

/// Hands a count over for review. Any difference must have been counted twice, and both counts
/// must agree: that is what tells a miscount from a real difference.
pub async fn finish(k: &Kernel, ctx: &Ctx, tx: &mut Tx, count: Uuid) -> Result<Value> {
    let doc = k.get_tx(&sys(ctx), tx, "CycleCount", count, true).await?;
    if status(&doc).await != "Counting" {
        return Err(Error::invalid("this count is not open"));
    }
    let mut differences = 0;
    for l in rows(&doc, "lines") {
        if l.get("counted_qty").is_none_or(Value::is_null) {
            return Err(Error::invalid("every line has to be counted"));
        }
        let (expected, counted) = (d(&l, "expected_qty"), d(&l, "counted_qty"));
        if expected != counted {
            if l.get("recount_qty").is_none_or(Value::is_null) {
                return Err(Error::invalid("a difference has to be counted again"));
            }
            if d(&l, "recount_qty") != counted {
                return Err(Error::invalid("the two counts disagree: count it a third time and enter that as the count"));
            }
            differences += 1;
        }
        let mut v = Doc::new();
        v.insert("variance".into(), json!(counted - expected));
        k.set_child_values_tx(&sys(ctx), tx, "CycleCount", "lines", id(&l, "id").ok_or_else(|| Error::internal("a line without an id"))?, v).await?;
    }
    let mut v = Doc::new();
    v.insert("status".into(), json!("Review"));
    k.set_values_tx(&sys(ctx), tx, "CycleCount", count, v).await?;
    Ok(json!({"differences": differences}))
}

/// Applies a reviewed count. The counter cannot approve their own count. Shortages leave through
/// the outbound bin and surpluses arrive through the inbound bin, so ERP's books follow the same way
/// as for anything else, and the warehouse's stock account moves with them.
pub async fn approve(k: &Kernel, ctx: &Ctx, tx: &mut Tx, count: Uuid) -> Result<Value> {
    let doc = k.get_tx(&sys(ctx), tx, "CycleCount", count, true).await?;
    if status(&doc).await != "Review" {
        return Err(Error::invalid("this count is not waiting for review"));
    }
    if s(&doc, "counted_by") == Some(ctx.email.as_str()) {
        return Err(Error::Forbidden("someone other than the counter has to approve this".into()));
    }
    let reason = s(&doc, "reason").unwrap_or_default().to_string();
    let warehouse = id(&doc, "warehouse").ok_or_else(|| Error::internal("a count without a warehouse"))?;
    let w = posting::must_manage(tx, ctx.tenant, warehouse).await?;
    let company = id(&doc, "company").ok_or_else(|| Error::internal("a count without a company"))?;
    let (outbound, inbound) =
        (w.outbound_bin.ok_or_else(|| Error::invalid("no outbound bin"))?, w.inbound_bin.ok_or_else(|| Error::invalid("no inbound bin"))?);
    let lines: Vec<Doc> = rows(&doc, "lines").into_iter().filter(|l| !d(l, "variance").is_zero()).collect();
    if !lines.is_empty() && reason.trim().is_empty() {
        return Err(Error::field("reason", "say why stock is being adjusted"));
    }
    let (mut issues, mut receipts) = (Vec::new(), Vec::new());
    for l in &lines {
        let (item, variance) = (id(l, "item").ok_or_else(|| Error::internal("a line without an item"))?, d(l, "variance"));
        let bin = id(l, "bin").ok_or_else(|| Error::internal("a line without a bin"))?;
        let (lot, serial) = (s(l, "lot").unwrap_or_default().to_string(), s(l, "serial").unwrap_or_default().to_string());
        let owner = id(l, "owner");
        let key = Key { bin, item, owner, lot: lot.clone(), serial: serial.clone(), unit: None, status: "Available".into() };
        let cause = Cause { source: Some(("CycleCount".into(), count)), reason: Some(reason.clone()), ..Default::default() };
        if owner.is_some() {
            // A customer's stock has no ERP side: the count is the correction.
            if variance < Decimal::ZERO {
                position::depart(tx, ctx, warehouse, Kind::Count, &key, -variance, &cause).await?;
            } else {
                position::arrive(tx, ctx, warehouse, Kind::Count, &key, variance, None, &cause).await?;
            }
        } else if variance < Decimal::ZERO {
            let out = Key { bin: outbound, ..key.clone() };
            position::transfer(tx, ctx, warehouse, Kind::Count, &key, &out, -variance, Decimal::ZERO, &cause).await?;
            issues.push(json!({"item": item, "qty": -variance, "source_warehouse": warehouse, "batch_no": lot, "serial_no": serial}));
        } else {
            receipts.push((key, variance, json!({"item": item, "qty": variance, "target_warehouse": warehouse, "batch_no": lot, "serial_no": serial})));
        }
    }
    let elevated = elevated(ctx);
    if !issues.is_empty() {
        let doc = json!({"company": company, "purpose": "Material Issue", "items": issues, "remarks": reason});
        let made = k.insert_tx(&elevated, tx, "StockEntry", doc.as_object().cloned().unwrap_or_default()).await?;
        k.submit_tx(&elevated, tx, "StockEntry", id(&made, "id").ok_or_else(|| Error::internal("no id"))?, None).await?;
    }
    if !receipts.is_empty() {
        let items: Vec<Value> = receipts.iter().map(|r| r.2.clone()).collect();
        let doc = json!({"company": company, "purpose": "Material Receipt", "items": items, "remarks": reason});
        let made = k.insert_tx(&elevated, tx, "StockEntry", doc.as_object().cloned().unwrap_or_default()).await?;
        k.submit_tx(&elevated, tx, "StockEntry", id(&made, "id").ok_or_else(|| Error::internal("no id"))?, None).await?;
        for (key, qty, _) in receipts {
            let status: Option<String> = sqlx::query_scalar::<_, Option<String>>("select wms_receive_status from e_item where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(key.item)
                .fetch_one(&mut **tx)
                .await?;
            let from = Key { bin: inbound, status: status.unwrap_or_else(|| "Available".into()), ..key.clone() };
            position::transfer(
                tx,
                ctx,
                warehouse,
                Kind::Count,
                &from,
                &key,
                qty,
                Decimal::ZERO,
                &Cause { source: Some(("CycleCount".into(), count)), reason: Some(reason.clone()), ..Default::default() },
            )
            .await?;
        }
    }
    unfreeze(tx, ctx.tenant, count).await?;
    let mut v = Doc::new();
    v.insert("status".into(), json!("Applied"));
    v.insert("approved_by".into(), json!(ctx.email));
    k.set_values_tx(&sys(ctx), tx, "CycleCount", count, v).await?;
    Ok(json!({"adjusted": lines.len()}))
}

async fn unfreeze(tx: &mut Tx, tenant: Uuid, count: Uuid) -> Result<()> {
    sqlx::query("update e_storage_bin set frozen_by = null where tenant_id = $1 and frozen_by = $2").bind(tenant).bind(count).execute(&mut **tx).await?;
    Ok(())
}
