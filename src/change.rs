//! A submitted sales or purchase order changed without cancelling it (Update Items): a line's
//! quantity up or down (never below what was delivered, received or billed on it), a line added, a
//! line with nothing fulfilled removed, a delivery or required-by date moved, and the price of
//! what is not yet billed where prices are typed (purchase orders; sales lines without an item).
//!
//! The changed order goes through every hook registered on its entity as at submission
//! (`before_validate`, `validate`, `validate_change` with the order as it was, `before_submit`),
//! so other apps' rules hold after a change exactly as they did when it was submitted; it is then
//! stored, its status, percentages and stock reservations follow, the hooks' `after_save` runs
//! with the order as it was (an app that counts an order's lines on submit moves its counts
//! there), and the customer's credit is checked on what the change adds. The change is one audit
//! event, `update_items`, listing what changed from what to what.

use crate::credit;
use crate::flow::{QTY_TOLERANCE, refresh_order};
use crate::pricing::Side;
use crate::trade::line_label;
use crate::util::{d, get, id, money, rows, s, set, sys};
use ferrum_kernel::{Doc, Error, HookCtx, Result};
use ferrum_meta::FieldType;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::str::FromStr;
use uuid::Uuid;

struct Names {
    entity: &'static str,
    fulfil: &'static str,
    pct: &'static str,
    verb: &'static str,
    date: &'static str,
}

fn names(side: Side) -> Names {
    match side {
        Side::Selling => Names { entity: "SalesOrder", fulfil: "delivered_qty", pct: "per_delivered", verb: "delivered", date: "delivery_date" },
        Side::Buying => Names { entity: "PurchaseOrder", fulfil: "received_qty", pct: "per_received", verb: "received", date: "schedule_date" },
    }
}

/// One line's change as asked: `line` names an existing line (its number on the order, or its id);
/// without it, a line is added.
#[derive(Default)]
struct Asked {
    line: Option<String>,
    item: Option<Uuid>,
    description: Option<String>,
    qty: Option<Decimal>,
    price: Option<Decimal>,
    date: Option<String>,
}

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(x) if !x.trim().is_empty() => Some(x.trim().to_string()),
        Value::Number(x) => Some(x.to_string()),
        _ => None,
    }
}

fn number(v: &Value, field: &str) -> Result<Option<Decimal>> {
    match text(v) {
        None => Ok(None),
        Some(x) => Decimal::from_str(&x).map(Some).map_err(|_| Error::field(field, format!("{x} is not a number"))),
    }
}

fn asked(m: &serde_json::Map<String, Value>, date: &str) -> Result<Asked> {
    let get = |k: &str| m.get(k).unwrap_or(&Value::Null);
    let item = match text(get("item")) {
        Some(x) => Some(Uuid::from_str(&x).map_err(|_| Error::field("item", "is not an item"))?),
        None => None,
    };
    Ok(Asked {
        line: text(get("line")),
        item,
        description: text(get("description")),
        qty: number(get("qty"), "qty")?,
        price: number(get("price"), "price")?,
        date: text(get(date)),
    })
}

/// What was asked: `items` (a list of line changes, through the API), or one line's change from
/// the action's own fields (the dialog); a date given without a line moves the order's.
fn parse(params: &Value, date: &str) -> Result<(Vec<Asked>, Option<String>)> {
    let Value::Object(m) = params else { return Ok((vec![], None)) };
    let mut out = Vec::new();
    if let Some(Value::Array(list)) = m.get("items") {
        for v in list {
            let Value::Object(l) = v else { return Err(Error::field("items", "each change is an object")) };
            out.push(asked(l, date)?);
        }
        return Ok((out, m.get(date).and_then(text)));
    }
    let one = asked(m, date)?;
    if one.line.is_some() || one.item.is_some() || one.description.is_some() || one.qty.is_some() || one.price.is_some() {
        // In the dialog, the date is the order's (it is filled in with it), never one line's.
        let header = one.date.clone();
        out.push(Asked { date: None, ..one });
        return Ok((out, header));
    }
    Ok((out, one.date))
}

fn qty_text(q: Decimal) -> String {
    q.normalize().to_string()
}

pub async fn update_items(hc: &mut HookCtx<'_>, side: Side, doc: &Doc, params: &Value) -> Result<Value> {
    let n = names(side);
    let oid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let label = hc.entity.def.label.clone();
    let number = s(doc, "number").unwrap_or_default().to_string();
    match s(doc, "status") {
        Some("Closed") => return Err(Error::invalid(format!("{label} {number} is closed: re-open it to change it"))),
        Some("Completed") => return Err(Error::invalid(format!("{label} {number} is completed: everything on it was {} and billed", n.verb))),
        Some("Cancelled") => return Err(Error::invalid(format!("{label} {number} is cancelled: amend it into a new order"))),
        _ => {}
    }
    if d(doc, "per_billed") >= Decimal::ONE_HUNDRED {
        return Err(Error::invalid(format!("{label} {number} is fully billed: nothing on it can change; make a new order for more")));
    }
    let (asks, header_date) = parse(params, n.date)?;
    let mut items = rows(doc, "items");
    let mut removed: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let child = hc.entity.children.get("items").and_then(|c| hc.ctx.schema.entity(&c.entity)).cloned().ok_or_else(|| Error::internal("an order without lines"))?;
    for a in asks {
        let Some(line) = a.line.clone() else {
            add_line(hc, side, doc, &child, &mut items, &mut notes, a).await?;
            continue;
        };
        let at = match line.parse::<usize>() {
            Ok(i) if i >= 1 && i <= items.len() => Some(i - 1),
            _ => items.iter().position(|r| s(r, "id") == Some(line.as_str())),
        };
        let Some(i) = at else { return Err(Error::field("line", format!("there is no line {line} on {number}"))) };
        let r = &mut items[i];
        let what = line_label(i, r);
        let (qty, done, billed) = (d(r, "qty"), d(r, n.fulfil), d(r, "billed_qty"));
        let floor = done.max(billed);
        if a.item.is_some_and(|it| Some(it) != id(r, "item")) {
            return Err(Error::field("item", format!("{what}: a line's item does not change; remove the line and add one")));
        }
        if let Some(q) = a.qty {
            if q < Decimal::ZERO {
                return Err(Error::field("qty", format!("{what}: a quantity is not negative")));
            }
            if q.is_zero() {
                if floor > Decimal::ZERO {
                    return Err(Error::field(
                        "qty",
                        format!(
                            "{what} cannot be removed: {} {} and {} billed on it; cut it to {} instead",
                            qty_text(done),
                            n.verb,
                            qty_text(billed),
                            qty_text(floor)
                        ),
                    ));
                }
                removed.push(s(r, "id").unwrap_or_default().to_string());
                notes.push(format!("{what} removed ({} ordered)", qty_text(qty)));
                continue;
            }
            if q < floor - QTY_TOLERANCE {
                return Err(Error::field(
                    "qty",
                    format!("{what} cannot go below {}: {} {} and {} billed on it", qty_text(floor), qty_text(done), n.verb, qty_text(billed)),
                ));
            }
            if q != qty {
                set(r, "qty", money(q));
                notes.push(format!("{what}: quantity {} → {}", qty_text(qty), qty_text(q)));
            }
        }
        if let Some(p) = a.price {
            if side == Side::Selling && id(r, "item").is_some() {
                return Err(Error::field("price", format!("{what} takes its price from the price list and pricing rules")));
            }
            if p < Decimal::ZERO {
                return Err(Error::field("price", format!("{what}: a price is not negative")));
            }
            let was = d(r, "price_list_rate");
            if p != was {
                let now = d(r, "qty");
                if billed >= now - QTY_TOLERANCE {
                    return Err(Error::field("price", format!("the price of {what} cannot change: all {} on it are billed", qty_text(now))));
                }
                set(r, "price_list_rate", money(p));
                let part = if billed > Decimal::ZERO { format!(", for the {} not yet billed", qty_text(now - billed)) } else { String::new() };
                notes.push(format!("{what}: price {} → {}{part}", was.normalize(), p.normalize()));
            }
        }
        if let Some(day) = a.date
            && s(r, n.date) != Some(day.as_str())
        {
            notes.push(format!("{what}: {} {} → {day}", n.date.replace('_', " "), s(r, n.date).unwrap_or("none")));
            set(r, n.date, day);
        }
    }
    let mut new = doc.clone();
    if let Some(day) = header_date
        && s(doc, n.date) != Some(day.as_str())
    {
        // The lines that followed the order's date follow it to the new one.
        let was = s(doc, n.date).map(str::to_string);
        for r in items.iter_mut() {
            if s(r, n.date).is_none() || s(r, n.date) == was.as_deref() {
                set(r, n.date, day.clone());
            }
        }
        notes.push(format!("{}: {} → {day}", n.date.replace('_', " "), was.as_deref().unwrap_or("none")));
        set(&mut new, n.date, day);
    }
    items.retain(|r| !removed.iter().any(|x| s(r, "id") == Some(x.as_str())));
    if items.is_empty() {
        return Err(Error::invalid(format!("{number} keeps at least one line: close it instead")));
    }
    if notes.is_empty() {
        return Err(Error::invalid(format!("nothing on {number} changes: give a line's new quantity or price, a line to add, or a new date")));
    }
    set(&mut new, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
    let credit = if side == Side::Selling { credit::before(hc, doc, false).await? } else { None };

    // Judged as at submission, by every app's hooks.
    let hooks = hc.kernel.hooks.for_tenant(n.entity, &hc.ctx.schema.packages);
    for h in &hooks {
        h.before_validate(hc, &mut new).await?;
    }
    for h in &hooks {
        h.validate(hc, &new).await?;
        h.validate_change(hc, Some(doc), &new).await?;
    }
    for h in &hooks {
        h.before_submit(hc, &mut new).await?;
    }

    // Stored: what changed of the order's own fields, and its tables as they now are.
    let sctx = sys(hc.ctx);
    let ent = hc.entity.clone();
    let mut header = Doc::new();
    for (k, v) in &new {
        if doc.get(k) == Some(v) {
            continue;
        }
        let Some(f) = ent.field(k) else { continue };
        if f.ftype != FieldType::Table {
            header.insert(k.clone(), v.clone());
            continue;
        }
        let Some(cent) = ent.children.get(k).and_then(|c| hc.ctx.schema.entity(&c.entity)).cloned() else { continue };
        let table: Vec<Doc> = rows(&new, k)
            .into_iter()
            .map(|r| r.into_iter().filter(|(key, _)| key == "id" || cent.field(key).is_some()).collect())
            .collect();
        hc.kernel.set_child_rows_tx(&sctx, hc.tx, n.entity, k, oid, table).await?;
    }
    if !header.is_empty() {
        hc.kernel.set_values_tx(&sctx, hc.tx, n.entity, oid, header).await?;
    }
    refresh_order(hc.kernel, hc.ctx, hc.tx, n.entity, oid, n.fulfil, n.pct).await?;
    if side == Side::Selling
        && let Some(company) = id(doc, "company")
    {
        // Lines removed no longer reserve what they did.
        let gone: Vec<(Uuid, Uuid)> = rows(doc, "items")
            .iter()
            .filter(|r| removed.iter().any(|x| s(r, "id") == Some(x.as_str())))
            .filter_map(|r| Some((id(r, "item")?, id(r, "warehouse").or(id(doc, "set_warehouse"))?)))
            .collect();
        crate::reservation::refresh_pairs(hc.tx, hc.ctx.tenant, company, gone).await?;
    }
    let stored = hc.kernel.get_tx(&sctx, hc.tx, n.entity, oid, false).await?;
    for h in &hooks {
        h.after_save(hc, &stored, Some(doc)).await?;
    }
    credit::check(hc, credit, "this change", false).await?;
    let (was, now) = (d(doc, "grand_total"), d(&stored, "grand_total"));
    if was != now {
        notes.push(format!("grand total {} → {}", was.normalize(), now.normalize()));
    }
    let diff = ferrum_kernel::audit::diff(Some(doc), Some(&stored));
    ferrum_kernel::audit::record(hc.tx, hc.ctx, n.entity, oid, "update_items", None, json!({"changes": notes, "fields": diff})).await?;
    Ok(json!({"status": stored.get("status"), "grand_total": stored.get("grand_total"), "changes": notes}))
}

/// A line added to a submitted order: an item (its price from the price list and pricing rules on
/// a sale, as on any new line; typed or the usual source on a purchase), or a description with
/// its price. It takes the order's date and warehouse.
async fn add_line(hc: &mut HookCtx<'_>, side: Side, doc: &Doc, child: &ferrum_meta::EffEntity, items: &mut Vec<Doc>, notes: &mut Vec<String>, a: Asked) -> Result<()> {
    let n = names(side);
    let q = a.qty.unwrap_or_default();
    if q <= Decimal::ZERO {
        return Err(Error::field("qty", "a line added needs its quantity"));
    }
    let mut r = Doc::new();
    set(&mut r, "qty", money(q));
    if let Some(day) = a.date.or_else(|| s(doc, n.date).map(str::to_string)) {
        set(&mut r, n.date, day);
    }
    if let Some(w) = id(doc, "set_warehouse") {
        set(&mut r, "warehouse", w.to_string());
    }
    if let Some(text) = &a.description {
        set(&mut r, "description", text.clone());
    }
    let what = match a.item {
        Some(item) => {
            if side == Side::Selling && a.price.is_some() {
                return Err(Error::field("price", "an item's price comes from the price list and pricing rules: leave the price empty"));
            }
            let it = get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?;
            set(&mut r, "item", item.to_string());
            // What the line fetches from its item (its name, its stock unit), as a form would.
            for f in &child.def.fields {
                if let Some(from) = f.fetch_from.as_deref().and_then(|x| x.strip_prefix("item."))
                    && let Some(v) = it.get(from).filter(|v| !v.is_null())
                {
                    r.insert(f.name.clone(), v.clone());
                }
            }
            s(&it, "item_name").unwrap_or("the item").to_string()
        }
        None => {
            let Some(text) = a.description.as_deref() else {
                return Err(Error::field("item", "a line added names its item, or says what it is in its description"));
            };
            if a.price.is_none() {
                return Err(Error::field("price", "a line without an item needs its price"));
            }
            set(&mut r, "item_name", text.lines().next().unwrap_or(text).trim().to_string());
            text.lines().next().unwrap_or(text).trim().to_string()
        }
    };
    if let Some(p) = a.price {
        if p < Decimal::ZERO {
            return Err(Error::field("price", "a price is not negative"));
        }
        set(&mut r, "price_list_rate", money(p));
    }
    let price = a.price.map(|p| format!(" at {}", p.normalize())).unwrap_or_default();
    notes.push(format!("line {} added: {what}, {}{price}", items.len() + 1, qty_text(q)));
    items.push(r);
    Ok(())
}
