//! A production run. Release is a gate: wrong revision, missing material, equipment out of service
//! or an unqualified operator each stop it, saying which. After that, what happened is events, and
//! only reported good quantities post to ERP, through the work order's own manufacture.

use chrono::{DateTime, Utc};
use ferrum_domain::util::{d, elevated, id, money, rows, s, sys};
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Tx, audit};
use ferrum_ops::commands::{self, Issuer, NewCommand};
use ferrum_ops::contract::{Lifecycle, State};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

const WORKFLOW: &str = "mes-run:";
const JOB_OPERATION: &str = "machine.start_job";

pub fn run_of(workflow: &str) -> Option<Uuid> {
    workflow.strip_prefix(WORKFLOW)?.parse().ok()
}

async fn set(k: &Kernel, ctx: &Ctx, tx: &mut Tx, id: Uuid, v: Value) -> Result<()> {
    k.set_values_tx(&sys(ctx), tx, "ProductionRun", id, v.as_object().cloned().unwrap_or_default()).await
}

fn status(doc: &Doc) -> &str {
    doc.get("status").and_then(Value::as_str).unwrap_or("Planned")
}

/// Who a run's device is, and whether it can work. No device means a manual station.
async fn equipment(tx: &mut Tx, tenant: Uuid, workstation: Uuid) -> Result<Option<(Uuid, Lifecycle, String)>> {
    let row: Option<(Option<Uuid>,)> = sqlx::query_as("select mes_device from e_workstation where tenant_id = $1 and id = $2").bind(tenant).bind(workstation).fetch_optional(&mut **tx).await?;
    let Some(device) = row.and_then(|r| r.0) else { return Ok(None) };
    let (lifecycle, code): (String, String) = sqlx::query_as("select lifecycle, device_code from e_device where tenant_id = $1 and id = $2").bind(tenant).bind(device).fetch_one(&mut **tx).await?;
    Ok(Some((device, Lifecycle::parse(&lifecycle).unwrap_or(Lifecycle::Quarantined), code)))
}

/// What is usable of `item` in `warehouse`: the books, less what is on quality hold.
async fn usable(tx: &mut Tx, tenant: Uuid, item: Uuid, warehouse: Uuid) -> Result<Decimal> {
    let held_all: i64 = sqlx::query_scalar("select count(*) from e_quality_hold where tenant_id = $1 and item = $2 and status = 'Active' and coalesce(lot, '') = ''").bind(tenant).bind(item).fetch_one(&mut **tx).await?;
    if held_all > 0 {
        return Ok(Decimal::ZERO);
    }
    let have: Option<Decimal> = sqlx::query_scalar("select qty from stock_balance where tenant_id = $1 and item_id = $2 and warehouse_id = $3").bind(tenant).bind(item).bind(warehouse).fetch_optional(&mut **tx).await?;
    let held: Decimal = sqlx::query_scalar(
        "select coalesce(sum(m.qty), 0) from stock_move m where m.tenant_id = $1 and m.item_id = $2 and m.warehouse_id = $3 \
         and m.batch_no in (select h.lot from e_quality_hold h where h.tenant_id = $1 and h.item = $2 and h.status = 'Active')",
    )
    .bind(tenant)
    .bind(item)
    .bind(warehouse)
    .fetch_one(&mut **tx)
    .await?;
    Ok(have.unwrap_or_default() - held)
}

/// The gate. Everything it refuses, it refuses by name, and nothing partly releases.
pub async fn release(k: &Kernel, ctx: &Ctx, tx: &mut Tx, run: Uuid, event: Uuid) -> Result<Value> {
    let sctx = sys(ctx);
    let doc = k.get_tx(&sctx, tx, "ProductionRun", run, true).await?;
    if status(&doc) != "Planned" {
        return Err(Error::invalid(format!("this run is {}", status(&doc))));
    }
    let wo_id = id(&doc, "work_order").ok_or_else(|| Error::field("work_order", "is required"))?;
    let wo = k.get_tx(&sctx, tx, "WorkOrder", wo_id, false).await?;
    if d(&wo, "docstatus") != Decimal::ONE || s(&wo, "status") == Some("Stopped") {
        return Err(Error::invalid("the work order is not open"));
    }
    let rev_id = id(&doc, "revision").ok_or_else(|| Error::field("revision", "is required"))?;
    let rev = k.get_tx(&sctx, tx, "ProductionRevision", rev_id, false).await?;
    if s(&rev, "status") != Some("Approved") {
        return Err(Error::invalid(format!("revision {} is not approved", s(&rev, "revision").unwrap_or_default())));
    }
    if id(&rev, "item") != id(&wo, "item") || id(&rev, "bom") != id(&wo, "bom") {
        return Err(Error::invalid("that revision is not how this work order is being made"));
    }
    let today = ctx.today();
    let qualified: Option<i32> = sqlx::query_scalar("select 1 from e_operator_qualification where tenant_id = $1 and lower(user_email) = lower($2) and operation = $3 and valid_until >= $4")
        .bind(ctx.tenant)
        .bind(s(&doc, "operator_email"))
        .bind(id(&doc, "operation"))
        .bind(today)
        .fetch_optional(&mut **tx)
        .await?;
    if qualified.is_none() {
        return Err(Error::invalid(format!("{} is not qualified for this operation", s(&doc, "operator_email").unwrap_or_default())));
    }
    let workstation = id(&doc, "workstation").ok_or_else(|| Error::field("workstation", "is required"))?;
    let device = equipment(tx, ctx.tenant, workstation).await?;
    if let Some((_, lifecycle, code)) = &device
        && !lifecycle.takes_commands()
    {
        return Err(Error::invalid(format!("{code} is {}: it cannot run", lifecycle.as_str())));
    }
    let bom = k.get_tx(&sctx, tx, "Bom", id(&wo, "bom").ok_or_else(|| Error::internal("no bom"))?, false).await?;
    let per = if d(&bom, "quantity").is_zero() { Decimal::ONE } else { d(&bom, "quantity") };
    let source = id(&wo, "source_warehouse").ok_or_else(|| Error::internal("no source warehouse"))?;
    let mut missing = Vec::new();
    let mut materials = Vec::new();
    for line in rows(&bom, "items") {
        let item = id(&line, "item").ok_or_else(|| Error::internal("a line without an item"))?;
        let need = (d(&line, "qty") / per * d(&doc, "qty")).round_dp(9);
        let warehouse = id(&line, "source_warehouse").unwrap_or(source);
        materials.push(json!({"item": item, "warehouse": warehouse, "qty": need}));
        let there = usable(tx, ctx.tenant, item, warehouse).await?;
        if there < need {
            let code: Option<String> = sqlx::query_scalar("select item_code from e_item where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(item).fetch_optional(&mut **tx).await?;
            missing.push(format!("{}: {} of {}", code.unwrap_or_default(), there.normalize(), need.normalize()));
        }
    }
    if !missing.is_empty() {
        return Err(Error::invalid(format!("material is short: {}", missing.join("; "))));
    }
    if !append(tx, ctx, run, event, "Release", Utc::now(), &Line::default(), json!({"revision": rev_id})).await? {
        return Ok(json!({"repeated": true}));
    }
    // For whoever runs the warehouse: a managed one brings this to the line.
    ferrum_kernel::outbox::enqueue(tx, ctx.tenant, "mes.run.released", Some("ProductionRun"), Some(run), json!({"run": run, "materials": materials})).await?;
    let mut command = None;
    if let Some((device, ..)) = device {
        let takes_jobs = ferrum_ops::devices::operation(tx, ctx.tenant, device, JOB_OPERATION).await?.is_some();
        if let (true, Some(job)) = (takes_jobs, rev.get("job_number").and_then(Value::as_i64)) {
            let payload = json!({"job": job, "revision": s(&rev, "revision"), "qty": d(&doc, "qty")});
            let cmd = NewCommand {
                target: device,
                operation: JOB_OPERATION.into(),
                schema_version: 1,
                payload,
                expires_in_secs: 8 * 3600,
                dedupe_key: Some(format!("{WORKFLOW}{run}:start")),
                workflow_id: Some(format!("{WORKFLOW}{run}")),
                issuer: Issuer::Engine,
                ..Default::default()
            };
            command = Some(commands::issue(tx, ctx, cmd).await?.id);
        }
    }
    set(k, ctx, tx, run, json!({"status": "Released", "command": command.map(|c| c.to_string())})).await?;
    Ok(json!({"status": "Released", "command": command}))
}

#[derive(Debug, Default, Clone)]
struct Line {
    item: Option<Uuid>,
    lot: String,
    serial: String,
    qty: Option<Decimal>,
    scrap: Option<Decimal>,
    rework: Option<Decimal>,
}

/// Writes an event, once. Returns false when it already landed.
async fn append(tx: &mut Tx, ctx: &Ctx, run: Uuid, event: Uuid, kind: &str, at: DateTime<Utc>, l: &Line, detail: Value) -> Result<bool> {
    let done = sqlx::query(
        "insert into mes_event (tenant_id, run_id, event_id, kind, at, actor, item_id, lot, serial, qty, scrap, rework, detail) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) on conflict (tenant_id, event_id) do nothing",
    )
    .bind(ctx.tenant)
    .bind(run)
    .bind(event)
    .bind(kind)
    .bind(at)
    .bind(&ctx.email)
    .bind(l.item)
    .bind(&l.lot)
    .bind(&l.serial)
    .bind(l.qty)
    .bind(l.scrap)
    .bind(l.rework)
    .bind(detail)
    .execute(&mut **tx)
    .await?;
    Ok(done.rows_affected() > 0)
}

async fn equipment_state(tx: &mut Tx, tenant: Uuid, workstation: Uuid, at: DateTime<Utc>, state: &str, reason: Option<&str>, event: Uuid) -> Result<()> {
    if let Some((device, ..)) = equipment(tx, tenant, workstation).await? {
        sqlx::query("insert into op_equipment_event (tenant_id, device_id, at, state, reason, fact_id) values ($1, $2, $3, $4, $5, $6) on conflict do nothing")
            .bind(tenant)
            .bind(device)
            .bind(at)
            .bind(state)
            .bind(reason)
            .bind(event)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// What an operator did, as it arrives from a terminal or a site. `at` is when it happened, kept
/// apart from when it was heard.
#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Step {
    Start,
    Pause { reason: String },
    Resume,
    Consume { item: Uuid, #[serde(default)] lot: String, #[serde(default)] serial: String, qty: Decimal },
    Report { #[serde(default)] good: Decimal, #[serde(default)] scrap: Decimal, #[serde(default)] rework: Decimal, #[serde(default)] lot: String },
    Complete { #[serde(default)] note: String },
    Cancel { reason: String },
}

#[derive(Debug, Deserialize)]
pub struct Event {
    /// The same id lands once, however many times it is sent.
    pub event_id: Uuid,
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
    #[serde(flatten)]
    pub step: Step,
}

pub async fn record(k: &Kernel, ctx: &Ctx, tx: &mut Tx, run: Uuid, e: &Event) -> Result<Value> {
    let doc = k.get_tx(&sys(ctx), tx, "ProductionRun", run, true).await?;
    let at = e.at.unwrap_or_else(Utc::now);
    let state = status(&doc).to_string();
    let workstation = id(&doc, "workstation").ok_or_else(|| Error::internal("a run without a workstation"))?;
    let seen: Option<i32> = sqlx::query_scalar("select 1 from mes_event where tenant_id = $1 and event_id = $2").bind(ctx.tenant).bind(e.event_id).fetch_optional(&mut **tx).await?;
    if seen.is_some() {
        return Ok(json!({"repeated": true}));
    }
    let wrong = |what: &str| Error::invalid(format!("a run that is {state} cannot {what}"));
    match &e.step {
        Step::Start => {
            if state != "Released" {
                return Err(wrong("start"));
            }
            append(tx, ctx, run, e.event_id, "Start", at, &Line::default(), Value::Null).await?;
            set(k, ctx, tx, run, json!({"status": "Running", "started_at": at.to_rfc3339()})).await?;
            equipment_state(tx, ctx.tenant, workstation, at, "Running", None, e.event_id).await?;
        }
        Step::Pause { reason } => {
            if state != "Running" {
                return Err(wrong("pause"));
            }
            let known: Option<i32> = sqlx::query_scalar("select 1 from e_downtime_reason where tenant_id = $1 and reason = $2").bind(ctx.tenant).bind(reason).fetch_optional(&mut **tx).await?;
            if known.is_none() {
                return Err(Error::field("reason", "choose a downtime reason"));
            }
            append(tx, ctx, run, e.event_id, "Pause", at, &Line::default(), json!({"reason": reason})).await?;
            set(k, ctx, tx, run, json!({"status": "Paused"})).await?;
            equipment_state(tx, ctx.tenant, workstation, at, "Stopped", Some(reason), e.event_id).await?;
        }
        Step::Resume => {
            if state != "Paused" {
                return Err(wrong("resume"));
            }
            append(tx, ctx, run, e.event_id, "Resume", at, &Line::default(), Value::Null).await?;
            set(k, ctx, tx, run, json!({"status": "Running"})).await?;
            equipment_state(tx, ctx.tenant, workstation, at, "Running", None, e.event_id).await?;
        }
        Step::Consume { item, lot, serial, qty } => {
            if !matches!(state.as_str(), "Running" | "Paused") {
                return Err(wrong("consume"));
            }
            consume_check(k, ctx, tx, &doc, *item, lot, *qty).await?;
            let line = Line { item: Some(*item), lot: lot.clone(), serial: serial.clone(), qty: Some(*qty), ..Default::default() };
            append(tx, ctx, run, e.event_id, "Consume", at, &line, Value::Null).await?;
        }
        Step::Report { good, scrap, rework, lot } => {
            if !matches!(state.as_str(), "Running" | "Paused") {
                return Err(wrong("report"));
            }
            if *good < Decimal::ZERO || *scrap < Decimal::ZERO || *rework < Decimal::ZERO || (*good + *scrap + *rework).is_zero() {
                return Err(Error::field("good", "report a quantity"));
            }
            let (new_good, new_scrap) = (d(&doc, "good_qty") + *good, d(&doc, "scrap_qty") + *scrap);
            if new_good + new_scrap > d(&doc, "qty") {
                return Err(Error::invalid(format!("that is more than this run's {}", d(&doc, "qty").normalize())));
            }
            if *good > Decimal::ZERO && lot.is_empty() && output_tracked(k, ctx, tx, &doc).await? {
                return Err(Error::field("lot", "this item is lot-tracked: say which lot this is"));
            }
            let line = Line { lot: lot.clone(), qty: Some(*good), scrap: Some(*scrap), rework: Some(*rework), ..Default::default() };
            append(tx, ctx, run, e.event_id, "Report", at, &line, Value::Null).await?;
            if *good > Decimal::ZERO {
                post_output(k, ctx, tx, run, &doc, *good, lot).await?;
            }
            if *scrap > Decimal::ZERO {
                post_scrap(k, ctx, tx, run, &doc, *scrap).await?;
            }
            set(k, ctx, tx, run, json!({"good_qty": money(new_good), "scrap_qty": money(new_scrap), "rework_qty": money(d(&doc, "rework_qty") + *rework)})).await?;
        }
        Step::Complete { note } => {
            if !matches!(state.as_str(), "Running" | "Paused") {
                return Err(wrong("complete"));
            }
            if d(&doc, "good_qty") + d(&doc, "scrap_qty") < d(&doc, "qty") && note.trim().is_empty() {
                return Err(Error::field("note", "this run is short of its quantity: say why"));
            }
            append(tx, ctx, run, e.event_id, "Complete", at, &Line::default(), json!({"note": note})).await?;
            set(k, ctx, tx, run, json!({"status": "Completed", "finished_at": at.to_rfc3339(), "note": note})).await?;
            equipment_state(tx, ctx.tenant, workstation, at, "Idle", None, e.event_id).await?;
        }
        Step::Cancel { reason } => {
            if !matches!(state.as_str(), "Planned" | "Released") {
                return Err(wrong("be cancelled"));
            }
            append(tx, ctx, run, e.event_id, "Cancel", at, &Line::default(), json!({"reason": reason})).await?;
            if let Some(c) = s(&doc, "command").and_then(|c| c.parse().ok()) {
                commands::cancel(tx, ctx, c, reason).await?;
            }
            set(k, ctx, tx, run, json!({"status": "Cancelled", "note": reason})).await?;
        }
    }
    Ok(json!({"done": true}))
}

async fn output_tracked(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc) -> Result<bool> {
    let wo = k.get_tx(&sys(ctx), tx, "WorkOrder", id(doc, "work_order").ok_or_else(|| Error::internal("no order"))?, false).await?;
    let tracked: Option<Option<bool>> = sqlx::query_scalar("select has_batch_no from e_item where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(id(&wo, "item")).fetch_optional(&mut **tx).await?;
    Ok(tracked.flatten().unwrap_or(false))
}

/// A material may go in only if the approved BOM says so, and only if it is not held, unless the
/// hold names this work order as its rework route.
async fn consume_check(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc, item: Uuid, lot: &str, qty: Decimal) -> Result<()> {
    if qty <= Decimal::ZERO {
        return Err(Error::field("qty", "must be above zero"));
    }
    let sctx = sys(ctx);
    let wo_id = id(doc, "work_order").ok_or_else(|| Error::internal("no order"))?;
    let wo = k.get_tx(&sctx, tx, "WorkOrder", wo_id, false).await?;
    let bom = k.get_tx(&sctx, tx, "Bom", id(&wo, "bom").ok_or_else(|| Error::internal("no bom"))?, false).await?;
    if !rows(&bom, "items").iter().any(|l| id(l, "item") == Some(item)) {
        return Err(Error::invalid("that item is not in the approved bill of materials"));
    }
    if let Some((_, rework)) = ferrum_quality::hold_on(tx, ctx.tenant, item, lot).await?
        && rework != Some(wo_id.to_string())
    {
        return Err(Error::invalid(format!("lot {lot} is on quality hold")));
    }
    Ok(())
}

/// Consumption not yet tied to an output: per item and lot, oldest first.
async fn open_consumption(tx: &mut Tx, tenant: Uuid, run: Uuid) -> Result<BTreeMap<Uuid, Vec<(String, String, Decimal)>>> {
    let rows: Vec<(Uuid, String, String, Decimal)> = sqlx::query_as(
        "with used as (select input_item, input_lot, input_serial, sum(input_qty) as qty from mes_genealogy where tenant_id = $1 and run_id = $2 group by 1, 2, 3), \
              taken as (select item_id, lot, serial, sum(qty) as qty, min(seq) as first from mes_event where tenant_id = $1 and run_id = $2 and kind = 'Consume' group by 1, 2, 3) \
         select t.item_id, t.lot, t.serial, t.qty - coalesce(u.qty, 0) from taken t \
         left join used u on u.input_item = t.item_id and u.input_lot = t.lot and u.input_serial = t.serial where t.qty > coalesce(u.qty, 0) order by t.first",
    )
    .bind(tenant)
    .bind(run)
    .fetch_all(&mut **tx)
    .await?;
    let mut out: BTreeMap<Uuid, Vec<(String, String, Decimal)>> = BTreeMap::new();
    for (item, lot, serial, qty) in rows {
        out.entry(item).or_default().push((lot, serial, qty));
    }
    Ok(out)
}

/// Spreads `need` of an item over what was consumed. A lot-tracked item must be fully accounted for
/// by consumption events. One that is not tracked may simply be what the BOM says.
fn spread(open: &mut Vec<(String, String, Decimal)>, need: Decimal, tracked: bool, code: &str) -> Result<Vec<(String, String, Decimal)>> {
    let (mut left, mut out) = (need, Vec::new());
    for row in open.iter_mut() {
        if left <= Decimal::ZERO {
            break;
        }
        let part = left.min(row.2);
        if part > Decimal::ZERO {
            out.push((row.0.clone(), row.1.clone(), part));
            row.2 -= part;
            left -= part;
        }
    }
    if left > Decimal::ZERO {
        if tracked {
            return Err(Error::invalid(format!("{code}: {} more must be consumed by lot before this can be reported", left.normalize())));
        }
        out.push((String::new(), String::new(), left));
    }
    Ok(out)
}

async fn record_genealogy(tx: &mut Tx, tenant: Uuid, run: Uuid, item: Uuid, lot: &str, serial: &str, qty: Decimal, out_item: Uuid, out_lot: &str) -> Result<()> {
    sqlx::query("insert into mes_genealogy (tenant_id, run_id, input_item, input_lot, input_serial, input_qty, output_item, output_lot) values ($1, $2, $3, $4, $5, $6, $7, $8)")
        .bind(tenant)
        .bind(run)
        .bind(item)
        .bind(lot)
        .bind(serial)
        .bind(qty)
        .bind(out_item)
        .bind(out_lot)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn tracked(tx: &mut Tx, tenant: Uuid, item: Uuid) -> Result<(bool, String)> {
    let row: (Option<bool>, String) = sqlx::query_as("select has_batch_no, item_code from e_item where tenant_id = $1 and id = $2").bind(tenant).bind(item).fetch_one(&mut **tx).await?;
    Ok((row.0.unwrap_or(false), row.1))
}

/// Good output posts through the work order's own manufacture, with the lots that really went in.
async fn post_output(k: &Kernel, ctx: &Ctx, tx: &mut Tx, run: Uuid, doc: &Doc, good: Decimal, out_lot: &str) -> Result<()> {
    let elevated = elevated(ctx);
    let wo_id = id(doc, "work_order").ok_or_else(|| Error::internal("no order"))?;
    let made = k.run_action_tx(&elevated, tx, "WorkOrder", wo_id, "manufacture", &json!({"qty": good, "submit": false})).await?;
    let entry: Uuid = made["id"].as_str().and_then(|v| v.parse().ok()).ok_or_else(|| Error::internal("no stock entry"))?;
    let draft = k.get_tx(&elevated, tx, "StockEntry", entry, true).await?;
    let wo = k.get_tx(&elevated, tx, "WorkOrder", wo_id, false).await?;
    let output = id(&wo, "item").ok_or_else(|| Error::internal("no item"))?;
    let mut open = open_consumption(tx, ctx.tenant, run).await?;
    let mut items = Vec::new();
    for mut line in rows(&draft, "items") {
        line.remove("id");
        let item = id(&line, "item").ok_or_else(|| Error::internal("no item"))?;
        if line.get("source_warehouse").is_none_or(Value::is_null) {
            if !out_lot.is_empty() {
                ensure_batch(k, ctx, tx, output, out_lot).await?;
                line.insert("batch_no".into(), json!(out_lot));
            }
            items.push(Value::Object(line));
            continue;
        }
        let (is_tracked, code) = tracked(tx, ctx.tenant, item).await?;
        let parts = spread(open.entry(item).or_default(), d(&line, "qty"), is_tracked, &code)?;
        for (lot, serial, qty) in parts {
            let mut l = line.clone();
            l.insert("qty".into(), json!(qty));
            if !lot.is_empty() {
                l.insert("batch_no".into(), json!(lot));
            }
            items.push(Value::Object(l));
            record_genealogy(tx, ctx.tenant, run, item, &lot, &serial, qty, output, out_lot).await?;
        }
    }
    let mut patch = Doc::new();
    patch.insert("items".into(), Value::Array(items));
    k.update_tx(&elevated, tx, "StockEntry", entry, patch, None).await?;
    k.submit_tx(&elevated, tx, "StockEntry", entry, None).await?;
    Ok(())
}

async fn ensure_batch(k: &Kernel, ctx: &Ctx, tx: &mut Tx, item: Uuid, lot: &str) -> Result<()> {
    let have: Option<i32> = sqlx::query_scalar("select 1 from e_batch where tenant_id = $1 and item = $2 and batch_id = $3").bind(ctx.tenant).bind(item).bind(lot).fetch_optional(&mut **tx).await?;
    if have.is_none() {
        k.insert_tx(&elevated(ctx), tx, "Batch", json!({"batch_id": lot, "item": item}).as_object().cloned().unwrap_or_default()).await?;
    }
    Ok(())
}

/// Scrap uses material and makes nothing: it leaves the books as an issue, and the genealogy keeps it.
async fn post_scrap(k: &Kernel, ctx: &Ctx, tx: &mut Tx, run: Uuid, doc: &Doc, scrap: Decimal) -> Result<()> {
    let elevated = elevated(ctx);
    let wo_id = id(doc, "work_order").ok_or_else(|| Error::internal("no order"))?;
    let wo = k.get_tx(&elevated, tx, "WorkOrder", wo_id, false).await?;
    let bom = k.get_tx(&elevated, tx, "Bom", id(&wo, "bom").ok_or_else(|| Error::internal("no bom"))?, false).await?;
    let output = id(&wo, "item").ok_or_else(|| Error::internal("no item"))?;
    let per = if d(&bom, "quantity").is_zero() { Decimal::ONE } else { d(&bom, "quantity") };
    let mut open = open_consumption(tx, ctx.tenant, run).await?;
    let mut items = Vec::new();
    for line in rows(&bom, "items") {
        let item = id(&line, "item").ok_or_else(|| Error::internal("no item"))?;
        let need = (d(&line, "qty") / per * scrap).round_dp(9);
        let (is_tracked, code) = tracked(tx, ctx.tenant, item).await?;
        let warehouse = id(&line, "source_warehouse").or(id(&wo, "source_warehouse"));
        for (lot, serial, qty) in spread(open.entry(item).or_default(), need, is_tracked, &code)? {
            record_genealogy(tx, ctx.tenant, run, item, &lot, &serial, qty, output, "").await?;
            items.push(json!({"item": item, "qty": qty, "source_warehouse": warehouse, "batch_no": lot}));
        }
    }
    if !items.is_empty() {
        let entry = json!({"company": doc.get("company"), "purpose": "Material Issue", "items": items, "remarks": format!("Scrap on run {}", s(doc, "number").unwrap_or_default())});
        let made = k.insert_tx(&elevated, tx, "StockEntry", entry.as_object().cloned().unwrap_or_default()).await?;
        k.submit_tx(&elevated, tx, "StockEntry", id(&made, "id").ok_or_else(|| Error::internal("no id"))?, None).await?;
    }
    Ok(())
}

/// Approves a revision. The approver cannot be its author, and the previous one is superseded.
pub async fn approve(k: &Kernel, ctx: &Ctx, tx: &mut Tx, revision: Uuid) -> Result<()> {
    let doc = k.get_tx(&sys(ctx), tx, "ProductionRevision", revision, true).await?;
    if s(&doc, "status") != Some("Draft") {
        return Err(Error::invalid("this revision is not a draft"));
    }
    if s(&doc, "created_by") == Some(ctx.user.to_string().as_str()) && !ctx.is_admin {
        return Err(Error::Forbidden("someone other than its author has to approve this".into()));
    }
    let item = id(&doc, "item").ok_or_else(|| Error::internal("no item"))?;
    sqlx::query("update e_production_revision set status = 'Superseded' where tenant_id = $1 and item = $2 and status = 'Approved'").bind(ctx.tenant).bind(item).execute(&mut **tx).await?;
    let values = json!({"status": "Approved", "approved_by": ctx.email, "approved_at": Utc::now().to_rfc3339()});
    k.set_values_tx(&sys(ctx), tx, "ProductionRevision", revision, values.as_object().cloned().unwrap_or_default()).await?;
    audit::record(tx, ctx, "ProductionRevision", revision, "mes.approved", None, json!({})).await
}

/// What the ledger established about the job sent to a controller. Stops are for a person to see.
pub async fn on_command(k: &Kernel, ctx: &Ctx, tx: &mut Tx, run: Uuid, state: State) -> Result<()> {
    let note = match state {
        State::Rejected => "The controller refused the job",
        State::Failed => "The controller reports the job failed",
        State::OutcomeUnknown => "Nobody can say what the controller did with this job: look at the machine",
        State::Expired => "The job was not picked up in time",
        _ => return Ok(()),
    };
    let doc = k.get_tx(&sys(ctx), tx, "ProductionRun", run, true).await?;
    if matches!(status(&doc), "Released" | "Running" | "Paused") {
        set(k, ctx, tx, run, json!({"note": note})).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: i64) -> Decimal {
        Decimal::from(v)
    }

    #[test]
    fn material_is_spread_over_lots_oldest_first() {
        let mut open = vec![("A".to_string(), String::new(), n(3)), ("B".to_string(), String::new(), n(10))];
        let parts = spread(&mut open, n(5), true, "X").unwrap();
        assert_eq!(parts, vec![("A".to_string(), String::new(), n(3)), ("B".to_string(), String::new(), n(2))]);
        assert_eq!(open[1].2, n(8), "what is left stays open for the next report");
    }

    #[test]
    fn a_tracked_item_must_be_accounted_for() {
        let mut open = vec![("A".to_string(), String::new(), n(3))];
        assert!(spread(&mut open, n(5), true, "X").is_err());
        let mut open = vec![("A".to_string(), String::new(), n(3))];
        let parts = spread(&mut open, n(5), false, "X").unwrap();
        assert_eq!(parts[1], (String::new(), String::new(), n(2)), "an untracked item is what the BOM says");
    }
}
