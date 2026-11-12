//! What a scanned text is (OP-31, the server's part): the company's read-only `scan` capability, for record
//! forms and other apps. Given the text a scanner typed, it answers every record it names: an item by its
//! barcode or code, a batch, a serial number, a bin, a document by its number (a carrier shipment by its
//! tracking number too), and a GS1 element string's item, batch, expiry, serial and quantity. Each match
//! says its item's unit and the quantity the scan stands for. Read with the caller's rights; bounded.

use crate::util::{id, s};
use chrono::Datelike;
use ferrum_kernel::{Action, Ctx, Doc, Error, Filter, Kernel, Result, Tx};
use serde_json::{Value, json};
use uuid::Uuid;

/// Longest text taken, and most matches answered.
const MAX_TEXT: usize = 200;
const MAX_MATCHES: usize = 20;
/// Documents found by their number.
const NUMBERED: [&str; 10] = [
    "SalesOrder",
    "DeliveryNote",
    "SalesInvoice",
    "PurchaseOrder",
    "PurchaseReceipt",
    "PurchaseInvoice",
    "StockEntry",
    "WorkOrder",
    "PickList",
    "CarrierShipment",
];

/// A GS1 element string's parts that matter for stock.
#[derive(Debug, Default, PartialEq)]
pub struct Gs1 {
    pub gtin: Option<String>,
    pub batch: Option<String>,
    pub expiry: Option<String>,
    pub serial: Option<String>,
    pub qty: Option<String>,
}

/// The length of an application identifier's data when it is fixed, `None` when it runs to a separator
/// (at most 30 characters); unknown identifiers are refused.
fn ai_length(ai: &str) -> Option<Option<usize>> {
    Some(match ai {
        "00" => Some(18),
        "01" | "02" => Some(14),
        "11" | "12" | "13" | "15" | "16" | "17" => Some(6),
        "20" => Some(2),
        "10" | "21" | "22" | "30" | "37" | "240" | "241" | "250" | "251" | "400" | "401" | "403" | "420" => None,
        a if a.len() == 4
            && (a.starts_with("31") || a.starts_with("32") || a.starts_with("33") || a.starts_with("34") || a.starts_with("35") || a.starts_with("36")) =>
        {
            Some(6)
        }
        _ => return None,
    })
}

fn ai_at(raw: &str) -> Option<&str> {
    [2, 3, 4].into_iter().filter_map(|n| raw.get(..n)).find(|a| a.chars().all(|c| c.is_ascii_digit()) && ai_length(a).is_some())
}

/// Reads a GS1 element string: human-readable (`(01)…(10)…`), or as scanned, after an optional symbology
/// identifier (`]C1`, `]d2`, `]e0`, `]Q3`), variable fields ended by the group separator. `None` when the text
/// is not one.
pub fn gs1(text: &str) -> Option<Gs1> {
    let mut parts: Vec<(String, String)> = Vec::new();
    if text.starts_with('(') {
        for chunk in text.split('(').skip(1) {
            let (ai, data) = chunk.split_once(')')?;
            ai_length(ai)?;
            parts.push((ai.to_string(), data.to_string()));
        }
    } else {
        let marked = ["]C1", "]d2", "]e0", "]Q3"].iter().find_map(|p| text.strip_prefix(p));
        let mut rest = marked.unwrap_or(text);
        if marked.is_none() && !(text.contains('\u{1d}') || (text.len() >= 16 && text.starts_with("01") && text[..16].chars().all(|c| c.is_ascii_digit()))) {
            return None;
        }
        while !rest.is_empty() {
            rest = rest.trim_start_matches('\u{1d}');
            if rest.is_empty() {
                break;
            }
            let ai = ai_at(rest)?;
            let after = &rest[ai.len()..];
            let (data, next) = match ai_length(ai)? {
                Some(n) => (after.get(..n)?, &after[n.min(after.len())..]),
                None => after.split_once('\u{1d}').unwrap_or((after, "")),
            };
            if data.is_empty() || data.chars().count() > 30 {
                return None;
            }
            parts.push((ai.to_string(), data.to_string()));
            rest = next;
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut g = Gs1::default();
    for (ai, data) in parts {
        match ai.as_str() {
            "01" | "02" => g.gtin = Some(data),
            "10" => g.batch = Some(data),
            "21" => g.serial = Some(data),
            "30" | "37" => g.qty = Some(data.trim_start_matches('0').to_string()).filter(|q| !q.is_empty()),
            "17" | "15" if g.expiry.is_none() || ai == "17" => {
                let (y, m, d) = (data.get(..2)?, data.get(2..4)?, data.get(4..6)?);
                // Day 00 is the month's last day.
                let first = chrono::NaiveDate::from_ymd_opt(2000 + y.parse::<i32>().ok()?, m.parse().ok()?, 1)?;
                let day = if d == "00" { (first + chrono::Months::new(1)).pred_opt()? } else { first.with_day0(d.parse::<u32>().ok()? - 1)? };
                g.expiry = Some(day.to_string());
            }
            _ => {}
        }
    }
    Some(g)
}

/// The forms a GTIN is kept in on an item's barcode: as read, and without its leading zeros down to 13, 12 or 8 digits.
fn gtin_forms(gtin: &str) -> Vec<String> {
    let mut out = vec![gtin.to_string()];
    for n in [13, 12, 8] {
        if gtin.len() > n && gtin[..gtin.len() - n].chars().all(|c| c == '0') {
            out.push(gtin[gtin.len() - n..].to_string());
        }
    }
    out
}

async fn found(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, filters: &[Filter]) -> Vec<Doc> {
    if ctx.schema.entity(entity).is_none() || ctx.require(entity, Action::Read).is_err() {
        return vec![];
    }
    k.find_tx(ctx, tx, entity, filters, None, 5).await.unwrap_or_default()
}

async fn item_match(k: &Kernel, ctx: &Ctx, tx: &mut Tx, item: Uuid) -> Value {
    let it = found(k, ctx, tx, "Item", &[Filter::eq("id", item.to_string())]).await;
    let it = it.first();
    json!({"item": item.to_string(), "item_name": it.and_then(|i| s(i, "item_name")), "uom": it.and_then(|i| s(i, "stock_uom"))})
}

fn with(mut base: Value, extra: Value) -> Value {
    if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
        b.extend(e.clone());
    }
    base
}

/// What `text` is, for `company`: `{text, matches: [{kind, entity, id, title, item, item_name, uom, qty, …}], gs1, ambiguous}`.
pub async fn resolve(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, text: &str) -> Result<Value> {
    let text = text.trim_matches(|c: char| c == '\r' || c == '\n' || c == ' ');
    if text.is_empty() {
        return Err(Error::field("text", "is required"));
    }
    if text.chars().count() > MAX_TEXT {
        return Err(Error::field("text", format!("is longer than {MAX_TEXT} characters: not a scan")));
    }
    let mut matches: Vec<Value> = Vec::new();
    let parsed = gs1(text);
    if let Some(g) = &parsed {
        let mut items = Vec::new();
        for form in g.gtin.as_deref().map(gtin_forms).unwrap_or_default() {
            for it in found(k, ctx, tx, "Item", &[Filter::eq("barcode", form)]).await {
                if let Some(i) = id(&it, "id").filter(|i| !items.contains(i)) {
                    items.push(i);
                }
            }
        }
        for item in items {
            let base = item_match(k, ctx, tx, item).await;
            matches.push(with(
                base,
                json!({"kind": "gs1", "entity": "Item", "id": item.to_string(), "qty": g.qty.clone().unwrap_or_else(|| "1".into()),
                       "batch_no": g.batch, "expiry": g.expiry, "serial_no": g.serial}),
            ));
        }
    }
    for field in ["barcode", "item_code"] {
        for it in found(k, ctx, tx, "Item", &[Filter::eq(field, text.to_string())]).await {
            let Some(item) = id(&it, "id") else { continue };
            if matches.iter().any(|m| m["kind"] == "item" && m["id"] == json!(item.to_string())) {
                continue;
            }
            matches.push(json!({"kind": "item", "entity": "Item", "id": item.to_string(), "title": s(&it, "item_name"), "item": item.to_string(),
                                "item_name": s(&it, "item_name"), "uom": s(&it, "stock_uom"), "qty": "1", "by": field}));
        }
    }
    for b in found(k, ctx, tx, "Batch", &[Filter::eq("batch_id", text.to_string())]).await {
        let Some(item) = id(&b, "item") else { continue };
        let base = item_match(k, ctx, tx, item).await;
        matches.push(with(
            base,
            json!({"kind": "batch", "entity": "Batch", "id": b.get("id"), "title": text, "qty": "1", "batch_no": text, "expiry": b.get("expiry_date")}),
        ));
    }
    if ctx.require("Item", Action::Read).is_ok() && ctx.schema.entity("Warehouse").is_some() {
        let serials: Vec<(Uuid, Option<Uuid>, String)> = sqlx::query_as(
            "select s.item_id, s.warehouse_id, s.status from serial_no s join e_item i on i.tenant_id = s.tenant_id and i.id = s.item_id \
             where s.tenant_id = $1 and s.serial = $2 and (s.warehouse_id is null or exists (select 1 from e_warehouse w where w.tenant_id = s.tenant_id \
             and w.id = s.warehouse_id and w.company = $3)) limit 5",
        )
        .bind(ctx.tenant)
        .bind(text)
        .bind(company)
        .fetch_all(&mut **tx)
        .await?;
        for (item, warehouse, status) in serials {
            let base = item_match(k, ctx, tx, item).await;
            matches.push(with(base, json!({"kind": "serial", "title": text, "qty": "1", "serial_no": text, "warehouse": warehouse, "status": status})));
        }
    }
    for b in found(k, ctx, tx, "StorageBin", &[Filter::eq("bin_code", text.to_string())]).await {
        matches.push(json!({"kind": "bin", "entity": "StorageBin", "id": b.get("id"), "title": text, "warehouse": b.get("warehouse")}));
    }
    for entity in NUMBERED {
        let scoped = ctx.schema.entity(entity).is_some_and(|e| e.field("company").is_some());
        let mut keys = vec!["number"];
        if entity == "CarrierShipment" {
            keys.push("tracking_number");
        }
        for key in keys {
            let mut filters = vec![Filter::eq(key, text.to_string())];
            if scoped {
                filters.push(Filter::eq("company", company.to_string()));
            }
            for d in found(k, ctx, tx, entity, &filters).await {
                matches.push(json!({"kind": "document", "entity": entity, "id": d.get("id"), "title": d.get("number"), "status": d.get("status"), "docstatus": d.get("docstatus")}));
            }
        }
    }
    matches.truncate(MAX_MATCHES);
    let ambiguous = matches.len() > 1;
    Ok(json!({"text": text, "gs1": parsed.map(|g| json!({"gtin": g.gtin, "batch_no": g.batch, "expiry": g.expiry, "serial_no": g.serial, "qty": g.qty})),
              "matches": matches, "ambiguous": ambiguous}))
}

/// The company's `scan` capability.
pub struct ScanHook;

#[async_trait::async_trait]
impl ferrum_kernel::DocHook for ScanHook {
    async fn action(&self, hc: &mut ferrum_kernel::HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "scan" {
            return Ok(None);
        }
        let company = id(doc, "id").ok_or_else(|| Error::internal("company without id"))?;
        let text = params.get("text").and_then(Value::as_str).unwrap_or_default();
        resolve(hc.kernel, hc.ctx, hc.tx, company, text).await.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn element_strings_are_read_in_both_forms() {
        let want =
            Gs1 { gtin: Some("09501101530003".into()), batch: Some("AB-12".into()), expiry: Some("2026-12-31".into()), serial: None, qty: Some("24".into()) };
        assert_eq!(gs1("(01)09501101530003(17)261231(10)AB-12(30)0024"), Some(want));
        let raw = gs1("]C1010950110153000317261200").unwrap();
        assert_eq!((raw.gtin.as_deref(), raw.expiry.as_deref()), (Some("09501101530003"), Some("2026-12-31")), "day 00 is the month's last");
        let raw = gs1("0109501101530003").unwrap();
        assert_eq!(raw.gtin.as_deref(), Some("09501101530003"));
        let raw = gs1("010950110153000310LOT7\u{1d}21SN9").unwrap();
        assert_eq!((raw.batch.as_deref(), raw.serial.as_deref()), (Some("LOT7"), Some("SN9")));
        assert_eq!(gs1("WIDGET-1"), None);
        assert_eq!(gs1("(99)x"), None, "an unknown identifier is not read");
        assert_eq!(gs1("]C10109501101"), None, "cut short");
        assert_eq!(gtin_forms("00012345678905"), vec!["00012345678905", "0012345678905", "012345678905"]);
    }
}
