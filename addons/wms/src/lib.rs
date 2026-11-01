//! Warehouse Management: warehouse execution on ERP's stock (Ferrum docs/plan/industrial.md).
//!
//! * [`position`]: what is where, and the append-only ledger that explains it;
//! * [`posting`]: the contract with ERP, so that positions and the books cannot drift;
//! * [`slotting`]: where stock may go;
//! * [`tasks`]: one movement, by a person or a robot;
//! * [`waves`]: allocation and replenishment;
//! * [`inbound`], [`shipping`], [`counts`], [`returns`]: the flows around them;
//! * [`billing`]: what customers pay for storage;
//! * [`site`]: the endpoints people and scanners use.
//!
//! Automation coordinates here, and controls nothing: a robot's route, traffic and safety are its
//! fleet manager's, and a conveyor's sequence is its controller's.

pub mod billing;
pub mod counts;
pub mod holds;
pub mod inbound;
pub mod position;
pub mod posting;
pub mod reconcile;
pub mod returns;
pub mod shipping;
pub mod site;
pub mod slotting;
pub mod tasks;
pub mod waves;

use async_trait::async_trait;
use ferrum_domain::reports::ReportSource;
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, DocHook, Error, EventSink, HookCtx, HookRegistry, Kernel, Result};
use ferrum_ops::contract::State;
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// This app's manifest (`app.yaml`).
pub const MANIFEST: &str = include_str!("../app.yaml");

/// This app's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers this app's document hooks. They run after ERP's, which post the stock they mirror.
pub fn register(h: &mut HookRegistry) {
    for entity in posting::STOCK_DOCUMENTS {
        h.register(entity, Arc::new(posting::MirrorHook(entity)));
    }
    h.register("ManagedWarehouse", Arc::new(ManagedHook)).register("PickWave", Arc::new(waves::WaveHook)).register("QualityHold", Arc::new(holds::HoldHook));
    h.add_event_sink(Arc::new(CommandEvents));
    h.add_site("wms", Arc::new(site::WmsSite));
}

/// This app's reports.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![]
}

struct ManagedHook;

#[async_trait]
impl DocHook for ManagedHook {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &ferrum_kernel::Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "start_managing" {
            return Ok(None);
        }
        let record = ferrum_domain::util::id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let warehouse = ferrum_domain::util::id(doc, "warehouse").ok_or_else(|| Error::field("warehouse", "is required"))?;
        let inbound = ferrum_domain::util::id(doc, "inbound_bin").ok_or_else(|| Error::field("inbound_bin", "is required"))?;
        for field in ["outbound_bin", "quarantine_bin"] {
            if ferrum_domain::util::id(doc, field).is_none() {
                return Err(Error::field(field, "is required"));
            }
        }
        let placed = posting::start_managing(hc.tx, hc.ctx, record, warehouse, inbound).await?;
        Ok(Some(serde_json::json!({"placed": placed})))
    }
}

/// Follows the ledger: a mission's progress moves its task. At least once and in any order, which
/// `tasks::on_command` is written for.
struct CommandEvents;

#[async_trait]
impl EventSink for CommandEvents {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic == "quality.hold.released" {
            return holds::released(k, ev).await;
        }
        if !ev.topic.starts_with("ops.command.") {
            return Ok(());
        }
        let Some(task) = ev.payload.get("workflow_id").and_then(Value::as_str).and_then(tasks::task_of) else { return Ok(()) };
        let (Some(state), Some(command)) =
            (ev.payload.get("state").and_then(Value::as_str).and_then(State::parse), ev.payload.get("command").and_then(Value::as_str).and_then(|c| c.parse::<Uuid>().ok()))
        else {
            return Ok(());
        };
        let schema = k.registry.schema(ev.tenant_id).await?;
        let ctx = Ctx::system(ev.tenant_id, schema);
        let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
        tasks::on_command(k, &ctx, &mut tx, task, state, command, ev.payload.get("outcome").unwrap_or(&Value::Null)).await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Periodic upkeep: replenishment that fell due, and a daily count of customers' bins for billing.
pub async fn maintenance(k: &Kernel) -> Result<()> {
    let tenants: Vec<Uuid> = {
        let mut tx = k.db.system_tx().await?;
        let t = sqlx::query_scalar("select distinct tenant_id from e_managed_warehouse where status = 'Managed'").fetch_all(&mut *tx).await?;
        tx.commit().await?;
        t
    };
    for tenant in tenants {
        let ctx = Ctx::system(tenant, k.registry.schema(tenant).await?);
        let mut tx = k.db.tenant_tx(tenant).await?;
        waves::replenish(k, &ctx, &mut tx).await?;
        billing::snapshot(&mut tx, tenant, ctx.today()).await?;
        tx.commit().await?;
    }
    Ok(())
}
