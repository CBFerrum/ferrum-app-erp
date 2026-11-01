//! Endpoints under `/api/app/wms/`, for scanners and the interface. Each call changes one thing in
//! one transaction, so a scanner can repeat itself after a lost answer.
//!
//! | Method & path | Who | What |
//! |---|---|---|
//! | `GET /overview?warehouse=` | any role here | what needs doing, and what needs a person |
//! | `GET /bins/{code}?warehouse=` | | what is in a bin |
//! | `GET /tasks?warehouse=&status=` | | open tasks, most urgent first |
//! | `POST /tasks/{id}/release` | Manager | `{robot?}` |
//! | `POST /tasks/{id}/confirm` | Operator | a [`Confirm`] |
//! | `POST /tasks/{id}/cancel`, `/recover` | Manager | |
//! | `POST /inbound/{id}/scan`, `/unknown`, `/close` | Operator, Manager to close | |
//! | `POST /shipments/{id}/verify`, `/hand-over` | Operator | |
//! | `POST /counts/{id}/start`, `/enter`, `/finish`, `/approve` | Operator; Manager to approve | |
//! | `POST /returns/{id}/receive`, `/dispose` | Operator; Manager to decide | |
//! | `POST /positions/status`, `/positions/move` | Manager, Operator | a reason is required |
//! | `GET /reconcile?warehouse=`, `GET /trace?item=&lot=` | Manager | |
//! | `POST /billing` | Manager | `{customer, from, to}` |

use crate::tasks::{self, Confirm, Executor};
use crate::{billing, counts, inbound, position, reconcile, returns, shipping, slotting};
use async_trait::async_trait;
use ferrum_kernel::{Ctx, Error, Kernel, Result, Site, SiteRequest, SiteResponse, Tx};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

pub struct WmsSite;

fn body<T: DeserializeOwned>(req: &SiteRequest) -> Result<T> {
    serde_json::from_slice(&req.body).map_err(|e| Error::invalid(format!("the request body: {e}")))
}

fn uuid(seg: &str) -> Result<Uuid> {
    seg.parse().map_err(|_| Error::NotFound("record".into()))
}

fn param(req: &SiteRequest, k: &str) -> Result<String> {
    req.query.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string).ok_or_else(|| Error::field(k, "is required"))
}

const READ: &[&str] = &["Warehouse Viewer", "Warehouse Operator", "Warehouse Manager"];
const WORK: &[&str] = &["Warehouse Operator", "Warehouse Manager"];
const MANAGE: &[&str] = &["Warehouse Manager"];

fn may(ctx: &Ctx, roles: &[&str]) -> Result<()> {
    if ctx.is_admin || roles.iter().any(|r| ctx.has_role(r)) {
        return Ok(());
    }
    Err(Error::Forbidden(format!("this takes one of: {}", roles.join(", "))))
}

#[derive(Deserialize)]
struct Release {
    #[serde(default)]
    robot: Option<Uuid>,
}

#[derive(Deserialize)]
struct Reason {
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct Recover {
    found_bin: String,
    qty: Decimal,
    note: String,
}

#[derive(Deserialize)]
struct Bill {
    customer: Uuid,
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
}

async fn overview(tx: &mut Tx, tenant: Uuid, warehouse: Uuid) -> Result<Value> {
    let tasks: Vec<(Option<String>, i64)> = sqlx::query_as("select status, count(*) from e_warehouse_task where tenant_id = $1 and warehouse = $2 and status not in ('Done', 'Short', 'Cancelled') group by status").bind(tenant).bind(warehouse).fetch_all(&mut **tx).await?;
    let positions: Vec<(String, Decimal)> = sqlx::query_as("select status, sum(qty) from wms_position where tenant_id = $1 and warehouse_id = $2 and qty > 0 group by status").bind(tenant).bind(warehouse).fetch_all(&mut **tx).await?;
    let blocked: i64 = sqlx::query_scalar("select count(*) from e_storage_bin where tenant_id = $1 and warehouse = $2 and status = 'Blocked'").bind(tenant).bind(warehouse).fetch_one(&mut **tx).await?;
    let closed: i64 = sqlx::query_scalar("select count(*) from e_storage_zone where tenant_id = $1 and warehouse = $2 and not available").bind(tenant).bind(warehouse).fetch_one(&mut **tx).await?;
    Ok(json!({
        "tasks": tasks.into_iter().map(|(s, n)| (s.unwrap_or_default(), n)).collect::<std::collections::BTreeMap<_, _>>(),
        "stock": positions.into_iter().collect::<std::collections::BTreeMap<_, _>>(),
        "blocked_bins": blocked,
        "closed_zones": closed,
    }))
}

async fn route(k: &Kernel, ctx: &Ctx, tx: &mut Tx, req: &SiteRequest, segments: &[&str]) -> Result<Value> {
    match (req.method.as_str(), segments) {
        ("GET", ["overview"]) => {
            may(ctx, READ)?;
            overview(tx, ctx.tenant, uuid(&param(req, "warehouse")?)?).await
        }
        ("GET", ["bins", code]) => {
            may(ctx, READ)?;
            let warehouse = uuid(&param(req, "warehouse")?)?;
            let bin = slotting::bin_by_code(tx, ctx.tenant, warehouse, code).await?;
            let held = position::in_bin(tx, ctx.tenant, bin.id).await?;
            Ok(json!({"bin": bin.bin_code, "id": bin.id, "status": bin.status, "frozen": bin.frozen_by.is_some(), "positions": held}))
        }
        ("GET", ["tasks"]) => {
            may(ctx, READ)?;
            let warehouse = uuid(&param(req, "warehouse")?)?;
            let status = param(req, "status").ok();
            let rows: Vec<Value> = sqlx::query_scalar(
                "select jsonb_build_object('id', t.id, 'summary', t.summary, 'kind', t.kind, 'status', t.status, 'custody', t.custody, 'qty', t.qty, 'reason', t.reason, 'item', i.item_code, \
                   'lot', t.lot, 'from_bin', f.bin_code, 'to_bin', d.bin_code, 'robot', t.robot) \
                 from e_warehouse_task t join e_item i on i.tenant_id = t.tenant_id and i.id = t.item \
                 left join e_storage_bin f on f.tenant_id = t.tenant_id and f.id = t.from_bin left join e_storage_bin d on d.tenant_id = t.tenant_id and d.id = t.to_bin \
                 where t.tenant_id = $1 and t.warehouse = $2 and (($3::text is null and t.status not in ('Done', 'Short', 'Cancelled')) or t.status = $3) \
                 order by t.priority desc, f.pick_sequence, t.id limit 500",
            )
            .bind(ctx.tenant)
            .bind(warehouse)
            .bind(status)
            .fetch_all(&mut **tx)
            .await?;
            Ok(json!({"tasks": rows}))
        }
        ("POST", ["tasks", id, "release"]) => {
            may(ctx, MANAGE)?;
            let r: Release = body(req)?;
            tasks::release(k, ctx, tx, uuid(id)?, r.robot.map_or(Executor::Person, Executor::Robot)).await?;
            Ok(json!({"released": true}))
        }
        ("POST", ["tasks", id, "confirm"]) => {
            may(ctx, WORK)?;
            let c: Confirm = body(req)?;
            tasks::confirm(k, ctx, tx, uuid(id)?, &c).await?;
            Ok(json!({"done": true}))
        }
        ("POST", ["tasks", id, "cancel"]) => {
            may(ctx, MANAGE)?;
            let Reason { reason } = body(req)?;
            Ok(json!({"status": tasks::cancel(k, ctx, tx, uuid(id)?, &reason).await?}))
        }
        ("POST", ["tasks", id, "recover"]) => {
            may(ctx, MANAGE)?;
            let r: Recover = body(req)?;
            tasks::recover(k, ctx, tx, uuid(id)?, &r.found_bin, r.qty, &r.note).await?;
            Ok(json!({"recovered": true}))
        }
        ("POST", ["inbound", id, "scan"]) => {
            may(ctx, WORK)?;
            inbound::scan(k, ctx, tx, uuid(id)?, &body(req)?).await
        }
        ("POST", ["inbound", id, "unknown"]) => {
            may(ctx, WORK)?;
            inbound::unknown(k, ctx, tx, uuid(id)?, &body(req)?).await?;
            Ok(json!({"noted": true}))
        }
        ("POST", ["inbound", id, "close"]) => {
            may(ctx, MANAGE)?;
            inbound::close(k, ctx, tx, uuid(id)?).await
        }
        ("POST", ["shipments", id, "verify"]) => {
            may(ctx, WORK)?;
            shipping::verify(k, ctx, tx, uuid(id)?, &body(req)?).await
        }
        ("POST", ["shipments", id, "hand-over"]) => {
            may(ctx, WORK)?;
            shipping::hand_over(k, ctx, tx, uuid(id)?, &body(req)?).await
        }
        ("POST", ["counts", id, "start"]) => {
            may(ctx, WORK)?;
            counts::start(k, ctx, tx, uuid(id)?).await
        }
        ("POST", ["counts", id, "enter"]) => {
            may(ctx, WORK)?;
            counts::enter(k, ctx, tx, uuid(id)?, &body(req)?).await
        }
        ("POST", ["counts", id, "finish"]) => {
            may(ctx, WORK)?;
            counts::finish(k, ctx, tx, uuid(id)?).await
        }
        ("POST", ["counts", id, "approve"]) => {
            may(ctx, MANAGE)?;
            counts::approve(k, ctx, tx, uuid(id)?).await
        }
        ("POST", ["returns", id, "receive"]) => {
            may(ctx, WORK)?;
            returns::receive(k, ctx, tx, uuid(id)?).await
        }
        ("POST", ["returns", id, "dispose"]) => {
            may(ctx, MANAGE)?;
            returns::dispose(k, ctx, tx, uuid(id)?, &body(req)?).await
        }
        ("POST", ["positions", "status"]) => {
            may(ctx, MANAGE)?;
            returns::change_status(tx, ctx, &body(req)?).await?;
            Ok(json!({"changed": true}))
        }
        ("POST", ["positions", "move"]) => {
            may(ctx, WORK)?;
            returns::manual_move(tx, ctx, &body(req)?).await?;
            Ok(json!({"moved": true}))
        }
        ("GET", ["reconcile"]) => {
            may(ctx, MANAGE)?;
            reconcile::differences(tx, ctx.tenant, uuid(&param(req, "warehouse")?)?).await
        }
        ("GET", ["trace"]) => {
            may(ctx, READ)?;
            Ok(json!({"moves": reconcile::trace(tx, ctx.tenant, uuid(&param(req, "item")?)?, &param(req, "lot")?).await?}))
        }
        ("POST", ["billing"]) => {
            may(ctx, MANAGE)?;
            let b: Bill = body(req)?;
            billing::bill(k, ctx, tx, b.customer, b.from, b.to).await
        }
        _ => Err(Error::NotFound("route".into())),
    }
}

#[async_trait]
impl Site for WmsSite {
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
