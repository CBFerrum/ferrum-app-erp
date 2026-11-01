//! Quality: holds, nonconformance, corrective action and inspection plans. A hold is enforced where
//! stock leaves or is consumed, by a gate on the documents that do it, so no app has to remember to
//! check, and it is released only by someone with the authority who did not place it.

use async_trait::async_trait;
use ferrum_domain::reports::ReportSource;
use ferrum_domain::util::{id, rows, s, sys};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, HookCtx, HookRegistry, Kernel, Result, Site, SiteRequest, SiteResponse, Tx, audit};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// This app's manifest (`app.yaml`).
pub const MANIFEST: &str = include_str!("../app.yaml");

/// This app's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers this app's document hooks and endpoints.
pub fn register(h: &mut HookRegistry) {
    for entity in ["DeliveryNote", "SalesInvoice", "StockEntry", "PickList"] {
        h.register(entity, Arc::new(HoldGate(entity)));
    }
    h.register("QualityHold", Arc::new(HoldHook)).register("InspectionPlan", Arc::new(PlanHook));
    h.add_site("quality", Arc::new(QualitySite));
}

pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![]
}

pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}

/// The active hold over this item and lot, if any. An empty lot on the hold covers every lot.
pub async fn hold_on(tx: &mut Tx, tenant: Uuid, item: Uuid, lot: &str) -> Result<Option<(Uuid, Option<String>)>> {
    Ok(sqlx::query_as(
        "select id, rework_order from e_quality_hold where tenant_id = $1 and item = $2 and status = 'Active' and (coalesce(lot, '') = '' or lot = $3) limit 1",
    )
    .bind(tenant)
    .bind(item)
    .bind(lot)
    .fetch_optional(&mut **tx)
    .await?)
}

/// Refuses documents that would ship or consume held stock. Receipts pass: stock may come in on
/// hold, and a hold must never stop anyone taking goods off a lorry.
struct HoldGate(&'static str);

#[async_trait]
impl DocHook for HoldGate {
    fn name(&self) -> &'static str {
        "quality.hold_gate"
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if self.0 == "SalesInvoice" && !ferrum_domain::util::b(doc, "update_stock") {
            return Ok(());
        }
        if matches!(self.0, "DeliveryNote" | "SalesInvoice") && ferrum_domain::util::b(doc, "is_return") {
            return Ok(());
        }
        let (table, lot_field) = if self.0 == "PickList" { ("locations", "batch_no") } else { ("items", "batch_no") };
        let work_order = s(doc, "mfg_work_order").map(str::to_string);
        for line in rows(doc, table) {
            // A line that only receives into a warehouse takes nothing from anywhere.
            if self.0 == "StockEntry" && line.get("source_warehouse").is_none_or(Value::is_null) {
                continue;
            }
            let Some(item) = id(&line, "item") else { continue };
            let lot = s(&line, lot_field).unwrap_or_default();
            if let Some((_, rework)) = hold_on(hc.tx, hc.ctx.tenant, item, lot).await? {
                if rework.is_some() && rework == work_order {
                    continue;
                }
                let code: Option<String> = sqlx::query_scalar("select item_code from e_item where tenant_id = $1 and id = $2")
                    .bind(hc.ctx.tenant)
                    .bind(item)
                    .fetch_optional(&mut **hc.tx)
                    .await?;
                return Err(Error::field("items", format!("{} {lot} is on quality hold", code.unwrap_or_default())));
            }
        }
        Ok(())
    }
}

struct HoldHook;

#[async_trait]
impl DocHook for HoldHook {
    fn name(&self) -> &'static str {
        "quality.hold"
    }

    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if doc.get("placed_by").is_none_or(Value::is_null) {
            doc.insert("placed_by".into(), json!(hc.ctx.email));
        }
        Ok(())
    }

    fn fills(&self) -> &'static [&'static str] {
        &["placed_by"]
    }
}

/// An approved plan is evidence of what was checked, so it does not change.
struct PlanHook;

#[async_trait]
impl DocHook for PlanHook {
    async fn validate_change(&self, _hc: &mut HookCtx<'_>, old: Option<&Doc>, doc: &Doc) -> Result<()> {
        let Some(old) = old else { return Ok(()) };
        if old.get("status").and_then(Value::as_str) != Some("Draft") {
            for field in ["item", "revision", "template", "sample_size"] {
                if old.get(field) != doc.get(field) {
                    return Err(Error::field(field, "an approved plan does not change: make a new revision"));
                }
            }
        }
        Ok(())
    }
}

fn may(ctx: &Ctx, role: &str) -> Result<()> {
    if ctx.is_admin || ctx.has_role(role) {
        return Ok(());
    }
    Err(Error::Forbidden(format!("this takes the {role} role")))
}

async fn set(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, id: Uuid, v: Value) -> Result<()> {
    k.set_values_tx(&sys(ctx), tx, entity, id, v.as_object().cloned().unwrap_or_default()).await
}

/// Lifts a hold. The person who placed it cannot, whatever their role, and the note says why it is
/// safe. Stock may then be used, by the route the disposition decided.
pub async fn release(k: &Kernel, ctx: &Ctx, tx: &mut Tx, hold: Uuid, note: &str) -> Result<()> {
    may(ctx, "Quality Manager")?;
    if note.trim().is_empty() {
        return Err(Error::field("note", "say why this is safe"));
    }
    let doc = k.get_tx(&sys(ctx), tx, "QualityHold", hold, true).await?;
    if doc.get("status").and_then(Value::as_str) != Some("Active") {
        return Err(Error::invalid("this hold is not active"));
    }
    if s(&doc, "placed_by") == Some(ctx.email.as_str()) {
        return Err(Error::Forbidden("someone other than who placed it has to release it".into()));
    }
    set(
        k,
        ctx,
        tx,
        "QualityHold",
        hold,
        json!({"status": "Released", "released_by": ctx.email, "released_at": chrono::Utc::now().to_rfc3339(), "release_note": note}),
    )
    .await?;
    ferrum_kernel::outbox::enqueue(tx, ctx.tenant, "quality.hold.released", Some("QualityHold"), Some(hold), json!({"hold": hold})).await?;
    audit::record(tx, ctx, "QualityHold", hold, "quality.released", None, json!({"note": note})).await
}

#[derive(Deserialize)]
struct Note {
    #[serde(default)]
    note: String,
}

#[derive(Deserialize)]
struct Disposition {
    disposition: String,
    #[serde(default)]
    note: String,
}

async fn decide(k: &Kernel, ctx: &Ctx, tx: &mut Tx, ncr: Uuid, d: &Disposition) -> Result<()> {
    may(ctx, "Quality Manager")?;
    if !matches!(d.disposition.as_str(), "Use As Is" | "Rework" | "Scrap" | "Return to Supplier") {
        return Err(Error::field("disposition", "not a disposition"));
    }
    if d.note.trim().is_empty() {
        return Err(Error::field("note", "say why"));
    }
    let doc = k.get_tx(&sys(ctx), tx, "Nonconformance", ncr, true).await?;
    if matches!(doc.get("status").and_then(Value::as_str), Some("Dispositioned" | "Closed")) {
        return Err(Error::invalid("this has been decided"));
    }
    set(
        k,
        ctx,
        tx,
        "Nonconformance",
        ncr,
        json!({"status": "Dispositioned", "disposition": d.disposition, "disposition_note": d.note, "decided_by": ctx.email}),
    )
    .await
}

async fn verify_capa(k: &Kernel, ctx: &Ctx, tx: &mut Tx, capa: Uuid, note: &str) -> Result<()> {
    may(ctx, "Quality Manager")?;
    let doc = k.get_tx(&sys(ctx), tx, "Capa", capa, true).await?;
    if note.trim().is_empty() {
        return Err(Error::field("note", "say how you know it worked"));
    }
    if s(&doc, "owner_email") == Some(ctx.email.as_str()) {
        return Err(Error::Forbidden("someone other than its owner has to verify this".into()));
    }
    if rows(&doc, "actions").iter().any(|a| a.get("done_on").is_none_or(Value::is_null)) {
        return Err(Error::invalid("its actions are not all done"));
    }
    set(k, ctx, tx, "Capa", capa, json!({"status": "Verified", "effectiveness_note": note, "verified_by": ctx.email})).await
}

async fn approve_plan(k: &Kernel, ctx: &Ctx, tx: &mut Tx, plan: Uuid) -> Result<()> {
    may(ctx, "Quality Manager")?;
    let doc = k.get_tx(&sys(ctx), tx, "InspectionPlan", plan, true).await?;
    if doc.get("status").and_then(Value::as_str) != Some("Draft") {
        return Err(Error::invalid("this plan is not a draft"));
    }
    let item = id(&doc, "item").ok_or_else(|| Error::internal("a plan without an item"))?;
    sqlx::query("update e_inspection_plan set status = 'Superseded' where tenant_id = $1 and item = $2 and status = 'Approved'")
        .bind(ctx.tenant)
        .bind(item)
        .execute(&mut **tx)
        .await?;
    set(k, ctx, tx, "InspectionPlan", plan, json!({"status": "Approved", "approved_by": ctx.email})).await
}

/// Endpoints under `/api/app/quality/`: `POST /holds/{id}/release`, `/ncr/{id}/disposition`,
/// `/capa/{id}/verify` and `/plans/{id}/approve`.
struct QualitySite;

#[async_trait]
impl Site for QualitySite {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        let Some(ctx) = req.user.clone() else { return Err(Error::Unauthorized("sign in required".into())) };
        let path = req.path.trim_matches('/').to_string();
        let segments: Vec<&str> = path.split('/').collect();
        let body = |req: &SiteRequest| serde_json::from_slice::<Value>(&req.body).unwrap_or(Value::Null);
        let note: String = serde_json::from_value::<Note>(body(&req)).map(|n| n.note).unwrap_or_default();
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let uuid = |s: &str| s.parse::<Uuid>().map_err(|_| Error::NotFound("record".into()));
        match (req.method.as_str(), segments.as_slice()) {
            ("POST", ["holds", id, "release"]) => release(k, &ctx, &mut tx, uuid(id)?, &note).await?,
            ("POST", ["ncr", id, "disposition"]) => {
                let d: Disposition = serde_json::from_slice(&req.body).map_err(|e| Error::invalid(e.to_string()))?;
                decide(k, &ctx, &mut tx, uuid(id)?, &d).await?;
            }
            ("POST", ["capa", id, "verify"]) => verify_capa(k, &ctx, &mut tx, uuid(id)?, &note).await?,
            ("POST", ["plans", id, "approve"]) => approve_plan(k, &ctx, &mut tx, uuid(id)?).await?,
            _ => return Err(Error::NotFound("route".into())),
        }
        tx.commit().await?;
        Ok(SiteResponse::json(200, &json!({"done": true})))
    }
}
