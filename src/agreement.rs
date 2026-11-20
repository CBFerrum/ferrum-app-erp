//! Purchase and sales agreements (blanket orders, docs/agreements.md): items with an agreed quantity and price for a
//! period. An order line that names one takes its price and counts against its quantity. What is ordered is counted
//! from the submitted orders under the agreement's row lock each time an order changes, never kept as a running total.

use crate::flow::{QTY_TOLERANCE, create_for_user, created};
use crate::pricing::Side;
use crate::util::{d, date, id, money, rows, s, sys};
use async_trait::async_trait;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

struct Names {
    agreement: &'static str,
    order: &'static str,
    party: &'static str,
}

fn names(side: Side) -> &'static Names {
    match side {
        Side::Selling => &Names { agreement: "SalesAgreement", order: "SalesOrder", party: "customer" },
        Side::Buying => &Names { agreement: "PurchaseAgreement", order: "PurchaseOrder", party: "supplier" },
    }
}

/// Why `a` cannot be drawn on by an order of `company`, `party` dated `on`; `None` when it can.
fn unusable(a: &Doc, n: &Names, company: Option<Uuid>, party: Option<Uuid>, on: &str) -> Option<String> {
    let number = s(a, "number").unwrap_or_default();
    if d(a, "docstatus") != Decimal::ONE {
        return Some(format!("{number} is not a submitted agreement"));
    }
    if s(a, "status") == Some("Closed") {
        return Some(format!("{number} is closed"));
    }
    if id(a, "company") != company || id(a, n.party) != party {
        return Some(format!("{number} is with another {} or company", n.party));
    }
    let (from, to) = (s(a, "from_date").unwrap_or_default(), s(a, "to_date").unwrap_or_default());
    if on < from || on > to {
        return Some(format!("{number} holds from {from} to {to}, not on {on}"));
    }
    None
}

/// Prices an order line that names an agreement (inside pricing's line loop): its agreed price per stock unit times
/// the line's `factor`, and the agreement line it draws on. False when the line names none. Pricing rules are not
/// applied to an agreed price.
pub async fn price_line(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut Tx,
    side: Side,
    doc: &Doc,
    row: &mut Doc,
    i: usize,
    factor: Decimal,
    cache: &mut HashMap<Uuid, Doc>,
) -> Result<bool> {
    let Some(aid) = id(row, "agreement") else { return Ok(false) };
    let n = names(side);
    let at = |msg: String| Error::field(format!("items.{i}.agreement"), msg);
    if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(aid) {
        e.insert(k.get_tx(&sys(ctx), tx, n.agreement, aid, false).await.map_err(|_| at("is not an agreement".into()))?);
    }
    let a = &cache[&aid];
    let on = date(doc, "posting_date")?.format("%Y-%m-%d").to_string();
    if let Some(why) = unusable(a, n, id(doc, "company"), id(doc, n.party), &on) {
        return Err(at(why));
    }
    let number = s(a, "number").unwrap_or_default();
    let item = id(row, "item");
    let Some(line) = rows(a, "items").into_iter().find(|l| id(l, "item") == item) else {
        return Err(at(format!("{number} has no line for {}", s(row, "item_name").or(s(row, "item")).unwrap_or_default())));
    };
    if id(a, "currency") != id(doc, "currency") {
        return Err(at(format!("{number} is in another currency than this order: make the order in the agreement's currency")));
    }
    row.insert("price_list_rate".into(), money((d(&line, "rate") * factor).round_dp(9)));
    row.insert("agreement_line".into(), json!(s(&line, "id")));
    Ok(true)
}

/// Recounts what `order`'s agreements have drawn, each under its row lock in id order. With `check` (a submit, Update
/// Items, a re-open), an order that cannot draw on one, or would bring what is ordered past what was agreed, is
/// refused.
pub async fn drawn(hc: &mut HookCtx<'_>, side: Side, order: &Doc, check: bool) -> Result<()> {
    let mut ids: Vec<Uuid> = rows(order, "items").iter().filter_map(|r| id(r, "agreement")).collect();
    ids.sort();
    ids.dedup();
    let n = names(side);
    for aid in ids {
        let a = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, n.agreement, aid, true).await?;
        if check {
            let on = date(order, "posting_date")?.format("%Y-%m-%d").to_string();
            if let Some(why) = unusable(&a, n, id(order, "company"), id(order, n.party), &on) {
                return Err(Error::invalid(why));
            }
        }
        recount(hc, n, &a, check).await?;
    }
    Ok(())
}

/// Sets each line's `ordered_qty`, `per_ordered` and the status of the (locked) agreement `a` from the submitted orders.
async fn recount(hc: &mut HookCtx<'_>, n: &Names, a: &Doc, check: bool) -> Result<String> {
    let aid = id(a, "id").ok_or_else(|| Error::internal("no id"))?;
    let ent = hc.ctx.schema.entity(n.order).ok_or_else(|| Error::internal("no order entity"))?;
    let lines = ent.children.get("items").map(|c| c.table.clone()).ok_or_else(|| Error::internal("no order lines"))?;
    let q = ferrum_kernel::ddl::q;
    let ordered: Vec<(String, Decimal)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select l.agreement_line, sum((l.qty - coalesce(l.closed_qty, 0)) * coalesce(nullif(l.conversion_factor, 0), 1)) \
         from {o} o join {l} l on l.tenant_id = o.tenant_id and l.parent_id = o.id \
         where o.tenant_id = $1 and o.docstatus = 1 and l.agreement = $2 and l.agreement_line is not null group by 1",
        o = q(&ent.table),
        l = q(&lines),
    )))
    .bind(hc.ctx.tenant)
    .bind(aid)
    .fetch_all(&mut **hc.tx)
    .await?;
    let ordered: HashMap<String, Decimal> = ordered.into_iter().collect();
    let number = s(a, "number").unwrap_or_default();
    let (mut agreed, mut met, mut done) = (Decimal::ZERO, Decimal::ZERO, true);
    let sctx = sys(hc.ctx);
    for line in rows(a, "items") {
        let Some(lid) = id(&line, "id") else { continue };
        let o = ordered.get(&lid.to_string()).copied().unwrap_or_default();
        let qty = d(&line, "qty");
        if check && o > qty + QTY_TOLERANCE {
            let item = s(&line, "item_name").unwrap_or_default();
            return Err(Error::invalid(format!("{item}: {number} agreed {}; this would bring what is ordered to {}", qty.normalize(), o.normalize())));
        }
        agreed += qty;
        met += o.min(qty);
        done &= o + QTY_TOLERANCE >= qty;
        let mut v = Doc::new();
        v.insert("ordered_qty".into(), money(o));
        hc.kernel.set_child_values_tx(&sctx, hc.tx, n.agreement, "items", lid, v).await?;
    }
    let status = if s(a, "status") == Some("Closed") {
        "Closed"
    } else if done {
        "Fulfilled"
    } else {
        "Open"
    };
    let mut v = Doc::new();
    v.insert("per_ordered".into(), money(if agreed.is_zero() { Decimal::ZERO } else { (met * Decimal::from(100) / agreed).round_dp(2) }));
    v.insert("status".into(), json!(status));
    hc.kernel.set_values_tx(&sctx, hc.tx, n.agreement, aid, v).await?;
    Ok(status.to_string())
}

pub struct AgreementHook(pub Side);

#[async_trait]
impl DocHook for AgreementHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if d(doc, "docstatus").is_zero() {
            doc.insert("status".into(), json!("Draft"));
        }
        if id(doc, "currency").is_none()
            && let Some(c) = id(doc, "company")
        {
            let co = crate::util::company(hc.kernel, hc.ctx, hc.tx, c).await?;
            doc.insert("currency".into(), json!(co.currency_id.to_string()));
        }
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut seen = HashSet::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            if !seen.insert(id(r, "item")) {
                return Err(Error::field(format!("items.{i}.item"), "is on another line of this agreement: one line per item"));
            }
        }
        Ok(())
    }

    async fn before_submit(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        doc.insert("status".into(), json!("Open"));
        Ok(())
    }

    async fn before_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let n = names(self.0);
        let ent = hc.ctx.schema.entity(n.order).ok_or_else(|| Error::internal("no order entity"))?;
        let lines = ent.children.get("items").map(|c| c.table.clone()).ok_or_else(|| Error::internal("no order lines"))?;
        let q = ferrum_kernel::ddl::q;
        let used: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select o.number from {o} o join {l} l on l.tenant_id = o.tenant_id and l.parent_id = o.id \
             where o.tenant_id = $1 and o.docstatus = 1 and l.agreement = $2 limit 1",
            o = q(&ent.table),
            l = q(&lines),
        )))
        .bind(hc.ctx.tenant)
        .bind(id(doc, "id"))
        .fetch_optional(&mut **hc.tx)
        .await?;
        match used {
            Some(o) => Err(Error::invalid(format!("{o} draws on this agreement: cancel or change the orders that draw on it first"))),
            None => Ok(()),
        }
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut v = Doc::new();
        v.insert("status".into(), json!("Cancelled"));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, names(self.0).agreement, id(doc, "id").unwrap_or_default(), v).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        let n = names(self.0);
        let aid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let number = s(doc, "number").unwrap_or_default().to_string();
        match name {
            "make_order" => {
                let a = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, n.agreement, aid, true).await?;
                if s(&a, "status") != Some("Open") {
                    return Err(Error::invalid(format!("{number} is not open")));
                }
                let lines: Vec<Value> = rows(&a, "items")
                    .iter()
                    .filter(|l| d(l, "qty") - d(l, "ordered_qty") > QTY_TOLERANCE)
                    .map(|l| json!({"item": l.get("item"), "qty": money(d(l, "qty") - d(l, "ordered_qty")), "agreement": aid.to_string()}))
                    .collect();
                if lines.is_empty() {
                    return Err(Error::invalid(format!("everything on {number} is ordered")));
                }
                let mut input = Doc::new();
                for f in ["company", "currency", n.party] {
                    input.insert(f.into(), a.get(f).cloned().unwrap_or(Value::Null));
                }
                input.insert("items".into(), Value::Array(lines));
                let order = create_for_user(hc, n.order, input).await?;
                Ok(Some(created(n.order, &order)))
            }
            "close" | "reopen" => {
                let closing = name == "close";
                let a = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, n.agreement, aid, true).await?;
                if closing == (s(&a, "status") == Some("Closed")) {
                    return Err(Error::invalid(if closing { format!("{number} is already closed") } else { format!("{number} is not closed") }));
                }
                let mut v = Doc::new();
                v.insert("status".into(), json!(if closing { "Closed" } else { "Open" }));
                hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, n.agreement, aid, v).await?;
                let a = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, n.agreement, aid, false).await?;
                let status = recount(hc, n, &a, false).await?;
                Ok(Some(json!({"status": status})))
            }
            _ => Ok(None),
        }
    }
}
