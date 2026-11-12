//! Selling and buying documents, implemented once and parameterized by side:
//! orders (SalesOrder / PurchaseOrder), shipments (DeliveryNote / PurchaseReceipt) and invoices
//! (SalesInvoice / PurchaseInvoice), including stock, ledger postings, open items, returns and
//! the "create next document" actions.

use crate::credit;
use crate::flow::{self, MapSpec, QTY_TOLERANCE, add_progress_in, create_for_user, created, refresh_billing, refresh_order, sync_invoice_outstanding};
use crate::ledger::{self, Entry, Line, OpenItem};
use crate::pricing::{Side, breakup, calculate};
use crate::stock::{self, MoveIn, Posted, parse_serials};
use crate::util::{Company, b, company, d, date, get, id, money, posting_ts, round, rows, s, set, stock_qty, sys, unit_factor};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, Filter, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

struct Names {
    order: &'static str,
    shipment: &'static str,
    invoice: &'static str,
    party: &'static str,
    party_entity: &'static str,
    fulfil_field: &'static str,
    fulfil_pct: &'static str,
    order_link: &'static str,
    order_line: &'static str,
    shipment_link: &'static str,
    shipment_line: &'static str,
    make_shipment: &'static str,
    make_invoice: &'static str,
    serial_status: &'static str,
}

const SELLING: Names = Names {
    order: "SalesOrder",
    shipment: "DeliveryNote",
    invoice: "SalesInvoice",
    party: "customer",
    party_entity: "Customer",
    fulfil_field: "delivered_qty",
    fulfil_pct: "per_delivered",
    order_link: "sales_order",
    order_line: "so_line",
    shipment_link: "delivery_note",
    shipment_line: "dn_line",
    make_shipment: "make_delivery_note",
    make_invoice: "make_sales_invoice",
    serial_status: "delivered",
};

const BUYING: Names = Names {
    order: "PurchaseOrder",
    shipment: "PurchaseReceipt",
    invoice: "PurchaseInvoice",
    party: "supplier",
    party_entity: "Supplier",
    fulfil_field: "received_qty",
    fulfil_pct: "per_received",
    order_link: "purchase_order",
    order_line: "po_line",
    shipment_link: "purchase_receipt",
    shipment_line: "pr_line",
    make_shipment: "make_purchase_receipt",
    make_invoice: "make_purchase_invoice",
    serial_status: "consumed",
};

fn names(side: Side) -> &'static Names {
    match side {
        Side::Selling => &SELLING,
        Side::Buying => &BUYING,
    }
}

fn header_fields(side: Side) -> Vec<&'static str> {
    let mut h = vec!["company", "currency", "conversion_rate", "tax_template", "set_warehouse", "discount_percent", "project"];
    h.push(names(side).party);
    if side == Side::Selling {
        h.extend(["price_list", "po_no"]);
    }
    h
}

const LINE_FIELDS: &[&str] = &["item", "uom", "price_list_rate", "discount_percent", "warehouse", "batch_no", "serial_no"];

async fn stock_account(hc: &mut HookCtx<'_>, co: &Company, warehouse: Uuid) -> Result<Uuid> {
    let w = get(hc.kernel, hc.ctx, hc.tx, "Warehouse", warehouse).await?;
    match id(&w, "account") {
        Some(a) => Ok(a),
        None => co.require("default_inventory_account", "default inventory account"),
    }
}

async fn is_stock_item(hc: &mut HookCtx<'_>, cache: &mut HashMap<Uuid, bool>, item: Uuid) -> Result<bool> {
    if let Some(v) = cache.get(&item) {
        return Ok(*v);
    }
    let it = get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?;
    let v = b(&it, "is_stock_item");
    cache.insert(item, v);
    Ok(v)
}

/// Unit cost at which an item left on the original document (None: use the current valuation).
/// The valuation an earlier sale or receipt used for `item`, so a return goes back at that cost.
pub async fn returned_cost(hc: &mut HookCtx<'_>, entity: &str, original: Uuid, item: Uuid) -> Result<Option<Decimal>> {
    let r: Option<Decimal> = sqlx::query_scalar(
        "select sum(value) / nullif(sum(qty), 0) from stock_move where tenant_id = $1 and source_entity = $2 and source_id = $3 \
         and item_id = $4 and qty < 0 and reversal_of is null",
    )
    .bind(hc.ctx.tenant)
    .bind(entity)
    .bind(original)
    .bind(item)
    .fetch_one(&mut **hc.tx)
    .await?;
    Ok(r.map(|x| x.abs().round_dp(12)))
}

/// Posts stock moves for document lines. `direction` is +1 for receipts, -1 for issues.
async fn post_line_stock(hc: &mut HookCtx<'_>, co: &Company, entity: &str, doc: &Doc, direction: Decimal, serial_status: &str) -> Result<Vec<Posted>> {
    let doc_id = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let rate = d(doc, "conversion_rate");
    let rate = if rate.is_zero() { Decimal::ONE } else { rate };
    let ret = b(doc, "is_return");
    let sales_return = ret && matches!(entity, "DeliveryNote" | "SalesInvoice");
    let original = match id(doc, "return_against").filter(|_| ret) {
        Some(orig) => Some(get(hc.kernel, hc.ctx, hc.tx, entity, orig).await?),
        None => None,
    };
    let mut cache = HashMap::new();
    let mut moves = Vec::new();
    for (i, r) in rows(doc, "items").iter().enumerate() {
        let Some(item) = id(r, "item") else { continue };
        if !is_stock_item(hc, &mut cache, item).await? {
            continue;
        }
        let wh = id(r, "warehouse").ok_or_else(|| Error::Validation(vec![FieldError::new("warehouse", "is required for stock items").in_row("items", i)]))?;
        let qty = stock_qty(r) * direction;
        if qty.is_zero() {
            continue;
        }
        // A return moves what its original moved, at the cost it moved at: goods come back at the
        // cost they left at (never at the selling price), and go back to a supplier at the cost
        // they came in at. Without an original, at the cost the line states, else today's.
        let back = match (&original, id(r, "return_line")) {
            (Some(o), Some(line)) => Some(original_moves(hc, o, entity, line, sales_return, i, r).await?),
            _ => None,
        };
        let mut exact = None;
        let incoming = if qty <= Decimal::ZERO {
            if let Some(cost) = back.flatten().filter(|_| ret)
                && !is_fifo(hc, item).await?
            {
                exact = Some(round(cost * qty.abs(), co.precision));
            }
            None
        } else if sales_return {
            match (back.flatten(), &original) {
                (Some(cost), _) => Some(cost),
                (None, Some(o)) => returned_cost(hc, entity, id(o, "id").unwrap_or_default(), item).await?,
                (None, None) => Some(d(r, "incoming_rate")).filter(|c| *c > Decimal::ZERO),
            }
        } else {
            let net = if r.get("base_net_amount").is_some_and(|v| !v.is_null()) { d(r, "base_net_amount") } else { d(r, "net_amount") * rate };
            let q = stock_qty(r).abs();
            Some(if q.is_zero() { Decimal::ZERO } else { (net.abs() / q).round_dp(12) })
        };
        moves.push(MoveIn {
            item,
            warehouse: wh,
            qty,
            rate: incoming,
            exact_value: exact,
            batch_no: s(r, "batch_no").map(str::to_string),
            serials: parse_serials(s(r, "serial_no")),
            source_line: id(r, "id"),
            // Goods leaving a purchase go back to the supplier.
            unusable_batch_ok: entity.starts_with("Purchase"),
            ..Default::default()
        });
    }
    if moves.is_empty() {
        return Ok(vec![]);
    }
    let on = date(doc, "posting_date")?;
    stock::post_moves(hc.tx, hc.ctx, co, entity, doc_id, on, posting_ts(doc)?, moves, serial_status).await
}

async fn write_valuation(hc: &mut HookCtx<'_>, entity: &str, posted: &[Posted]) -> Result<()> {
    let ent = hc.kernel.entity(hc.ctx, entity)?;
    let child = ent.children.get("items").and_then(|c| hc.ctx.schema.entity(&c.entity)).cloned();
    if child.is_none_or(|c| c.field("valuation_rate").is_none()) {
        return Ok(());
    }
    for p in posted {
        if let Some(line) = p.source_line {
            let mut v = Doc::new();
            v.insert("valuation_rate".into(), money(p.rate.round_dp(9)));
            hc.kernel.set_child_values_tx(&sys(hc.ctx), hc.tx, entity, "items", line, v).await?;
        }
    }
    Ok(())
}

// ─────────────────────────── lines without an item ───────────────────────────

/// What a line is called in a refusal: "line 2 (Rent for May)".
pub(crate) fn line_label(i: usize, r: &Doc) -> String {
    match s(r, "item_name").or(s(r, "description")).filter(|x| !x.trim().is_empty()) {
        Some(x) => format!("line {} ({})", i + 1, x.lines().next().unwrap_or(x).trim()),
        None => format!("line {}", i + 1),
    }
}

/// Every line says what it is: an item, or a description. On an invoice (`account_field` given), a
/// line without an item needs an account a sale or purchase can be booked to: not a group, nor an
/// account that only parties, money or stock move (a receivable or payable, a bank or cash
/// account, stock). Lines with an item are left as they were.
async fn check_lines(hc: &mut HookCtx<'_>, doc: &Doc, account_field: Option<&str>) -> Result<()> {
    let company = id(doc, "company");
    let mut errors = Vec::new();
    for (i, r) in rows(doc, "items").iter().enumerate() {
        let free = id(r, "item").is_none();
        if free && s(r, "description").is_none_or(|x| x.trim().is_empty()) {
            errors.push(FieldError::new("description", format!("{} has no item: say what it is in its description", line_label(i, r))).in_row("items", i));
            continue;
        }
        // A line with an item is booked as it always was (its item's or group's account, stock by
        // its warehouse): only the account a person chooses for a line without one is checked.
        let Some(field) = account_field.filter(|_| free) else { continue };
        let Some(acc) = id(r, field) else {
            errors.push(
                FieldError::new(field, format!("{} has neither an item nor an account: choose the account it is booked to", line_label(i, r)))
                    .in_row("items", i),
            );
            continue;
        };
        let a = get(hc.kernel, hc.ctx, hc.tx, "Account", acc).await?;
        let name = s(&a, "account_name").unwrap_or("this account");
        let why = if id(&a, "company") != company {
            Some("it belongs to another company")
        } else if b(&a, "is_group") {
            Some("it is a group of accounts; choose one under it")
        } else {
            match s(&a, "account_type") {
                Some("Receivable" | "Payable") => Some("what a party owes goes there through the invoice itself"),
                Some("Bank" | "Cash") => Some("money goes there through a payment entry"),
                Some("Stock") => Some("stock is valued there by its receipts and deliveries"),
                _ => None,
            }
        };
        if let Some(why) = why {
            errors.push(FieldError::new(field, format!("{} cannot be booked to {name}: {why}", line_label(i, r))).in_row("items", i));
        }
    }
    if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
}

// ───────────────────────────── returns ─────────────────────────────
//
// A return (a delivery or receipt returned, a credit note, a supplier's debit note) names its
// original in `return_against`; each of its lines counts against one of the original's lines
// (`return_line`), which keeps what came back (`returned_qty`) and, on invoices, what was given
// back before tax (`credited_amount`). A return without an original is a correction with a reason:
// nothing to count against.

async fn is_fifo(hc: &mut HookCtx<'_>, item: Uuid) -> Result<bool> {
    let it = get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?;
    Ok(s(&it, "valuation_method") == Some("FIFO"))
}

/// The shipment an invoice line was made from, if any: (entity, document, line).
fn shipment_of(entity: &str, line: &Doc) -> Option<(&'static str, Uuid, Uuid)> {
    let side = if entity.starts_with("Sales") { &SELLING } else { &BUYING };
    if entity != side.invoice {
        return None;
    }
    Some((side.shipment, id(line, side.shipment_link)?, s(line, side.shipment_line)?.parse().ok()?))
}

/// What the original's line moved (issued when `issued`, else received): checks that a return
/// line's serial numbers and batch are among them, and returns their unit cost (None when the line
/// moved nothing: the stock went on another document the engine cannot see).
async fn original_moves(hc: &mut HookCtx<'_>, original: &Doc, entity: &str, line: Uuid, issued: bool, i: usize, r: &Doc) -> Result<Option<Decimal>> {
    let orig = id(original, "id").unwrap_or_default();
    // The original's own moves, else (an invoice made from a delivery or receipt) that shipment's.
    let mut sources = vec![("", orig, line)];
    if let Some(o) = rows(original, "items").iter().find(|o| id(o, "id") == Some(line))
        && let Some(sh) = shipment_of(entity, o)
    {
        sources.push(sh);
    }
    let mut moved: Vec<(Decimal, Decimal, Option<String>, Option<String>)> = Vec::new();
    for (source, doc, line) in sources {
        moved = sqlx::query_as(
            "select qty, value, batch_no, serial_no from stock_move where tenant_id = $1 and source_entity = $2 and source_id = $3 \
             and source_line_id = $4 and reversal_of is null and (qty < 0) = $5",
        )
        .bind(hc.ctx.tenant)
        .bind(if source.is_empty() { entity } else { source })
        .bind(doc)
        .bind(line)
        .bind(issued)
        .fetch_all(&mut **hc.tx)
        .await?;
        if !moved.is_empty() {
            break;
        }
    }
    if moved.is_empty() {
        return Ok(None);
    }
    let number = s(original, "number").unwrap_or_default();
    let what = s(r, "item_name").unwrap_or("this item");
    let verb = if issued { "left on" } else { "came in on" };
    let serials: Vec<String> = moved.iter().flat_map(|m| parse_serials(m.3.as_deref())).collect();
    if let Some(sn) = parse_serials(s(r, "serial_no")).into_iter().find(|sn| !serials.contains(sn)) {
        return Err(Error::Validation(vec![
            FieldError::new("serial_no", format!("serial {sn} of {what} never {verb} {number}: only what it moved can be returned")).in_row("items", i),
        ]));
    }
    if let Some(batch) = s(r, "batch_no").filter(|x| !x.is_empty())
        && moved.iter().any(|m| m.2.is_some())
        && !moved.iter().any(|m| m.2.as_deref() == Some(batch))
    {
        return Err(Error::Validation(vec![
            FieldError::new("batch_no", format!("batch {batch} of {what} never {verb} {number}: only what it moved can be returned")).in_row("items", i),
        ]));
    }
    let (qty, value) = moved.iter().fold((Decimal::ZERO, Decimal::ZERO), |(q, v), m| (q + m.0, v + m.1));
    Ok((!qty.is_zero()).then(|| (value / qty).abs().round_dp(12)))
}

/// On saving a return against an original: the original must be a submitted document of the same
/// party that is not itself a return, the return takes its currency and rate unless it names
/// them, and each line is tied to the original's line it gives back (the first with its item that
/// has something left, when the line does not say). An item the original does not have is refused
/// on a delivery or receipt returned; on a credit or debit note it counts against no line.
async fn link_return(hc: &mut HookCtx<'_>, side: Side, entity: &str, doc: &mut Doc) -> Result<()> {
    let Some(orig) = id(doc, "return_against").filter(|_| b(doc, "is_return")) else { return Ok(()) };
    let original = original_of(hc, side, entity, doc, "return_against", orig).await?;
    let number = s(&original, "number").unwrap_or("this document").to_string();
    let lines = rows(&original, "items");
    let mut items = rows(doc, "items");
    let mut errors = Vec::new();
    for (i, r) in items.iter_mut().enumerate() {
        let Some(item) = id(r, "item") else {
            // A line without an item gives back one of the original's without one: the line it
            // names, else the first with its description that has something left to give back.
            let tied = id(r, "return_line").and_then(|l| lines.iter().find(|o| id(o, "id") == Some(l)));
            if tied.is_some_and(|o| id(o, "item").is_none()) {
                continue;
            }
            let what = s(r, "description").map(str::trim);
            let same: Vec<&Doc> = lines.iter().filter(|o| id(o, "item").is_none() && s(o, "description").map(str::trim) == what).collect();
            let line = same.iter().find(|o| d(o, "net_amount").abs() - d(o, "credited_amount") > Decimal::ZERO).or(same.first()).and_then(|o| s(o, "id"));
            set(r, "return_line", line.map(|l| json!(l)).unwrap_or(Value::Null));
            continue;
        };
        let tied = id(r, "return_line").and_then(|l| lines.iter().find(|o| id(o, "id") == Some(l)));
        if tied.is_some_and(|o| id(o, "item") == Some(item)) {
            continue;
        }
        let same: Vec<&Doc> = lines.iter().filter(|o| id(o, "item") == Some(item)).collect();
        match same.iter().find(|o| d(o, "qty") - d(o, "returned_qty") > QTY_TOLERANCE).or(same.first()).and_then(|o| s(o, "id")) {
            Some(line) => set(r, "return_line", line.to_string()),
            // A note may credit, against an invoice, what was billed on another (a subscription's
            // seats billed on a supplementary invoice, credited against the period's): it counts
            // against no line. Goods never come back on a delivery or receipt that did not move them.
            None if entity == names(side).invoice => set(r, "return_line", Value::Null),
            None => {
                let what = s(r, "item_name").unwrap_or("this item").to_string();
                errors.push(FieldError::new("item", format!("{what} is not on {number}: a return gives back what its original has")).in_row("items", i));
            }
        }
    }
    if !errors.is_empty() {
        return Err(Error::Validation(errors));
    }
    set(doc, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
    Ok(())
}

/// The original a return or a debit note (`field`) is made against: a submitted document of the same
/// party that is not itself a return. The new one takes its currency and rate unless it names them.
async fn original_of(hc: &mut HookCtx<'_>, side: Side, entity: &str, doc: &mut Doc, field: &str, orig: Uuid) -> Result<Doc> {
    let original = get(hc.kernel, hc.ctx, hc.tx, entity, orig).await?;
    let label = hc.kernel.entity(hc.ctx, entity)?.def.label.clone();
    let number = s(&original, "number").unwrap_or("this document").to_string();
    let what = if field == "return_against" { "a return" } else { "a debit note" };
    let refuse = |m: String| Err(Error::field(field, m));
    match d(&original, "docstatus") {
        x if x == Decimal::ONE => {}
        x if x.is_zero() => return refuse(format!("{label} {number} is a draft: {what} is made against a submitted one")),
        _ => return refuse(format!("{label} {number} is cancelled: {what} cannot be made against it")),
    }
    if b(&original, "is_return") {
        return refuse(format!("{number} is itself a return: make {what} against the original"));
    }
    let party = names(side).party;
    if id(&original, party) != id(doc, party) {
        return Err(Error::field(party, format!("{number} is another {party}'s: {what} is for the same {party}")));
    }
    match id(doc, "currency") {
        None => {
            for f in ["currency", "conversion_rate"] {
                set(doc, f, original.get(f).cloned().unwrap_or(Value::Null));
            }
        }
        Some(c) if Some(c) != id(&original, "currency") => return Err(Error::field("currency", format!("{what} is in the currency of {number}"))),
        Some(_) => {}
    }
    Ok(original)
}

/// Adds a customer's debit note to what its original shows charged on top (`sign` 1), or takes a
/// cancelled one off (-1).
async fn count_debit_note(hc: &mut HookCtx<'_>, doc: &Doc, sign: Decimal) -> Result<()> {
    let Some(orig) = id(doc, "debit_note_against").filter(|_| b(doc, "is_debit_note")) else { return Ok(()) };
    let original = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", orig, true).await?;
    let mut v = Doc::new();
    v.insert("debit_noted".into(), money((d(&original, "debit_noted") + sign * d(doc, "rounded_total")).max(Decimal::ZERO)));
    hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", orig, v).await
}

/// Counts a submitted return on its original's lines (`sign` 1), or takes a cancelled one off
/// (`sign` -1). Refused when a line would take back more than its original has left: quantity
/// for goods, and on invoices the amount before tax (exactly for a credit of price alone; one
/// cent over is let through for goods, so that several partial returns' rounding adds up).
async fn count_return(hc: &mut HookCtx<'_>, entity: &str, doc: &Doc, sign: Decimal) -> Result<()> {
    let Some(orig) = id(doc, "return_against").filter(|_| b(doc, "is_return")) else { return Ok(()) };
    let invoice = matches!(entity, "SalesInvoice" | "PurchaseInvoice");
    let price_only = b(doc, "price_only");
    let original = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, entity, orig, true).await?;
    let number = s(&original, "number").unwrap_or_default().to_string();
    let lines = rows(&original, "items");
    let co = company(hc.kernel, hc.ctx, hc.tx, id(&original, "company").unwrap_or_default()).await?;
    let (_, precision) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, id(&original, "currency").unwrap_or(co.currency_id)).await?;
    let mut taken: BTreeMap<Uuid, (Decimal, Decimal)> = BTreeMap::new();
    for r in rows(doc, "items") {
        let Some(line) = id(&r, "return_line") else { continue };
        let Some(o) = lines.iter().find(|o| id(o, "id") == Some(line)) else { continue };
        // In the original line's unit: a box of 12 returned against a line in pieces is 12.
        let q = (d(&r, "qty").abs() * unit_factor(&r) / unit_factor(o)).round_dp(9);
        let t = taken.entry(line).or_default();
        t.0 += q;
        t.1 += d(&r, "net_amount").abs();
    }
    for (line, (q, amount)) in taken {
        let Some(o) = lines.iter().find(|o| id(o, "id") == Some(line)) else { continue };
        let what = s(o, "item_name").unwrap_or("item");
        let mut v = Doc::new();
        if !price_only {
            let (had, done) = (d(o, "qty"), d(o, "returned_qty"));
            let mut now = done + sign * q;
            if (now - had).abs() <= QTY_TOLERANCE {
                now = had;
            }
            if sign > Decimal::ZERO && now > had {
                let left = (had - done).max(Decimal::ZERO);
                return Err(Error::invalid(format!(
                    "{what}: {} of the {} on {number} can still be returned, and this return has {}",
                    left.normalize(),
                    had.normalize(),
                    q.normalize()
                )));
            }
            v.insert("returned_qty".into(), money(now.max(Decimal::ZERO)));
        }
        if invoice {
            let (had, done) = (d(o, "net_amount").abs(), d(o, "credited_amount"));
            let now = done + sign * amount;
            let slack = if price_only { Decimal::ZERO } else { Decimal::new(1, precision) };
            if sign > Decimal::ZERO && now > had + slack {
                let left = (had - done).max(Decimal::ZERO);
                return Err(Error::invalid(format!(
                    "{what}: {} of the {} billed on {number} can still be given back, and this note gives back {}",
                    left.normalize(),
                    had.normalize(),
                    amount.normalize()
                )));
            }
            v.insert("credited_amount".into(), money(now.max(Decimal::ZERO)));
        }
        hc.kernel.set_child_values_tx(&sys(hc.ctx), hc.tx, entity, "items", line, v).await?;
    }
    Ok(())
}

/// An original with returns counted against it stays until they are cancelled.
fn has_returns(doc: &Doc) -> Result<()> {
    if rows(doc, "items").iter().any(|r| d(r, "returned_qty") > Decimal::ZERO || d(r, "credited_amount") > Decimal::ZERO) {
        return Err(Error::invalid("returns are made against this document; cancel those returns first"));
    }
    Ok(())
}

/// The draft return of an original: its lines, prices, taxes, currency and rate, for what is still
/// returnable. With `percent`, a credit (or debit) of price alone: that share of each line's price
/// for the quantity the party still holds, and no goods move.
fn return_input(hc: &HookCtx<'_>, side: Side, entity: &str, doc: &Doc, percent: Option<Decimal>) -> Result<Doc> {
    let n = names(side);
    let oid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let invoice = entity == n.invoice;
    let mut header = header_fields(side);
    if invoice {
        header.push("update_stock");
    }
    let mut line_fields = LINE_FIELDS.to_vec();
    if !invoice {
        // A returned delivery or receipt takes back what its order shows delivered or received.
        line_fields.extend([n.order_link, n.order_line]);
    }
    let extra = flow::shared_line_fields(&hc.ctx.schema, entity, entity);
    let mut input = flow::map_document_with(
        doc,
        &MapSpec { done_field: "returned_qty", link_field: "return_against", line_field: "return_line", header: &header, line_fields: &line_fields },
        &extra,
    )
    .map_err(|_| Error::invalid(format!("everything on {} has already been returned", s(doc, "number").unwrap_or("it"))))?;
    let originals = rows(doc, "items");
    let mut share_num = Decimal::ZERO;
    if let Some(Value::Array(lines)) = input.get_mut("items") {
        for l in lines.iter_mut() {
            let Value::Object(m) = l else { continue };
            m.remove("return_against");
            let o = originals.iter().find(|o| s(o, "id") == s(m, "return_line")).cloned().unwrap_or_default();
            let q = d(m, "qty");
            if !d(&o, "qty").is_zero() {
                share_num += d(&o, "net_amount") * q / d(&o, "qty");
            }
            if let Some(p) = percent {
                m.insert("price_list_rate".into(), money((d(&o, "price_list_rate") * p / Decimal::ONE_HUNDRED).round_dp(9)));
            }
            if invoice {
                m.insert("qty".into(), money(-q));
            }
        }
    }
    // A fixed (Actual) charge is given back in the share of the lines given back, the other way.
    let net = d(doc, "net_total");
    let mut share = if net.is_zero() { Decimal::ONE } else { (share_num / net).min(Decimal::ONE) };
    if let Some(p) = percent {
        share *= p / Decimal::ONE_HUNDRED;
    }
    if let Some(Value::Array(taxes)) = input.get_mut("taxes") {
        for t in taxes.iter_mut().filter_map(Value::as_object_mut) {
            if s(t, "charge_type") == Some("Actual") {
                let amount = (-d(t, "amount") * share).round_dp(9);
                t.insert("amount".into(), money(amount));
            }
        }
    }
    input.insert("is_return".into(), json!(true));
    input.insert("return_against".into(), json!(oid.to_string()));
    if percent.is_some() {
        input.insert("price_only".into(), json!(true));
        input.insert("update_stock".into(), json!(false));
    }
    Ok(input)
}

/// A percentage given to an action (`credit_percent`), when one is: above 0 and at most 100.
fn percent_param(params: &Value, name: &str) -> Result<Option<Decimal>> {
    let raw = match params.get(name) {
        Some(Value::String(x)) if !x.trim().is_empty() => x.trim().to_string(),
        Some(Value::Number(x)) => x.to_string(),
        _ => return Ok(None),
    };
    match raw.trim_end_matches('%').trim().parse::<Decimal>() {
        Ok(p) if p > Decimal::ZERO && p <= Decimal::ONE_HUNDRED => Ok(Some(p)),
        _ => Err(Error::field(name, "a share of the price above 0 and at most 100")),
    }
}

// ───────────────────────────── orders ─────────────────────────────

pub struct OrderHook(pub Side);

#[async_trait]
impl DocHook for OrderHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        calculate(hc.kernel, hc.ctx, hc.tx, hc.entity, doc, self.0).await?;
        if d(doc, "docstatus").is_zero() {
            flow::set_status(doc, "Draft");
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        check_lines(hc, doc, None).await
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        crate::localization::check(hc, doc, self.0).await?;
        // A change to a submitted order (Update Items) is judged by what it adds, after it is made.
        if self.0 == Side::Selling && d(doc, "docstatus").is_zero() {
            credit::hold(hc, doc, "orders").await?;
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        let credit = if self.0 == Side::Selling { credit::before(hc, doc, true).await? } else { None };
        crate::promotion::count_uses(hc, doc, 1).await?;
        refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, id(doc, "id").unwrap_or_default(), n.fulfil_field, n.fulfil_pct).await?;
        credit::check(hc, credit, "this order", credit::released(doc)).await
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        if rows(doc, "items").iter().any(|r| d(r, n.fulfil_field) > Decimal::ZERO || d(r, "billed_qty") > Decimal::ZERO) {
            return Err(Error::invalid("cancel the deliveries/receipts and invoices made against this order first"));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        crate::promotion::count_uses(hc, doc, -1).await?;
        refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, id(doc, "id").unwrap_or_default(), n.fulfil_field, n.fulfil_pct).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let n = names(self.0);
        let oid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        if name == "update_items" {
            return crate::change::update_items(hc, self.0, doc, params).await.map(Some);
        }
        if name == "close" || name == "reopen" {
            let number = s(doc, "number").unwrap_or_default();
            // What each line still had open: never delivered or received, and never billed.
            let open = |r: &Doc| {
                let done = if id(r, "item").is_some() { d(r, n.fulfil_field).max(d(r, "billed_qty")) } else { d(r, "billed_qty") };
                (d(r, "qty") - done).max(Decimal::ZERO)
            };
            let lines = rows(doc, "items");
            let closing = name == "close";
            if closing && s(doc, "status") == Some("Closed") {
                return Err(Error::invalid(format!("{number} is already closed")));
            }
            let settled = |r: &Doc| d(r, "billed_qty") >= d(r, "qty") && (id(r, "item").is_none() || d(r, n.fulfil_field) >= d(r, "qty"));
            if !closing && lines.iter().all(settled) {
                return Err(Error::invalid(format!("everything on {number} was delivered or received and billed: there is nothing to re-open")));
            }
            for r in &lines {
                let Some(line) = id(r, "id") else { continue };
                let mut v = Doc::new();
                v.insert("closed_qty".into(), money(if closing { open(r) } else { Decimal::ZERO }));
                hc.kernel.set_child_values_tx(&sys(hc.ctx), hc.tx, n.order, "items", line, v).await?;
            }
            let mut v = Doc::new();
            v.insert("status".into(), json!(if closing { "Closed" } else { "To Bill" }));
            hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, n.order, oid, v).await?;
            if !closing {
                refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, oid, n.fulfil_field, n.fulfil_pct).await?;
            } else if n.order == "SalesOrder" {
                // A closed order no longer holds stock.
                crate::reservation::refresh_for_order(hc.tx, hc.ctx.tenant, oid).await?;
            }
            return Ok(Some(json!({"status": if closing { "Closed" } else { "reopened" }})));
        }
        let header = header_fields(self.0);
        let (target, done) = if name == n.make_shipment {
            (n.shipment, n.fulfil_field)
        } else if name == n.make_invoice {
            (n.invoice, "billed_qty")
        } else {
            return Ok(None);
        };
        let extra = flow::shared_line_fields(&hc.ctx.schema, n.order, target);
        let mut input = flow::map_document_with(
            doc,
            &MapSpec { done_field: done, link_field: n.order_link, line_field: n.order_line, header: &header, line_fields: LINE_FIELDS },
            &extra,
        )?;
        if target == n.shipment {
            // A line without an item is a service: billed, never delivered or received.
            if let Some(Value::Array(lines)) = input.get_mut("items") {
                lines.retain(|l| l.get("item").is_some_and(|v| !v.is_null()));
                if lines.is_empty() {
                    return Err(Error::invalid("nothing on this order is delivered or received: its open lines have no item and are billed on an invoice"));
                }
            }
        }
        if target == n.invoice && self.0 == Side::Buying {
            input.insert("bill_no".into(), json!(format!("PENDING-{}", s(doc, "number").unwrap_or_default())));
        }
        let created_doc = create_for_user(hc, target, input).await?;
        Ok(Some(created(target, &created_doc)))
    }
}

// ─────────────────────────── shipments ───────────────────────────

pub struct ShipmentHook(pub Side);

#[async_trait]
impl DocHook for ShipmentHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        link_return(hc, self.0, names(self.0).shipment, doc).await?;
        calculate(hc.kernel, hc.ctx, hc.tx, hc.entity, doc, self.0).await?;
        if d(doc, "docstatus").is_zero() {
            flow::set_status(doc, "Draft");
        }
        Ok(())
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        crate::localization::check(hc, doc, self.0).await?;
        if self.0 == Side::Selling {
            credit::hold(hc, doc, "deliveries").await?;
        }
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            if id(r, "warehouse").is_none() {
                errors.push(FieldError::new("warehouse", "is required").in_row("items", i));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        let sid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let credit = if self.0 == Side::Selling { credit::before(hc, doc, true).await? } else { None };
        count_return(hc, n.shipment, doc, Decimal::ONE).await?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let ret = b(doc, "is_return");
        // Selling issues stock (returns receive it back); buying receives (returns issue).
        let direction = match (self.0, ret) {
            (Side::Selling, false) | (Side::Buying, true) => -Decimal::ONE,
            _ => Decimal::ONE,
        };
        let posted = post_line_stock(hc, &co, n.shipment, doc, direction, n.serial_status).await?;
        write_valuation(hc, n.shipment, &posted).await?;
        let on = date(doc, "posting_date")?;
        let mut entry = Entry::new(co.id, on, n.shipment, sid, s(doc, "number").map(str::to_string));
        for p in &posted {
            let inv = stock_account(hc, &co, p.warehouse).await?;
            let offset = match self.0 {
                Side::Selling => co.require("cogs_account", "cost of goods sold account")?,
                Side::Buying => co.require("srbnb_account", "stock received but not billed account")?,
            };
            entry.lines.push(Line::new(inv, p.value, &co.currency_code));
            entry.lines.push(Line::new(offset, -p.value, &co.currency_code));
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
        let sign = if ret { -Decimal::ONE } else { Decimal::ONE };
        let mut orders = Vec::new();
        for r in rows(doc, "items") {
            if let (Some(o), Some(line)) = (id(&r, n.order_link), id(&r, n.order_line)) {
                add_progress_in(hc.kernel, hc.ctx, hc.tx, n.order, o, line, n.fulfil_field, sign * d(&r, "qty"), Some(unit_factor(&r))).await?;
                if !orders.contains(&o) {
                    orders.push(o);
                }
            }
        }
        for o in orders {
            refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, o, n.fulfil_field, n.fulfil_pct).await?;
        }
        refresh_billing(hc.kernel, hc.ctx, hc.tx, n.shipment, sid).await?;
        credit::check(hc, credit, "this delivery", credit::released(doc)).await
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if rows(doc, "items").iter().any(|r| d(r, "billed_qty") > Decimal::ZERO) {
            return Err(Error::invalid("cancel the invoices made against this document first"));
        }
        has_returns(doc)
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        let sid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, ts) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        count_return(hc, n.shipment, doc, -Decimal::ONE).await?;
        stock::reverse_moves(hc.tx, hc.ctx, &co, n.shipment, sid, on, ts).await?;
        ledger::reverse(hc.tx, hc.ctx, n.shipment, sid, on, co.precision).await?;
        let sign = if b(doc, "is_return") { Decimal::ONE } else { -Decimal::ONE };
        let mut orders = Vec::new();
        for r in rows(doc, "items") {
            if let (Some(o), Some(line)) = (id(&r, n.order_link), id(&r, n.order_line)) {
                add_progress_in(hc.kernel, hc.ctx, hc.tx, n.order, o, line, n.fulfil_field, sign * d(&r, "qty"), Some(unit_factor(&r))).await?;
                if !orders.contains(&o) {
                    orders.push(o);
                }
            }
        }
        for o in orders {
            refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, o, n.fulfil_field, n.fulfil_pct).await?;
        }
        refresh_billing(hc.kernel, hc.ctx, hc.tx, n.shipment, sid).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        let n = names(self.0);
        if matches!(name, "make_sales_return" | "make_purchase_return") {
            if b(doc, "is_return") {
                return Err(Error::invalid("this is itself a return: make the return against the original"));
            }
            let input = return_input(hc, self.0, n.shipment, doc, None)?;
            let made = create_for_user(hc, n.shipment, input).await?;
            return Ok(Some(created(n.shipment, &made)));
        }
        if name != n.make_invoice {
            return Ok(None);
        }
        let header = header_fields(self.0);
        let mut line_fields = LINE_FIELDS.to_vec();
        line_fields.extend([n.order_link, n.order_line]);
        let extra = flow::shared_line_fields(&hc.ctx.schema, n.shipment, n.invoice);
        let mut input = flow::map_document_with(
            doc,
            &MapSpec { done_field: "billed_qty", link_field: n.shipment_link, line_field: n.shipment_line, header: &header, line_fields: &line_fields },
            &extra,
        )?;
        if self.0 == Side::Buying {
            input.insert("bill_no".into(), json!(format!("PENDING-{}", s(doc, "number").unwrap_or_default())));
        }
        let created_doc = create_for_user(hc, n.invoice, input).await?;
        Ok(Some(created(n.invoice, &created_doc)))
    }
}

// ─────────────────────────── a sale within the group ───────────────────────────

/// The purchase invoice a sale to one of the group's companies is in that company's books, if one
/// was made from it (`submitted`: only a submitted one): its number, and the buying company's name.
/// It is the one from the supplier that represents the seller, with the sale's number as the
/// supplier's invoice number.
async fn mirrored(hc: &mut HookCtx<'_>, doc: &Doc, submitted: bool) -> Result<Option<(String, String)>> {
    let (Some(buyer), Some(seller), Some(number)) = (id(doc, "customer_company"), id(doc, "company"), s(doc, "number")) else { return Ok(None) };
    if hc.ctx.schema.entity("PurchaseInvoice").is_none() {
        return Ok(None);
    }
    Ok(sqlx::query_as(
        "select coalesce(p.number, p.id::text), c.company_name from e_purchase_invoice p join e_supplier v on v.tenant_id = p.tenant_id and v.id = p.supplier \
         join e_company c on c.tenant_id = p.tenant_id and c.id = p.company where p.tenant_id = $1 and p.company = $2 and v.represents_company = $3 \
         and p.bill_no = $4 and p.docstatus = any($5) order by p.docstatus desc limit 1",
    )
    .bind(hc.ctx.tenant)
    .bind(buyer)
    .bind(seller)
    .bind(number)
    .bind(if submitted { vec![1i16] } else { vec![0i16, 1] })
    .fetch_optional(&mut **hc.tx)
    .await?)
}

/// The purchase invoice a sale to one of the group's companies is in the buying company's books:
/// from the supplier that represents the seller, with the sale's number and day as the supplier's
/// invoice number and date, at the sale's net prices; the buying company's own taxes and accounts
/// apply. Made once: a second is refused while the first stands.
async fn inter_company_invoice(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
    if hc.ctx.schema.entity("PurchaseInvoice").is_none() {
        return Err(Error::invalid("buying is not installed here: there is no purchase invoice to make"));
    }
    if b(doc, "is_return") {
        return Err(Error::invalid("a credit note is not mirrored; make the group company's own debit note"));
    }
    let buyer = id(doc, "customer_company").ok_or_else(|| Error::invalid("the customer represents none of the group's companies"))?;
    let seller = id(doc, "company").ok_or_else(|| Error::internal("an invoice without its company"))?;
    if let Some((number, name)) = mirrored(hc, doc, false).await? {
        return Err(Error::invalid(format!("this sale is already {number} in {name}'s books")));
    }
    let supplier: Option<Uuid> =
        sqlx::query_scalar("select id from e_supplier where tenant_id = $1 and represents_company = $2 order by created_at, id limit 1")
            .bind(hc.ctx.tenant)
            .bind(seller)
            .fetch_optional(&mut **hc.tx)
            .await?;
    let seller_name = company(hc.kernel, hc.ctx, hc.tx, seller).await?.name().to_string();
    let supplier =
        supplier.ok_or_else(|| Error::invalid(format!("no supplier represents {seller_name}: make one, Represents Company {seller_name}, to buy from")))?;
    let lines: Vec<Value> = rows(doc, "items")
        .iter()
        .map(|r| {
            json!({"item": r.get("item"), "description": r.get("description"), "qty": r.get("qty"), "uom": r.get("uom"), "price_list_rate": r.get("rate"), "discount_percent": "0"})
        })
        .collect();
    let mut input = Doc::new();
    for (k, v) in [
        ("company", json!(buyer.to_string())),
        ("supplier", json!(supplier.to_string())),
        ("posting_date", doc.get("posting_date").cloned().unwrap_or(Value::Null)),
        ("bill_no", json!(s(doc, "number"))),
        ("bill_date", doc.get("posting_date").cloned().unwrap_or(Value::Null)),
        ("due_date", doc.get("due_date").cloned().unwrap_or(Value::Null)),
        ("currency", doc.get("currency").cloned().unwrap_or(Value::Null)),
        ("discount_percent", doc.get("discount_percent").cloned().unwrap_or(Value::Null)),
        ("items", Value::Array(lines)),
    ] {
        input.insert(k.into(), v);
    }
    let made = create_for_user(hc, "PurchaseInvoice", input).await?;
    Ok(created("PurchaseInvoice", &made))
}

// ─────────────────────────── a customer's debit note ───────────────────────────

/// A debit note to a customer: an invoice charging more on a submitted one (a price increase,
/// something charged short), linked to it. Its lines are the original's, at `percent` of their
/// prices when given (a fixed charge in the same share), else as invoiced for the user to change.
async fn debit_note(hc: &mut HookCtx<'_>, doc: &Doc, percent: Option<Decimal>) -> Result<Value> {
    if b(doc, "is_return") {
        return Err(Error::invalid("a credit note takes no debit note: make it against the original invoice"));
    }
    let oid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let extra = flow::shared_line_fields(&hc.ctx.schema, "SalesInvoice", "SalesInvoice");
    let spec =
        MapSpec { done_field: "__none__", link_field: "__against", line_field: "__line", header: &header_fields(Side::Selling), line_fields: LINE_FIELDS };
    let mut input = flow::map_document_with(doc, &spec, &extra)?;
    let share = percent.map(|p| p / Decimal::ONE_HUNDRED);
    if let Some(Value::Array(lines)) = input.get_mut("items") {
        for m in lines.iter_mut().filter_map(Value::as_object_mut) {
            m.remove("__against");
            m.remove("__line");
            if let Some(f) = share {
                m.insert("price_list_rate".into(), money((d(m, "price_list_rate") * f).round_dp(9)));
            }
        }
    }
    if let Some(Value::Array(taxes)) = input.get_mut("taxes") {
        for t in taxes.iter_mut().filter_map(Value::as_object_mut) {
            if s(t, "charge_type") == Some("Actual") {
                let amount = (d(t, "amount") * share.unwrap_or_default()).round_dp(9);
                t.insert("amount".into(), money(amount));
            }
        }
    }
    input.insert("is_debit_note".into(), json!(true));
    input.insert("debit_note_against".into(), json!(oid.to_string()));
    let made = create_for_user(hc, "SalesInvoice", input).await?;
    Ok(created("SalesInvoice", &made))
}

// ─────────────────────────── invoices ───────────────────────────

pub struct InvoiceHook(pub Side);

impl InvoiceHook {
    fn party_account_field(&self) -> &'static str {
        match self.0 {
            Side::Selling => "debit_to",
            Side::Buying => "credit_to",
        }
    }
}

#[async_trait]
impl DocHook for InvoiceHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let n = names(self.0);
        link_return(hc, self.0, n.invoice, doc).await?;
        if b(doc, "is_debit_note")
            && let Some(orig) = id(doc, "debit_note_against")
        {
            original_of(hc, self.0, n.invoice, doc, "debit_note_against", orig).await?;
        }
        let co = calculate(hc.kernel, hc.ctx, hc.tx, hc.entity, doc, self.0).await?;
        if let Some(pid) = id(doc, n.party) {
            let p = get(hc.kernel, hc.ctx, hc.tx, n.party_entity, pid).await?;
            let acc = match self.0 {
                Side::Selling => id(&p, "receivable_account").or(co.account("default_receivable_account")),
                Side::Buying => id(&p, "payable_account").or(co.account("default_payable_account")),
            };
            set(doc, self.party_account_field(), acc.map(|a| json!(a.to_string())).unwrap_or(Value::Null));
            if s(doc, "due_date").is_none() {
                let days = p.get("payment_terms_days").and_then(Value::as_i64).unwrap_or(0);
                let due = date(doc, "posting_date")? + chrono::Duration::days(days);
                set(doc, "due_date", due.to_string());
            }
        }
        crate::withholding::invoice(hc, doc, self.0, &co).await?;
        if self.0 == Side::Selling {
            crate::stored_value::mask_lines(hc.ctx, doc)?;
        }
        if d(doc, "docstatus").is_zero() {
            flow::set_status(doc, "Draft");
            // What is withheld comes off what the supplier is owed; what is collected is added to what the customer owes.
            let withheld = if self.0 == Side::Selling { d(doc, "withholding_amount") } else { -d(doc, "withholding_amount") };
            set(doc, "outstanding_amount", money(d(doc, "rounded_total") + withheld));
        }
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        let ret = b(doc, "is_return");
        let mut errors = Vec::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            let q = d(r, "qty");
            if q.is_zero() || (ret && q > Decimal::ZERO) || (!ret && q < Decimal::ZERO) {
                errors.push(FieldError::new("qty", if ret { "returns need negative quantities" } else { "quantity must be positive" }).in_row("items", i));
            }
            if b(doc, "update_stock") && id(r, "item").is_some() && id(r, "warehouse").is_none() {
                errors.push(FieldError::new("warehouse", "is required when updating stock").in_row("items", i));
            }
        }
        if id(doc, self.party_account_field()).is_none() {
            errors.push(FieldError::new(self.party_account_field(), "no receivable/payable account found; set the company defaults"));
        }
        if b(doc, "price_only") && !ret {
            errors.push(FieldError::new("price_only", "only a credit or debit note gives back price alone"));
        }
        if b(doc, "price_only") && b(doc, "update_stock") {
            errors.push(FieldError::new("update_stock", "a note for price alone moves no goods: untick Update Stock"));
        }
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        check_lines(hc, doc, Some(if self.0 == Side::Selling { "income_account" } else { "expense_account" })).await?;
        if self.0 == Side::Selling {
            crate::stored_value::check_invoice(hc, doc).await?;
        }
        if self.0 == Side::Buying
            && let (Some(supplier), Some(bill)) = (id(doc, "supplier"), s(doc, "bill_no"))
        {
            let dupes = hc
                .kernel
                .find_tx(
                    &sys(hc.ctx),
                    hc.tx,
                    n.invoice,
                    &[
                        Filter::eq("supplier", supplier.to_string()),
                        Filter::eq("bill_no", bill),
                        Filter { field: "docstatus".into(), op: ferrum_kernel::Op::Ne, value: json!(2) },
                    ],
                    None,
                    2,
                )
                .await?;
            if dupes.iter().any(|x| id(x, "id") != id(doc, "id")) {
                return Err(Error::field("bill_no", "this supplier invoice number was already entered"));
            }
        }
        Ok(())
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        crate::localization::check(hc, doc, self.0).await?;
        if self.0 == Side::Buying {
            // A reverse charge on a purchase is owed by the company: it needs the account it is owed on.
            let missing: Vec<FieldError> = rows(doc, "taxes")
                .iter()
                .enumerate()
                .filter(|(_, t)| b(t, "reverse_charge") && id(t, "reverse_charge_account").is_none())
                .map(|(i, _)| FieldError::new("reverse_charge_account", "is required for a reverse charge").in_row("taxes", i))
                .collect();
            if !missing.is_empty() {
                return Err(Error::Validation(missing));
            }
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        let iid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let credit = if self.0 == Side::Selling { credit::before(hc, doc, true).await? } else { None };
        crate::promotion::count_uses(hc, doc, 1).await?;
        count_return(hc, n.invoice, doc, Decimal::ONE).await?;
        count_debit_note(hc, doc, Decimal::ONE).await?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let on = date(doc, "posting_date")?;
        let number = s(doc, "number").map(str::to_string);
        let rate = d(doc, "conversion_rate");
        let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, id(doc, "currency").unwrap_or(co.currency_id)).await?;
        let party_id = id(doc, n.party);
        let party_account = id(doc, self.party_account_field()).ok_or_else(|| Error::field(self.party_account_field(), "is required"))?;
        // Debit-side sign: selling debits the customer, buying credits the supplier.
        let sign = match self.0 {
            Side::Selling => Decimal::ONE,
            Side::Buying => -Decimal::ONE,
        };
        // What the party owes or is owed: the invoice's total less what is withheld from a supplier,
        // or plus what is collected from a customer, booked to the categories' accounts.
        let withheld = crate::withholding::lines(hc, doc, &co).await?;
        let withheld_base: Decimal = -withheld.iter().map(|l| l.amount).sum::<Decimal>();
        let total_txn = d(doc, "rounded_total") + sign * d(doc, "withholding_amount");
        let total_base = d(doc, "base_rounded_total") + sign * withheld_base;
        let mut entry = Entry::new(co.id, on, n.invoice, iid, number.clone());
        entry.lines.extend(withheld);
        entry.memo = s(doc, "remarks").map(str::to_string);
        let mut party_line = Line::new(party_account, sign * total_base, &co.currency_code).txn(sign * total_txn, &code, rate).against(n.invoice, iid);
        party_line = match self.0 {
            Side::Selling => party_line.customer(party_id),
            Side::Buying => party_line.supplier(party_id),
        };
        entry.lines.push(party_line);
        let bk = breakup(doc);
        let update_stock = b(doc, "update_stock");
        let mut stock_cache = HashMap::new();
        for (i, (r, base)) in bk.lines.iter().enumerate() {
            let account_field = if self.0 == Side::Selling { "income_account" } else { "expense_account" };
            let mut account = id(r, account_field);
            let mut amount = *base;
            // A line earned or used over a service period waits on its deferred account until then.
            let deferred = crate::deferred::account(r, &co, self.0 == Side::Selling, i)?;
            if self.0 == Side::Buying
                && let Some(item) = id(r, "item")
            {
                let stock = is_stock_item(hc, &mut stock_cache, item).await?;
                if stock && deferred.is_some() {
                    return Err(Error::field(format!("items.{i}.enable_deferred_expense"), "a stock item is valued by its stock, not deferred"));
                }
                if stock && update_stock {
                    // Valued by the stock moves below; nothing to book here.
                    continue;
                }
                if stock && b(doc, "price_only") {
                    // A lower price for goods kept: a price variance, as on a receipt's bill.
                    account = Some(co.require("stock_adjustment_account", "stock adjustment account")?);
                } else if stock {
                    if let (Some(pr), Some(pr_line)) = (id(r, "purchase_receipt"), id(r, "pr_line")) {
                        // Clear SRBNB at the receipt's value; the difference is a price variance.
                        let received: Option<(Decimal, Decimal)> = sqlx::query_as(
                            "select coalesce(sum(value), 0), coalesce(sum(qty), 0) from stock_move where tenant_id = $1 and source_entity = 'PurchaseReceipt' \
                                 and source_id = $2 and source_line_id = $3 and reversal_of is null",
                        )
                        .bind(hc.ctx.tenant)
                        .bind(pr)
                        .bind(pr_line)
                        .fetch_optional(&mut **hc.tx)
                        .await?;
                        if let Some((v, q)) = received.filter(|(_, q)| !q.is_zero()) {
                            let srbnb_amount = round(v / q * stock_qty(r), co.precision);
                            let srbnb = co.require("srbnb_account", "stock received but not billed account")?;
                            entry.lines.push(Line::new(srbnb, srbnb_amount, &co.currency_code).remarks("SRBNB clearing"));
                            let variance = *base - srbnb_amount;
                            if !variance.is_zero() {
                                let adj = co.require("stock_adjustment_account", "stock adjustment account")?;
                                entry.lines.push(Line::new(adj, variance, &co.currency_code).remarks("Purchase price variance"));
                            }
                            continue;
                        }
                    }
                    account = account.or(co.account("srbnb_account"));
                }
            }
            if deferred.is_none()
                && account.is_none()
                && let Some(item) = id(r, "item")
            {
                let it = get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?;
                account = crate::pricing::item_account(hc.ctx, hc.tx, &co, &it, self.0).await?;
            }
            let account = match deferred.or(account) {
                Some(a) => a,
                None => match self.0 {
                    Side::Selling => co.require("default_income_account", "default income account")?,
                    Side::Buying => co.require("default_expense_account", "default expense account")?,
                },
            };
            amount *= -sign;
            entry.lines.push(
                Line::new(account, amount, &co.currency_code)
                    .cost_center(id(r, "cost_center"))
                    .project(id(doc, "project"))
                    .item(id(r, "item"))
                    .dims(crate::dimensions::of_line(doc, r)),
            );
        }
        for (t, base) in &bk.taxes {
            let acc = id(t, "account").ok_or_else(|| Error::field("taxes", "a tax row has no account"))?;
            let memo = s(t, "description").unwrap_or("Tax");
            if b(t, "reverse_charge") {
                // The buyer accounts for it: a purchase books it as input tax and as tax owed, which
                // leaves the supplier's credit at the net; a sale books nothing.
                if self.0 == Side::Buying && !base.is_zero() {
                    let owed = id(t, "reverse_charge_account").ok_or_else(|| Error::field("taxes", "a reverse-charge row needs the account it is owed on"))?;
                    entry.lines.push(Line::new(acc, *base, &co.currency_code).cost_center(id(t, "cost_center")).remarks(memo));
                    entry.lines.push(Line::new(owed, -*base, &co.currency_code).cost_center(id(t, "cost_center")).remarks(format!("{memo} (reverse charge)")));
                }
                continue;
            }
            entry.lines.push(Line::new(acc, -sign * *base, &co.currency_code).cost_center(id(t, "cost_center")).remarks(memo));
        }
        let adj = d(doc, "base_rounded_total") - d(doc, "base_grand_total");
        if !adj.is_zero() {
            let ro = co.require("round_off_account", "round off account")?;
            entry.lines.push(Line::new(ro, -sign * adj, &co.currency_code).remarks("Rounding"));
        }
        if update_stock {
            let direction = match (self.0, b(doc, "is_return")) {
                (Side::Selling, _) => -Decimal::ONE,
                (Side::Buying, _) => Decimal::ONE,
            };
            let posted = post_line_stock(hc, &co, n.invoice, doc, direction, n.serial_status).await?;
            write_valuation(hc, n.invoice, &posted).await?;
            for p in &posted {
                let inv = stock_account(hc, &co, p.warehouse).await?;
                entry.lines.push(Line::new(inv, p.value, &co.currency_code));
                match self.0 {
                    Side::Selling => entry.lines.push(Line::new(co.require("cogs_account", "cost of goods sold account")?, -p.value, &co.currency_code)),
                    Side::Buying => {
                        // The supplier credit already carries the line value; the stock debit replaces the expense debit.
                        let line_base = bk.lines.iter().find(|(r, _)| id(r, "id") == p.source_line).map(|(_, b)| *b).unwrap_or(p.value);
                        let diff = line_base - p.value;
                        if !diff.is_zero() {
                            entry.lines.push(Line::new(co.require("stock_adjustment_account", "stock adjustment account")?, diff, &co.currency_code));
                        }
                    }
                }
            }
        }
        // The party, tax and stock lines are the document's: its dimensions.
        let header = crate::dimensions::of(doc.get("dimensions"));
        for l in entry.lines.iter_mut() {
            l.dims = crate::dimensions::over(&header, &l.dims);
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;

        // Open item (negative for credit/debit notes) and settlement of the original by a return.
        let due = s(doc, "due_date").and_then(|x| ferrum_kernel::value::parse_date(x).ok());
        let (customer, supplier) = match self.0 {
            Side::Selling => (party_id, None),
            Side::Buying => (None, party_id),
        };
        ledger::open_item(
            hc.tx,
            hc.ctx,
            &OpenItem {
                id: iid,
                company: co.id,
                entity: n.invoice.into(),
                number,
                customer,
                supplier,
                account: party_account,
                posting_date: on,
                due_date: due,
                currency: code,
                exchange_rate: rate,
                amount: total_txn,
                base_amount: total_base,
            },
        )
        .await?;
        if b(doc, "is_return")
            && let Some(orig) = id(doc, "return_against")
        {
            let item = ledger::load_open_item(hc.tx, hc.ctx.tenant, orig, true)
                .await?
                .ok_or_else(|| Error::field("return_against", "original invoice is not open"))?;
            let settle = (-total_txn).min(item.outstanding);
            if settle > Decimal::ZERO {
                let base = round(settle * item.exchange_rate, co.precision);
                ledger::allocate(hc.tx, hc.ctx, co.id, orig, n.invoice, iid, on, settle, base).await?;
                ledger::allocate(hc.tx, hc.ctx, co.id, iid, n.invoice, iid, on, -settle, -round(settle * rate, co.precision)).await?;
                sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, n.invoice, orig).await?;
            }
        }
        sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, n.invoice, iid).await?;

        // Progress on orders and shipments.
        let mut orders = Vec::new();
        let mut shipments = Vec::new();
        for r in rows(doc, "items") {
            let (q, unit) = (d(&r, "qty"), Some(unit_factor(&r)));
            if let (Some(o), Some(line)) = (id(&r, n.order_link), id(&r, n.order_line)) {
                add_progress_in(hc.kernel, hc.ctx, hc.tx, n.order, o, line, "billed_qty", q, unit).await?;
                if update_stock {
                    add_progress_in(hc.kernel, hc.ctx, hc.tx, n.order, o, line, n.fulfil_field, q, unit).await?;
                }
                if !orders.contains(&o) {
                    orders.push(o);
                }
            }
            if let (Some(sh), Some(line)) = (id(&r, n.shipment_link), id(&r, n.shipment_line)) {
                add_progress_in(hc.kernel, hc.ctx, hc.tx, n.shipment, sh, line, "billed_qty", q, unit).await?;
                if !shipments.contains(&sh) {
                    shipments.push(sh);
                }
            }
        }
        for o in orders {
            refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, o, n.fulfil_field, n.fulfil_pct).await?;
        }
        for sh in shipments {
            refresh_billing(hc.kernel, hc.ctx, hc.tx, n.shipment, sh).await?;
        }
        if self.0 == Side::Buying && !b(doc, "is_return") {
            for (r, base) in &bk.lines {
                // Per stock unit, as price lists and valuation count.
                let (Some(item), q) = (id(r, "item"), stock_qty(r)) else { continue };
                if q > Decimal::ZERO {
                    let mut v = Doc::new();
                    v.insert("last_purchase_rate".into(), money((*base / q).round_dp(9)));
                    hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "Item", item, v).await?;
                }
            }
        }
        if self.0 == Side::Selling {
            crate::stored_value::invoice_submitted(hc, doc).await?;
        }
        credit::check(hc, credit, "this invoice", credit::released(doc)).await
    }

    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let iid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let payments: i64 = sqlx::query_scalar(
            "select count(*) from allocation a where a.tenant_id = $1 and a.open_item_id = $2 and a.payment_entity = 'PaymentEntry' \
             and a.reversal_of is null and not exists (select 1 from allocation r where r.tenant_id = a.tenant_id and r.reversal_of = a.id)",
        )
        .bind(hc.ctx.tenant)
        .bind(iid)
        .fetch_one(&mut **hc.tx)
        .await?;
        if payments > 0 {
            return Err(Error::invalid("payments are allocated to this invoice; cancel those payments first"));
        }
        has_returns(doc)?;
        if d(doc, "debit_noted") > Decimal::ZERO {
            return Err(Error::invalid("debit notes are made against this invoice; cancel those debit notes first"));
        }
        if self.0 == Side::Selling
            && let Some((number, buyer)) = mirrored(hc, doc, true).await?
        {
            return Err(Error::invalid(format!("this sale is {number} in {buyer}'s books; cancel that purchase invoice first")));
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        let iid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let (on, ts) = ledger::reversal_point(hc.tx, hc.ctx.tenant, co.id, doc).await?;
        count_return(hc, n.invoice, doc, -Decimal::ONE).await?;
        count_debit_note(hc, doc, -Decimal::ONE).await?;
        crate::promotion::count_uses(hc, doc, -1).await?;
        let touched = ledger::unallocate(hc.tx, hc.ctx, n.invoice, iid, on).await?;
        ledger::cancel_open_item(hc.tx, hc.ctx, iid).await?;
        if b(doc, "update_stock") {
            stock::reverse_moves(hc.tx, hc.ctx, &co, n.invoice, iid, on, ts).await?;
        }
        ledger::reverse(hc.tx, hc.ctx, n.invoice, iid, on, co.precision).await?;
        if self.0 == Side::Selling {
            crate::stored_value::reverse_entries(hc.kernel, hc.ctx, hc.tx, n.invoice, iid, on, "this sale").await?;
        }
        for t in touched {
            if t != iid {
                sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, n.invoice, t).await?;
            }
        }
        sync_invoice_outstanding(hc.kernel, hc.ctx, hc.tx, n.invoice, iid).await?;
        let mut orders = Vec::new();
        let mut shipments = Vec::new();
        for r in rows(doc, "items") {
            let (q, unit) = (-d(&r, "qty"), Some(unit_factor(&r)));
            if let (Some(o), Some(line)) = (id(&r, n.order_link), id(&r, n.order_line)) {
                add_progress_in(hc.kernel, hc.ctx, hc.tx, n.order, o, line, "billed_qty", q, unit).await?;
                if b(doc, "update_stock") {
                    add_progress_in(hc.kernel, hc.ctx, hc.tx, n.order, o, line, n.fulfil_field, q, unit).await?;
                }
                if !orders.contains(&o) {
                    orders.push(o);
                }
            }
            if let (Some(sh), Some(line)) = (id(&r, n.shipment_link), id(&r, n.shipment_line)) {
                add_progress_in(hc.kernel, hc.ctx, hc.tx, n.shipment, sh, line, "billed_qty", q, unit).await?;
                if !shipments.contains(&sh) {
                    shipments.push(sh);
                }
            }
        }
        for o in orders {
            refresh_order(hc.kernel, hc.ctx, hc.tx, n.order, o, n.fulfil_field, n.fulfil_pct).await?;
        }
        for sh in shipments {
            refresh_billing(hc.kernel, hc.ctx, hc.tx, n.shipment, sh).await?;
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let n = names(self.0);
        let iid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        match (self.0, name) {
            // A credit note to a customer; a supplier's debit note (named either way).
            (Side::Selling, "make_credit_note") | (Side::Buying, "make_debit_note" | "make_credit_note") => {
                if b(doc, "is_return") {
                    return Err(Error::invalid("this is itself a return: make the return against the original"));
                }
                let percent = percent_param(params, "credit_percent")?;
                let mut input = return_input(hc, self.0, n.invoice, doc, percent)?;
                if self.0 == Side::Buying {
                    // The supplier's credit note number, or the company's own debit note's, replaces it.
                    let place = Uuid::now_v7().simple().to_string();
                    input.insert("bill_no".into(), json!(format!("PENDING-{}-{}", s(doc, "number").unwrap_or_default(), &place[place.len() - 6..])));
                }
                let c = create_for_user(hc, n.invoice, input).await?;
                return Ok(Some(created(n.invoice, &c)));
            }
            (Side::Selling, "make_debit_note") => return debit_note(hc, doc, percent_param(params, "charge_percent")?).await.map(Some),
            (_, "make_refund") => {
                let credit = -d(doc, "outstanding_amount");
                if !b(doc, "is_return") || credit <= Decimal::ZERO {
                    return Err(Error::invalid("nothing is left to give back on this note"));
                }
                let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
                let mut input = Doc::new();
                input.insert("company".into(), json!(co.id.to_string()));
                input.insert("payment_type".into(), json!(if self.0 == Side::Selling { "Pay" } else { "Receive" }));
                input.insert("is_refund".into(), json!(true));
                input.insert(n.party.into(), doc.get(n.party).cloned().unwrap_or(Value::Null));
                input.insert("currency".into(), doc.get("currency").cloned().unwrap_or(Value::Null));
                input.insert("paid_amount".into(), money(credit));
                if let Some(bank) = co.account("default_bank_account") {
                    input.insert("bank_account".into(), json!(bank.to_string()));
                }
                input.insert("references".into(), json!([{"invoice": iid.to_string(), "allocated_amount": credit.to_string()}]));
                if self.0 == Side::Selling {
                    // Onto new store credit when asked, else back onto the stored value that paid the sale, when its kind says so.
                    let onto = match (crate::stored_value::uuid_param(params, "store_credit")?, id(doc, "return_against")) {
                        (Some(kind), _) => Some(("stored_value_kind", kind)),
                        (None, Some(orig)) => crate::stored_value::refund_onto(hc.kernel, hc.ctx, hc.tx, orig).await?.map(|a| ("stored_value_account", a)),
                        _ => None,
                    };
                    if let Some((field, v)) = onto {
                        input.insert(field.into(), json!(v.to_string()));
                    }
                }
                let p = create_for_user(hc, "PaymentEntry", input).await?;
                return Ok(Some(created("PaymentEntry", &p)));
            }
            _ => {}
        }
        match name {
            "make_payment" => {
                let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
                let outstanding = d(doc, "outstanding_amount");
                let kind = if self.0 == Side::Selling { "Sales Invoice" } else { "Purchase Invoice" };
                let mut input = Doc::new();
                input.insert("company".into(), json!(co.id.to_string()));
                input.insert("payment_type".into(), json!(if self.0 == Side::Selling { "Receive" } else { "Pay" }));
                input.insert(n.party.into(), doc.get(n.party).cloned().unwrap_or(Value::Null));
                input.insert("currency".into(), doc.get("currency").cloned().unwrap_or(Value::Null));
                input.insert("paid_amount".into(), money(outstanding));
                if let Some(bank) = co.account("default_bank_account") {
                    input.insert("bank_account".into(), json!(bank.to_string()));
                }
                input.insert("references".into(), json!([{"invoice_type": kind, "invoice": iid.to_string(), "allocated_amount": outstanding.to_string()}]));
                let p = create_for_user(hc, "PaymentEntry", input).await?;
                Ok(Some(created("PaymentEntry", &p)))
            }
            "make_inter_company_invoice" if self.0 == Side::Selling => inter_company_invoice(hc, doc).await.map(Some),
            _ => Ok(None),
        }
    }
}
