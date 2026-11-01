//! Manufacturing Execution on ERP's work orders (Ferrum docs/plan/industrial.md).
//!
//! * [`runs`]: the release gate, execution events and what posts to ERP;
//! * [`trace`]: lot genealogy, both ways;
//! * [`oee`]: availability, performance and quality from recorded facts.
//!
//! It dispatches a reference to an approved job. It never writes controller logic.

pub mod oee;
pub mod runs;
pub mod trace;

use async_trait::async_trait;
use ferrum_domain::reports::ReportSource;
use ferrum_domain::util::s;
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, EventSink, HookCtx, HookRegistry, Kernel, Result, Site, SiteRequest, SiteResponse, Tx};
use ferrum_ops::contract::State;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));
include!(concat!(env!("OUT_DIR"), "/ui.rs"));

/// This app's manifest (`app.yaml`).
pub const MANIFEST: &str = include_str!("../app.yaml");

/// This app's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers this app's document hooks and endpoints.
pub fn register(h: &mut HookRegistry) {
    h.register("ProductionRevision", Arc::new(RevisionHook));
    h.add_event_sink(Arc::new(Events));
    h.add_site("mes", Arc::new(MesSite));
}

pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![]
}

pub async fn maintenance(_k: &Kernel) -> Result<()> {
    Ok(())
}

/// A revision that has been approved is the record of how things were made, so it does not change.
struct RevisionHook;

#[async_trait]
impl DocHook for RevisionHook {
    async fn validate_change(&self, _hc: &mut HookCtx<'_>, old: Option<&Doc>, doc: &Doc) -> Result<()> {
        let Some(old) = old else { return Ok(()) };
        if s(old, "status") != Some("Draft") {
            for field in ["item", "revision", "bom", "job_number", "ideal_cycle_secs", "instructions", "parameters"] {
                if old.get(field).filter(|v| !v.is_null()) != doc.get(field).filter(|v| !v.is_null()) {
                    return Err(Error::field(field, "an approved revision does not change: make a new one"));
                }
            }
        }
        Ok(())
    }

    async fn before_delete(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if s(doc, "status") != Some("Draft") {
            return Err(Error::invalid("an approved revision stays: it says how things were made"));
        }
        Ok(())
    }
}

struct Events;

#[async_trait]
impl EventSink for Events {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        let schema = || k.registry.schema(ev.tenant_id);
        if ev.topic == "ops.fact.mes.event" {
            // Recorded at a terminal with the link down, and arriving now. A console knows a run by
            // the reference on its job sheet.
            let Ok(step) = serde_json::from_value::<runs::Event>(ev.payload["body"].clone()) else { return Ok(()) };
            let mut ctx = Ctx::system(ev.tenant_id, schema().await?);
            ctx.email = ev.payload["actor"].as_str().unwrap_or("site").to_string();
            let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
            let run = match ev.payload["body"]["run"].as_str().and_then(|r| r.parse::<Uuid>().ok()) {
                Some(run) => Some(run),
                None => {
                    sqlx::query_scalar("select id from e_production_run where tenant_id = $1 and number = $2")
                        .bind(ev.tenant_id)
                        .bind(ev.payload["body"]["reference"].as_str())
                        .fetch_optional(&mut *tx)
                        .await?
                }
            };
            let Some(run) = run else { return Ok(()) };
            match runs::record(k, &ctx, &mut tx, run, &step).await {
                Ok(_) => tx.commit().await?,
                // What the record refuses now stays in the fact, for a person: a retry cannot help.
                Err(Error::Invalid(_)) => {}
                Err(e) => return Err(e),
            }
            return Ok(());
        }
        if ev.topic == "wms.supply.short" {
            let Some(run) = ev.payload["run"].as_str().and_then(|r| r.parse::<Uuid>().ok()) else { return Ok(()) };
            let ctx = Ctx::system(ev.tenant_id, schema().await?);
            let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
            let mut note = Doc::new();
            note.insert(
                "note".into(),
                json!(format!("The warehouse cannot bring all of this to the line: {}", ev.payload["short"].as_str().unwrap_or_default())),
            );
            k.set_values_tx(&ctx, &mut tx, "ProductionRun", run, note).await?;
            tx.commit().await?;
            return Ok(());
        }
        if !ev.topic.starts_with("ops.command.") {
            return Ok(());
        }
        let Some(run) = ev.payload.get("workflow_id").and_then(Value::as_str).and_then(runs::run_of) else { return Ok(()) };
        let Some(state) = ev.payload.get("state").and_then(Value::as_str).and_then(State::parse) else { return Ok(()) };
        let ctx = Ctx::system(ev.tenant_id, schema().await?);
        let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
        runs::on_command(k, &ctx, &mut tx, run, state).await?;
        tx.commit().await?;
        Ok(())
    }
}

fn may(ctx: &Ctx, roles: &[&str]) -> Result<()> {
    if ctx.is_admin || roles.iter().any(|r| ctx.has_role(r)) {
        return Ok(());
    }
    Err(Error::Forbidden(format!("this takes one of: {}", roles.join(", "))))
}

const READ: &[&str] = &["Production Operator", "Production Supervisor"];
const SUPERVISE: &[&str] = &["Production Supervisor"];

fn param(req: &SiteRequest, k: &str) -> Result<String> {
    req.query.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string).ok_or_else(|| Error::field(k, "is required"))
}

async fn route(k: &Kernel, ctx: &Ctx, tx: &mut Tx, req: &SiteRequest, segments: &[&str]) -> Result<Value> {
    let uuid = |s: &str| s.parse::<Uuid>().map_err(|_| Error::NotFound("record".into()));
    match (req.method.as_str(), segments) {
        ("POST", ["runs", run, "release"]) => {
            may(ctx, SUPERVISE)?;
            let event =
                serde_json::from_slice::<Value>(&req.body).ok().and_then(|v| v["event_id"].as_str().and_then(|e| e.parse().ok())).unwrap_or_else(Uuid::now_v7);
            runs::release(k, ctx, tx, uuid(run)?, event).await
        }
        ("POST", ["runs", run, "events"]) => {
            may(ctx, READ)?;
            let e: runs::Event = serde_json::from_slice(&req.body).map_err(|e| Error::invalid(format!("the request body: {e}")))?;
            runs::record(k, ctx, tx, uuid(run)?, &e).await
        }
        ("POST", ["revisions", revision, "approve"]) => {
            may(ctx, SUPERVISE)?;
            runs::approve(k, ctx, tx, uuid(revision)?).await?;
            Ok(json!({"approved": true}))
        }
        ("GET", ["trace", direction @ ("forward" | "back")]) => {
            may(ctx, READ)?;
            let (item, lot) = (uuid(&param(req, "item")?)?, param(req, "lot")?);
            Ok(json!({"steps": trace::follow(tx, ctx.tenant, item, &lot, *direction == "forward").await?}))
        }
        ("GET", ["oee"]) => {
            may(ctx, READ)?;
            let at = |key: &str| param(req, key)?.parse().map_err(|_| Error::field(key, "must be an RFC 3339 time"));
            oee::for_workstation(tx, ctx.tenant, uuid(&param(req, "workstation")?)?, at("from")?, at("to")?).await
        }
        ("GET", ["runs", run, "events"]) => {
            may(ctx, READ)?;
            let rows: Vec<Value> = sqlx::query_scalar("select to_jsonb(e) - 'tenant_id' from mes_event e where tenant_id = $1 and run_id = $2 order by seq")
                .bind(ctx.tenant)
                .bind(uuid(run)?)
                .fetch_all(&mut **tx)
                .await?;
            Ok(json!({"events": rows}))
        }
        _ => Err(Error::NotFound("route".into())),
    }
}

/// Endpoints under `/api/app/mes/`.
struct MesSite;

#[async_trait]
impl Site for MesSite {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        let Some(ctx) = req.user.clone() else { return Err(Error::Unauthorized("sign in required".into())) };
        let path = req.path.trim_matches('/').to_string();
        let segments: Vec<&str> = path.split('/').collect();
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let answer = route(k, &ctx, &mut tx, &req, &segments).await?;
        tx.commit().await?;
        Ok(SiteResponse::json(200, &answer))
    }
}
