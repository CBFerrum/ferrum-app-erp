//! Stock entries: receipts, issues, transfers, opening stock and (with the manufacturing package)
//! manufacture entries that consume materials and receive the output at its actual cost.

use crate::ledger::{self, Entry, Line};
use crate::stock::{self, MoveIn, Posted, parse_serials};
use crate::util::{Company, b, company, d, date, get, id, money, posting_ts, round, rows, s, set, sys};
use async_trait::async_trait;
use ferrum_domain::flow::{create_for_user, created};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

const RECEIPT: &str = "Material Receipt";
const ISSUE: &str = "Material Issue";
const TRANSFER: &str = "Material Transfer";
const OPENING: &str = "Opening Stock";
pub const MANUFACTURE: &str = "Manufacture";

fn purpose(doc: &Doc) -> &str {
    s(doc, "purpose").unwrap_or(RECEIPT)
}

/// A manufacture line is an output when it only has a target warehouse.
fn is_output(r: &Doc) -> bool {
    id(r, "target_warehouse").is_some() && id(r, "source_warehouse").is_none()
}

async fn stock_account(hc: &mut HookCtx<'_>, co: &Company, warehouse: Uuid, cache: &mut HashMap<Uuid, Uuid>) -> Result<Uuid> {
    if let Some(a) = cache.get(&warehouse) {
        return Ok(*a);
    }
    let w = get(hc.kernel, hc.ctx, hc.tx, "Warehouse", warehouse).await?;
    let a = match id(&w, "account") {
        Some(a) => a,
        None => co.require("default_inventory_account", "default inventory account")?,
    };
    cache.insert(warehouse, a);
    Ok(a)
}

pub struct StockEntryHook;

#[async_trait]
impl DocHook for StockEntryHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let p = purpose(doc).to_string();
        let precision = match id(doc, "company") {
            Some(c) => company(hc.kernel, hc.ctx, hc.tx, c).await?.precision,
            None => 2,
        };
        if p == MANUFACTURE && !hc.ctx.schema.packages.iter().any(|x| x == "manufacturing") {
            return Err(Error::field("purpose", "manufacture entries need the manufacturing package"));
        }
        let mut lines = rows(doc, "items");
        let mut total = Decimal::ZERO;
        for r in lines.iter_mut() {
            if p == MANUFACTURE {
                set(r, "mfg_is_finished_item", is_output(r));
            } else {
                r.remove("mfg_is_finished_item");
            }
            if matches!(p.as_str(), RECEIPT | OPENING) && r.get("basic_rate").is_some_and(|v| !v.is_null()) {
                let amount = round(d(r, "qty") * d(r, "basic_rate"), precision);
                set(r, "amount", money(amount));
            }
            total += d(r, "amount");
        }
        set(doc, "items", serde_json::Value::Array(lines.into_iter().map(serde_json::Value::Object).collect()));
        if d(doc, "docstatus").is_zero() {
            set(doc, "total_amount", money(total));
        }
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let p = purpose(doc);
        let mut errors = Vec::new();
        let lines = rows(doc, "items");
        for (i, r) in lines.iter().enumerate() {
            let (src, tgt) = (id(r, "source_warehouse"), id(r, "target_warehouse"));
            let mut need = |field: &str, ok: bool, msg: &str| {
                if !ok {
                    errors.push(FieldError::new(field, msg).in_row("items", i));
                }
            };
            match p {
                RECEIPT | OPENING => {
                    need("target_warehouse", tgt.is_some(), "is required for receipts");
                    need("source_warehouse", src.is_none(), "must be empty for receipts");
                    need("basic_rate", r.get("basic_rate").is_some_and(|v| !v.is_null()), "is required for receipts");
                }
                ISSUE => {
                    need("source_warehouse", src.is_some(), "is required for issues");
                    need("target_warehouse", tgt.is_none(), "must be empty for issues");
                }
                TRANSFER => {
                    need("source_warehouse", src.is_some(), "is required for transfers");
                    need("target_warehouse", tgt.is_some(), "is required for transfers");
                    need("target_warehouse", src.is_none() || src != tgt, "must differ from the source warehouse");
                }
                MANUFACTURE => {
                    need("source_warehouse", src.is_some() != tgt.is_some(), "set either a source (consumed) or a target (produced) warehouse");
                }
                other => errors.push(FieldError::new("purpose", format!("unknown purpose {other}"))),
            }
        }
        if p == TRANSFER && id(doc, "transfer_of").is_some() {
            for (i, r) in lines.iter().enumerate() {
                let short = d(r, "short_qty");
                if s(r, "transfer_line").is_none() {
                    errors.push(FieldError::new("item", "a receipt of a transfer takes only the transfer's lines").in_row("items", i));
                }
                if short > Decimal::ZERO && s(r, "short_reason").is_none() {
                    errors.push(FieldError::new("short_reason", "say why it is short or damaged").in_row("items", i));
                }
                if short > Decimal::ZERO && s(r, "short_action") == Some("Claimed") && id(r, "short_account").is_none() {
                    errors.push(FieldError::new("short_account", "is where a claim is owed: set it").in_row("items", i));
                }
            }
        } else if lines.iter().any(|r| d(r, "short_qty") > Decimal::ZERO) {
            errors.push(FieldError::new("items", "only a receipt of a transfer can be short"));
        }
        if p == MANUFACTURE {
            if !lines.iter().any(is_output) {
                errors.push(FieldError::new("items", "a manufacture entry needs at least one produced line (target warehouse only)"));
            }
            if !lines.iter().any(|r| id(r, "source_warehouse").is_some()) {
                errors.push(FieldError::new("items", "a manufacture entry needs at least one consumed line (source warehouse only)"));
            }
            if id(doc, "mfg_work_order").is_none() {
                errors.push(FieldError::new("mfg_work_order", "manufacture entries are created from a work order"));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let eid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let p = purpose(doc).to_string();
        let on = date(doc, "posting_date")?;
        let ts = posting_ts(doc)?;
        let lines = rows(doc, "items");
        // Through transit: shipped into the transit warehouse now, received later by an entry of its own.
        let shipping = p == TRANSFER && b(doc, "through_transit") && id(doc, "transfer_of").is_none();
        let transit = if shipping {
            Some(match id(doc, "transit_warehouse") {
                Some(w) => w,
                None => stock::kind_warehouse(hc.kernel, hc.ctx, hc.tx, co.id, "transit_warehouse", "Transit").await?,
            })
        } else {
            None
        };
        let receiving = match id(doc, "transfer_of").filter(|_| p == TRANSFER) {
            Some(of) => Some(transit_receipt(hc, of, &lines).await?),
            None => None,
        };
        let short_of = |r: &Doc| if receiving.is_some() { d(r, "short_qty") } else { Decimal::ZERO };

        let mut keys = Vec::new();
        for r in &lines {
            let item = id(r, "item").ok_or_else(|| Error::field("items", "a line has no item"))?;
            for w in [id(r, "source_warehouse"), transit.or(id(r, "target_warehouse"))].into_iter().flatten() {
                keys.push((item, w));
            }
        }
        stock::lock_balances(hc.tx, hc.ctx.tenant, co.id, &keys).await?;

        // Issues first: transfers and manufacture value their receipts from what was issued.
        let issues: Vec<MoveIn> = lines
            .iter()
            .filter_map(|r| {
                id(r, "source_warehouse").map(|w| MoveIn {
                    item: id(r, "item").unwrap_or_default(),
                    warehouse: w,
                    qty: -(d(r, "qty") + short_of(r)),
                    batch_no: s(r, "batch_no").map(str::to_string),
                    serials: parse_serials(s(r, "serial_no")),
                    source_line: id(r, "id"),
                    // Written off or moved within the company; manufacture consumes it.
                    unusable_batch_ok: p == ISSUE || p == TRANSFER,
                    ..Default::default()
                })
            })
            .collect();
        let issued = stock::post_moves(hc.tx, hc.ctx, &co, "StockEntry", eid, on, ts, issues, "consumed").await?;
        let issued_value: Decimal = issued.iter().map(|m| -m.value).sum();
        let issued_by_line: HashMap<Uuid, Decimal> = issued.iter().filter_map(|m| m.source_line.map(|l| (l, -m.value))).collect();

        let outputs: Vec<&Doc> = lines.iter().filter(|r| id(r, "target_warehouse").is_some()).collect();
        let additional = if p == MANUFACTURE { d(doc, "mfg_additional_cost") } else { Decimal::ZERO };
        // Manufacture: the outputs carry consumed value + additional cost, by-products first, the rest by the BOM's
        // basis (by quantity without one).
        let shares = if p == MANUFACTURE {
            let wo = id(doc, "mfg_work_order").ok_or_else(|| Error::field("mfg_work_order", "is required"))?;
            crate::manufacturing::output_shares(hc, wo, &outputs, round(issued_value + additional, co.precision), &co).await?
        } else {
            vec![]
        };
        let receipts: Vec<MoveIn> = outputs
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let line = id(r, "id");
                let (rate, exact) = match p.as_str() {
                    // A receipt short of what left transit takes the share of value of what arrived.
                    TRANSFER => (None, line.and_then(|l| issued_by_line.get(&l).copied()).map(|v| arrived_share(v, d(r, "qty"), short_of(r), co.precision))),
                    MANUFACTURE => (None, shares.get(i).copied()),
                    _ => (Some(d(r, "basic_rate")), None),
                };
                MoveIn {
                    item: id(r, "item").unwrap_or_default(),
                    warehouse: transit.or(id(r, "target_warehouse")).unwrap_or_default(),
                    qty: d(r, "qty"),
                    rate: rate.or(Some(Decimal::ZERO)),
                    exact_value: exact,
                    batch_no: s(r, "batch_no").map(str::to_string),
                    serials: parse_serials(s(r, "serial_no")),
                    source_line: line,
                    ..Default::default()
                }
            })
            .collect();
        let received = stock::post_moves(hc.tx, hc.ctx, &co, "StockEntry", eid, on, ts, receipts, "consumed").await?;

        // Ledger: every move hits its warehouse's stock account; the purpose decides the offset.
        let mut entry = Entry::new(co.id, on, "StockEntry", eid, s(doc, "number").map(str::to_string));
        entry.memo = s(doc, "remarks").map(str::to_string);
        let mut accounts = HashMap::new();
        let line_by_id: HashMap<Uuid, &Doc> = lines.iter().filter_map(|r| id(r, "id").map(|i| (i, r))).collect();
        let all: Vec<&Posted> = issued.iter().chain(received.iter()).collect();
        for m in &all {
            let acc = stock_account(hc, &co, m.warehouse, &mut accounts).await?;
            let row = m.source_line.and_then(|l| line_by_id.get(&l).copied());
            let cc = row.and_then(|r| id(r, "cost_center")).or(co.account("default_cost_center"));
            entry.lines.push(Line::new(acc, m.value, &co.currency_code));
            let offset = match p.as_str() {
                RECEIPT | ISSUE => match row.and_then(|r| id(r, "expense_account")) {
                    Some(a) => Some(a),
                    None => Some(co.require("stock_adjustment_account", "stock adjustment account")?),
                },
                OPENING => Some(match co.account("opening_balance_account") {
                    Some(a) => a,
                    None => co.require("stock_adjustment_account", "stock adjustment account")?,
                }),
                _ => None,
            };
            if let Some(o) = offset {
                entry.lines.push(Line::new(o, -m.value, &co.currency_code).cost_center(cc));
            }
        }
        // What did not arrive: written off, or owed by whoever is claimed from.
        if receiving.is_some() {
            for r in lines.iter().filter(|r| short_of(r) > Decimal::ZERO) {
                let Some(line) = id(r, "id") else { continue };
                let lost = issued_by_line.get(&line).copied().unwrap_or_default()
                    - arrived_share(issued_by_line.get(&line).copied().unwrap_or_default(), d(r, "qty"), short_of(r), co.precision);
                let to = match id(r, "short_account") {
                    Some(a) => a,
                    None => co.require("stock_adjustment_account", "stock adjustment account")?,
                };
                let why = s(r, "short_reason").unwrap_or_default().to_string();
                entry.lines.push(Line::new(to, lost, &co.currency_code).cost_center(id(r, "cost_center").or(co.account("default_cost_center"))).remarks(why));
            }
        }
        if p == MANUFACTURE && additional > Decimal::ZERO {
            let absorbed = match co.account("overhead_absorbed_account") {
                Some(a) => a,
                None => co.require("stock_adjustment_account", "stock adjustment account")?,
            };
            entry
                .lines
                .push(Line::new(absorbed, -additional, &co.currency_code).cost_center(co.account("default_cost_center")).remarks("Operating cost absorbed"));
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;

        // Write the actual rates back to the lines (a transfer line takes its receipt side).
        let sctx = sys(hc.ctx);
        let mut by_line: HashMap<Uuid, &Posted> = HashMap::new();
        for m in issued.iter().chain(received.iter()) {
            if let Some(l) = m.source_line {
                by_line.insert(l, m);
            }
        }
        for (line, m) in by_line {
            let mut v = Doc::new();
            v.insert("basic_rate".into(), money(m.rate.round_dp(9)));
            v.insert("amount".into(), money(m.value.abs()));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, "StockEntry", "items", line, v).await?;
        }
        let total: Decimal = if p == ISSUE { issued_value } else { received.iter().map(|m| m.value).sum() };
        let mut v = Doc::new();
        v.insert("total_amount".into(), money(total));
        if let Some(w) = transit {
            v.insert("transit_warehouse".into(), json!(w.to_string()));
            v.insert("transit_status".into(), json!("In Transit"));
        }
        hc.kernel.set_values_tx(&sctx, hc.tx, "StockEntry", eid, v).await?;
        if let Some(of) = receiving {
            settle_transit(hc, of, &lines, Decimal::ONE).await?;
        }

        if p == MANUFACTURE {
            let wo = id(doc, "mfg_work_order").ok_or_else(|| Error::field("mfg_work_order", "is required"))?;
            let produced: Vec<(Uuid, Decimal, Decimal)> = received.iter().map(|m| (m.item, m.qty, m.value)).collect();
            crate::manufacturing::record_production(hc, wo, &produced, Decimal::ONE).await?;
        }
        Ok(())
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if rows(doc, "items").iter().any(|r| d(r, "received_qty") > Decimal::ZERO) {
            return Err(Error::invalid("cancel the receipts of this transfer first"));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "receive_transfer" {
            return Ok(None);
        }
        let transit = id(doc, "transit_warehouse").ok_or_else(|| Error::invalid("this entry was not shipped through transit"))?;
        let mut lines = Vec::new();
        for r in rows(doc, "items") {
            let open = d(&r, "qty") - d(&r, "received_qty");
            if open <= Decimal::ZERO {
                continue;
            }
            lines.push(json!({"item": r.get("item"), "qty": money(open), "source_warehouse": transit.to_string(), "target_warehouse": r.get("target_warehouse"),
                              "batch_no": r.get("batch_no"), "serial_no": r.get("serial_no"), "transfer_line": r.get("id"), "cost_center": r.get("cost_center")}));
        }
        if lines.is_empty() {
            return Err(Error::invalid("everything on this transfer has been received"));
        }
        let input = json!({"company": doc.get("company"), "purpose": TRANSFER, "transfer_of": doc.get("id"), "items": lines});
        let made = create_for_user(hc, "StockEntry", input.as_object().cloned().unwrap_or_default()).await?;
        Ok(Some(created("StockEntry", &made)))
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let eid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        if purpose(doc) == TRANSFER
            && let Some(of) = id(doc, "transfer_of")
        {
            hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "StockEntry", of, true).await?;
            settle_transit(hc, of, &rows(doc, "items"), -Decimal::ONE).await?;
        }
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, ts) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        let keys: Vec<(Uuid, Uuid)> =
            sqlx::query_as("select item_id, warehouse_id from stock_move where tenant_id = $1 and source_entity = 'StockEntry' and source_id = $2")
                .bind(hc.ctx.tenant)
                .bind(eid)
                .fetch_all(&mut **hc.tx)
                .await?;
        stock::lock_balances(hc.tx, hc.ctx.tenant, co.id, &keys).await?;
        let reversed = stock::reverse_moves(hc.tx, hc.ctx, &co, "StockEntry", eid, on, ts).await?;
        ledger::reverse(hc.tx, hc.ctx, "StockEntry", eid, on, co.precision).await?;
        if purpose(doc) == MANUFACTURE
            && let Some(wo) = id(doc, "mfg_work_order")
        {
            // Reversed outputs are the negative moves of the reversal.
            let produced: Vec<(Uuid, Decimal, Decimal)> = reversed.iter().filter(|m| m.qty < Decimal::ZERO).map(|m| (m.item, -m.qty, -m.value)).collect();
            crate::manufacturing::record_production(hc, wo, &produced, -Decimal::ONE).await?;
        }
        Ok(())
    }
}

/// Of `issued` (what a receipt's line took out of transit, `qty` arrived and `short` not), the value of what arrived.
fn arrived_share(issued: Decimal, qty: Decimal, short: Decimal, precision: u32) -> Decimal {
    if short.is_zero() || (qty + short).is_zero() { issued } else { round(issued * qty / (qty + short), precision) }
}

/// A receipt of a transfer, checked against the shipping entry (locked first, so two receipts at once
/// cannot take the same goods): each line takes from the transfer's transit warehouse what is still open on
/// its line, arrived or short.
async fn transit_receipt(hc: &mut HookCtx<'_>, of: Uuid, lines: &[Doc]) -> Result<Uuid> {
    let orig = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "StockEntry", of, true).await?;
    let transit = id(&orig, "transit_warehouse").filter(|_| d(&orig, "docstatus") == Decimal::ONE);
    let transit = transit.ok_or_else(|| Error::field("transfer_of", "is not a submitted transfer through transit"))?;
    let by_line: HashMap<String, Doc> = rows(&orig, "items").into_iter().filter_map(|r| s(&r, "id").map(|i| (i.to_string(), r.clone()))).collect();
    let mut taken: HashMap<String, Decimal> = HashMap::new();
    for (i, r) in lines.iter().enumerate() {
        let line = s(r, "transfer_line").unwrap_or_default();
        let o = by_line.get(line).ok_or_else(|| Error::Validation(vec![FieldError::new("item", "is not a line of the transfer").in_row("items", i)]))?;
        if id(r, "source_warehouse") != Some(transit) || id(r, "item") != id(o, "item") {
            return Err(Error::Validation(vec![
                FieldError::new("source_warehouse", "a receipt takes the transfer's item from its transit warehouse").in_row("items", i),
            ]));
        }
        let t = taken.entry(line.to_string()).or_default();
        *t += d(r, "qty") + d(r, "short_qty");
        let open = d(o, "qty") - d(o, "received_qty");
        if *t > open {
            return Err(Error::Validation(vec![FieldError::new("qty", format!("only {} of it is still in transit", open.normalize())).in_row("items", i)]));
        }
    }
    Ok(of)
}

/// Adds (`sign` 1) or takes back (-1) what a receipt settled on the transfer's lines, and its status.
async fn settle_transit(hc: &mut HookCtx<'_>, of: Uuid, lines: &[Doc], sign: Decimal) -> Result<()> {
    let sctx = sys(hc.ctx);
    let orig = hc.kernel.get_tx(&sctx, hc.tx, "StockEntry", of, false).await?;
    let mut settled: HashMap<String, Decimal> = HashMap::new();
    for r in lines {
        *settled.entry(s(r, "transfer_line").unwrap_or_default().to_string()).or_default() += d(r, "qty") + d(r, "short_qty");
    }
    let (mut any, mut all) = (false, true);
    for o in rows(&orig, "items") {
        let line = s(&o, "id").unwrap_or_default().to_string();
        let now = d(&o, "received_qty") + sign * settled.get(&line).copied().unwrap_or_default();
        if let (Some(lid), Some(_)) = (id(&o, "id"), settled.get(&line)) {
            let mut v = Doc::new();
            v.insert("received_qty".into(), money(now));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, "StockEntry", "items", lid, v).await?;
        }
        any |= now > Decimal::ZERO;
        all &= now >= d(&o, "qty");
    }
    let status = if all {
        "Received"
    } else if any {
        "Partly Received"
    } else {
        "In Transit"
    };
    let mut v = Doc::new();
    v.insert("transit_status".into(), json!(status));
    hc.kernel.set_values_tx(&sctx, hc.tx, "StockEntry", of, v).await
}
