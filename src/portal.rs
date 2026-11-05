//! ERP's pages of the workspace's portal (docs/plan/apps-ui.md §9.1): a customer's own sales
//! invoices, at `/api/app/selling/portal/invoices…` ("Your invoices" at `/portal/invoices/`), and
//! a supplier's own purchase orders and the bills ERP holds from them, at
//! `/api/app/buying/portal/…` ("Your orders" at `/portal/orders/`). Portal people hold no roles:
//! what is theirs is what belongs to the customers or suppliers their address belongs to (the
//! party's own email, or one of its contacts'), read as the system once that is settled. Drafts
//! and cancelled documents are not theirs to see.

use crate::util::{b, d, id, rows, s, sys};
use async_trait::async_trait;
use ferrum_kernel::{Ctx, Doc, Error, Filter, Kernel, ListQuery, Op, Result, Site, SiteRequest, SiteResponse, Tx};
use serde_json::{Value, json};
use uuid::Uuid;

pub struct SellingPortal;

#[async_trait]
impl Site for SellingPortal {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        let user = req.user.as_ref().ok_or_else(|| Error::Unauthorized("sign in first".into()))?;
        let segs: Vec<&str> = req.path.split('/').filter(|p| !p.is_empty()).collect();
        let query = |n: &str| req.query.get(n).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
        let invoice = |raw: &str| raw.parse::<Uuid>().map_err(|_| Error::NotFound("invoice".into()));
        let out = match (req.method.as_str(), segs.as_slice()) {
            ("GET", ["portal", "invoices"]) => list(k, user, query("cursor")).await?,
            ("GET", ["portal", "invoices", inv]) => detail(k, user, invoice(inv)?).await?,
            ("GET", ["portal", "invoices", inv, "print"]) => {
                let inv = own(k, user, invoice(inv)?).await?;
                return print(k, user, "SalesInvoice", &inv, query("format") == Some("pdf")).await;
            }
            _ => return Err(Error::NotFound(format!("selling endpoint {} {}", req.method, req.path))),
        };
        let mut r = SiteResponse::json(200, &out);
        r.headers.push(("cache-control".into(), "no-store".into()));
        Ok(r)
    }
}

/// The customers an address belongs to: those with it as their email, and those of contacts
/// with it; never disabled ones, nor the guests a web shop records unchecked (anyone may type
/// anyone's address at a guest checkout).
pub async fn customers_of(k: &Kernel, ctx: &Ctx, tx: &mut Tx, email: &str) -> Result<Vec<Uuid>> {
    parties_of(k, ctx, tx, email, "Customer", "customer").await
}

/// The suppliers an address belongs to, as [`customers_of`] finds customers.
pub async fn suppliers_of(k: &Kernel, ctx: &Ctx, tx: &mut Tx, email: &str) -> Result<Vec<Uuid>> {
    parties_of(k, ctx, tx, email, "Supplier", "supplier").await
}

/// The parties of `entity` an address belongs to: with it as their email, or through a contact
/// whose `link` field names them.
async fn parties_of(k: &Kernel, ctx: &Ctx, tx: &mut Tx, email: &str, entity: &str, link: &str) -> Result<Vec<Uuid>> {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return Ok(vec![]);
    }
    let sctx = sys(ctx);
    let usable = |c: &Doc| !b(c, "disabled") && !b(c, "commerce_guest");
    let mut out: Vec<Uuid> = k
        .find_tx(&sctx, tx, entity, &[Filter::eq("email", email.clone())], Some("created_at asc"), 50)
        .await?
        .iter()
        .filter(|c| usable(c))
        .filter_map(|c| id(c, "id"))
        .collect();
    if sctx.schema.entity("Contact").is_some_and(|c| c.field("emails").is_some() && c.field(link).is_some()) {
        for c in k.find_tx(&sctx, tx, "Contact", &[Filter::eq("emails.email", email)], Some("created_at asc"), 50).await? {
            let Some(party) = id(&c, link).filter(|p| !out.contains(p)) else { continue };
            if let Ok(doc) = k.get_tx(&sctx, tx, entity, party, false).await
                && usable(&doc)
            {
                out.push(party);
            }
        }
    }
    Ok(out)
}

fn titled<'a>(doc: &'a Doc, field: &str) -> Option<&'a str> {
    doc.get("_titles").and_then(|t| t.get(field)).and_then(Value::as_str)
}

/// A document's total as it is paid: rounded, where it was.
fn total(doc: &Doc) -> rust_decimal::Decimal {
    if doc.get("rounded_total").is_some_and(|v| !v.is_null()) && !d(doc, "rounded_total").is_zero() { d(doc, "rounded_total") } else { d(doc, "grand_total") }
}

fn summary(inv: &Doc, today: &str) -> Value {
    let total = total(inv);
    let outstanding = d(inv, "outstanding_amount");
    json!({
        "id": s(inv, "id"),
        "number": s(inv, "number"),
        "date": s(inv, "posting_date"),
        "due_date": s(inv, "due_date"),
        "status": s(inv, "status"),
        "is_return": b(inv, "is_return"),
        "customer": titled(inv, "customer").or(s(inv, "customer_name")),
        "currency": titled(inv, "currency"),
        "total": total.normalize().to_string(),
        "outstanding": outstanding.normalize().to_string(),
        "overdue": outstanding > rust_decimal::Decimal::ZERO && s(inv, "due_date").is_some_and(|due| due < today),
    })
}

/// `GET portal/invoices`: the caller's customers' invoices, newest first, twenty at a time.
async fn list(k: &Kernel, user: &Ctx, cursor: Option<&str>) -> Result<Value> {
    let mut tx = k.db.tenant_tx(user.tenant).await?;
    let mine = customers_of(k, user, &mut tx, &user.email).await?;
    tx.commit().await?;
    if mine.is_empty() {
        return Ok(json!({"invoices": [], "next_cursor": null}));
    }
    let q = ListQuery {
        filters: vec![
            Filter { field: "customer".into(), op: Op::In, value: json!(mine.iter().map(Uuid::to_string).collect::<Vec<_>>()) },
            Filter::eq("docstatus", 1),
        ],
        sort: Some("posting_date desc".into()),
        limit: Some(20),
        cursor: cursor.map(str::to_string),
        with_titles: true,
        ..Default::default()
    };
    let page = k.list(&sys(user), "SalesInvoice", &q).await?;
    let today = user.today().to_string();
    Ok(json!({"invoices": page.rows.iter().map(|r| summary(r, &today)).collect::<Vec<_>>(), "next_cursor": page.next_cursor}))
}

/// The invoice when it is one of the caller's customers' and submitted; anything else reads as missing.
async fn own(k: &Kernel, user: &Ctx, invoice: Uuid) -> Result<Doc> {
    let sctx = sys(user);
    let mut tx = k.db.tenant_tx(user.tenant).await?;
    let mine = customers_of(k, user, &mut tx, &user.email).await?;
    tx.commit().await?;
    let missing = || Error::NotFound("invoice".into());
    let inv = match k.get(&sctx, "SalesInvoice", invoice, true).await {
        Ok(inv) => inv,
        Err(Error::NotFound(_)) => return Err(missing()),
        Err(e) => return Err(e),
    };
    let theirs = id(&inv, "customer").is_some_and(|c| mine.contains(&c));
    if !theirs || inv.get("docstatus").and_then(Value::as_i64) != Some(1) {
        return Err(missing());
    }
    Ok(inv)
}

/// `GET portal/invoices/{id}`: an invoice with its lines and taxes.
async fn detail(k: &Kernel, user: &Ctx, invoice: Uuid) -> Result<Value> {
    let inv = own(k, user, invoice).await?;
    let mut view = summary(&inv, &user.today().to_string());
    let lines: Vec<Value> = rows(&inv, "items")
        .iter()
        .map(|l| {
            json!({
                "item": s(l, "item_name").or(titled(l, "item")),
                "description": s(l, "description"),
                "qty": d(l, "qty").normalize().to_string(),
                "unit": titled(l, "uom"),
                "rate": d(l, "rate").normalize().to_string(),
                "amount": d(l, "amount").normalize().to_string(),
            })
        })
        .collect();
    let more = json!({
        "company": titled(&inv, "company"),
        "lines": lines,
        "taxes": taxes_of(&inv),
        "net_total": d(&inv, "net_total").normalize().to_string(),
        "total_taxes": d(&inv, "total_taxes").normalize().to_string(),
        "grand_total": d(&inv, "grand_total").normalize().to_string(),
        "pdf": k.pdf_enabled(),
    });
    if let (Value::Object(v), Value::Object(m)) = (&mut view, more) {
        v.extend(m);
    }
    Ok(view)
}

/// `GET portal/…/{id}/print[?format=pdf]`: a document of the caller's as the workspace prints it.
async fn print(k: &Kernel, user: &Ctx, entity: &str, doc: &Doc, pdf: bool) -> Result<SiteResponse> {
    let sctx = sys(user);
    let doc_id = id(doc, "id").ok_or_else(|| Error::NotFound("document".into()))?;
    let name = s(doc, "number").unwrap_or("document").replace(['"', '\\', '/', '\r', '\n'], "");
    let mut r = if pdf {
        let bytes = k.print_pdf(&sctx, entity, doc_id).await?;
        let mut r = SiteResponse { status: 200, content_type: "application/pdf".into(), headers: vec![], body: bytes };
        r.headers.push(("content-disposition".into(), format!("inline; filename=\"{name}.pdf\"")));
        r
    } else {
        SiteResponse::text(200, "text/html; charset=utf-8", k.render_print(&sctx, entity, doc_id).await?)
    };
    // A page of the workspace's own making, kept from running anything.
    r.headers.push((
        "content-security-policy".into(),
        "default-src 'none'; style-src 'unsafe-inline'; img-src 'self' data: https:; font-src 'self' data:; sandbox".into(),
    ));
    r.headers.push(("x-content-type-options".into(), "nosniff".into()));
    r.headers.push(("cache-control".into(), "private, no-store".into()));
    Ok(r)
}

/// A document's taxes, as its party reads them.
fn taxes_of(doc: &Doc) -> Vec<Value> {
    rows(doc, "taxes")
        .iter()
        .filter(|t| !d(t, "tax_amount").is_zero())
        .map(|t| {
            // A reverse charge is the buyer's to account for: shown, not charged.
            let description = s(t, "description").map(|x| if b(t, "reverse_charge") { format!("{x} (reverse charge)") } else { x.to_string() });
            json!({"description": description, "amount": (if b(t, "deduct") { -d(t, "tax_amount") } else { d(t, "tax_amount") }).normalize().to_string()})
        })
        .collect()
}

// ───────────────────────── a supplier's orders and bills ─────────────────────────

pub struct BuyingPortal;

#[async_trait]
impl Site for BuyingPortal {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        let user = req.user.as_ref().ok_or_else(|| Error::Unauthorized("sign in first".into()))?;
        let segs: Vec<&str> = req.path.split('/').filter(|p| !p.is_empty()).collect();
        let query = |n: &str| req.query.get(n).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
        let order = |raw: &str| raw.parse::<Uuid>().map_err(|_| Error::NotFound("order".into()));
        let out = match (req.method.as_str(), segs.as_slice()) {
            ("GET", ["portal", "orders"]) => orders(k, user, query("cursor")).await?,
            ("GET", ["portal", "orders", o]) => order_detail(k, user, order(o)?).await?,
            ("GET", ["portal", "orders", o, "print"]) => {
                let po = own_order(k, user, order(o)?).await?;
                return print(k, user, "PurchaseOrder", &po, query("format") == Some("pdf")).await;
            }
            ("GET", ["portal", "bills"]) => bills(k, user, query("cursor")).await?,
            _ => return Err(Error::NotFound(format!("buying endpoint {} {}", req.method, req.path))),
        };
        let mut r = SiteResponse::json(200, &out);
        r.headers.push(("cache-control".into(), "no-store".into()));
        Ok(r)
    }
}

/// The caller's suppliers, read in a transaction of their own.
async fn my_suppliers(k: &Kernel, user: &Ctx) -> Result<Vec<Uuid>> {
    let mut tx = k.db.tenant_tx(user.tenant).await?;
    let mine = suppliers_of(k, user, &mut tx, &user.email).await?;
    tx.commit().await?;
    Ok(mine)
}

/// Submitted documents of `entity` for the caller's suppliers, newest first, twenty at a time.
async fn of_my_suppliers(k: &Kernel, user: &Ctx, entity: &str, cursor: Option<&str>) -> Result<Option<ferrum_kernel::Page>> {
    let mine = my_suppliers(k, user).await?;
    if mine.is_empty() {
        return Ok(None);
    }
    let q = ListQuery {
        filters: vec![
            Filter { field: "supplier".into(), op: Op::In, value: json!(mine.iter().map(Uuid::to_string).collect::<Vec<_>>()) },
            Filter::eq("docstatus", 1),
        ],
        sort: Some("posting_date desc".into()),
        limit: Some(20),
        cursor: cursor.map(str::to_string),
        with_titles: true,
        ..Default::default()
    };
    Ok(Some(k.list(&sys(user), entity, &q).await?))
}

fn order_summary(po: &Doc) -> Value {
    json!({
        "id": s(po, "id"),
        "number": s(po, "number"),
        "date": s(po, "posting_date"),
        "required_by": s(po, "schedule_date"),
        "status": s(po, "status"),
        "supplier": titled(po, "supplier").or(s(po, "supplier_name")),
        "currency": titled(po, "currency"),
        "total": total(po).normalize().to_string(),
        "received": d(po, "per_received").normalize().to_string(),
        "billed": d(po, "per_billed").normalize().to_string(),
    })
}

/// `GET portal/orders`: what the caller's suppliers were ordered to supply.
async fn orders(k: &Kernel, user: &Ctx, cursor: Option<&str>) -> Result<Value> {
    Ok(match of_my_suppliers(k, user, "PurchaseOrder", cursor).await? {
        Some(page) => json!({"orders": page.rows.iter().map(order_summary).collect::<Vec<_>>(), "next_cursor": page.next_cursor}),
        None => json!({"orders": [], "next_cursor": null}),
    })
}

/// The order when it is one of the caller's suppliers' and submitted; anything else reads as missing.
async fn own_order(k: &Kernel, user: &Ctx, order: Uuid) -> Result<Doc> {
    let mine = my_suppliers(k, user).await?;
    let missing = || Error::NotFound("order".into());
    let po = match k.get(&sys(user), "PurchaseOrder", order, true).await {
        Ok(po) => po,
        Err(Error::NotFound(_)) => return Err(missing()),
        Err(e) => return Err(e),
    };
    if !id(&po, "supplier").is_some_and(|sup| mine.contains(&sup)) || po.get("docstatus").and_then(Value::as_i64) != Some(1) {
        return Err(missing());
    }
    Ok(po)
}

/// `GET portal/orders/{id}`: an order with its lines, what was received of each, and its taxes.
async fn order_detail(k: &Kernel, user: &Ctx, order: Uuid) -> Result<Value> {
    let po = own_order(k, user, order).await?;
    let mut view = order_summary(&po);
    let lines: Vec<Value> = rows(&po, "items")
        .iter()
        .map(|l| {
            json!({
                "item": s(l, "item_name").or(titled(l, "item")),
                "description": s(l, "description"),
                "qty": d(l, "qty").normalize().to_string(),
                "received": d(l, "received_qty").normalize().to_string(),
                "unit": titled(l, "uom"),
                "rate": d(l, "rate").normalize().to_string(),
                "amount": d(l, "amount").normalize().to_string(),
                "required_by": s(l, "schedule_date"),
            })
        })
        .collect();
    let more = json!({
        "company": titled(&po, "company"),
        "lines": lines,
        "taxes": taxes_of(&po),
        "net_total": d(&po, "net_total").normalize().to_string(),
        "grand_total": d(&po, "grand_total").normalize().to_string(),
        "pdf": k.pdf_enabled(),
    });
    if let (Value::Object(v), Value::Object(m)) = (&mut view, more) {
        v.extend(m);
    }
    Ok(view)
}

/// `GET portal/bills`: the caller's suppliers' bills as ERP holds them, and what is still owed on
/// each.
async fn bills(k: &Kernel, user: &Ctx, cursor: Option<&str>) -> Result<Value> {
    let Some(page) = of_my_suppliers(k, user, "PurchaseInvoice", cursor).await? else {
        return Ok(json!({"bills": [], "next_cursor": null}));
    };
    let today = user.today().to_string();
    let bills: Vec<Value> = page
        .rows
        .iter()
        .map(|pi| {
            let owed = d(pi, "outstanding_amount");
            json!({
                "id": s(pi, "id"),
                "bill_no": s(pi, "bill_no"),
                "bill_date": s(pi, "bill_date").or(s(pi, "posting_date")),
                "due_date": s(pi, "due_date"),
                "status": s(pi, "status"),
                "is_return": b(pi, "is_return"),
                "supplier": titled(pi, "supplier"),
                "currency": titled(pi, "currency"),
                "total": total(pi).normalize().to_string(),
                "owed": owed.normalize().to_string(),
                "overdue": owed > rust_decimal::Decimal::ZERO && s(pi, "due_date").is_some_and(|due| due < today.as_str()),
            })
        })
        .collect();
    Ok(json!({"bills": bills, "next_cursor": page.next_cursor}))
}
