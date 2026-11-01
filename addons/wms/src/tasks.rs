//! A task is one movement of stock, by a person or a robot, and its states keep apart what the
//! assessment says must stay apart: a robot arriving is not stock delivered. A move finishes when
//! the right thing is confirmed at the right place, and until then its stock stays where the books
//! say, promised, with its custody stated. Nothing else can touch it.

use crate::position::{self, Cause, Key, Kind};
use crate::posting;
use crate::slotting;
use ferrum_domain::util::{elevated, sys};
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Tx, audit};
use ferrum_ops::commands::{self, Issuer, NewCommand};
use ferrum_ops::contract::State;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

/// How long a robot's mission may wait before it is out of date.
const MISSION_SECS: i64 = 4 * 3600;

const WORKFLOW: &str = "wms-task:";

#[derive(Debug, Clone, Default)]
pub struct NewTask {
    pub warehouse: Uuid,
    pub kind: &'static str,
    pub item: Uuid,
    pub qty: Decimal,
    pub lot: String,
    pub serial: String,
    pub owner: Option<Uuid>,
    pub unit: Option<Uuid>,
    pub status: String,
    pub from_bin: Uuid,
    pub to_bin: Option<Uuid>,
    pub priority: i64,
    pub after: Option<Uuid>,
    pub wave: Option<Uuid>,
    pub sales_order: Option<Uuid>,
    pub so_line: Option<String>,
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub struct Task {
    pub id: Uuid,
    pub warehouse: Uuid,
    pub kind: String,
    pub status: Option<String>,
    pub item: Uuid,
    pub qty: Decimal,
    pub done_qty: Option<Decimal>,
    pub lot: Option<String>,
    pub serial: Option<String>,
    pub owner: Option<Uuid>,
    pub stock_status: Option<String>,
    pub from_bin: Option<Uuid>,
    pub to_bin: Option<Uuid>,
    pub handling_unit: Option<Uuid>,
    pub robot: Option<Uuid>,
    pub command: Option<String>,
    pub custody: Option<String>,
    pub after_task: Option<Uuid>,
    pub wave: Option<Uuid>,
}

const TASK: &str = "select id, warehouse, kind, status, item, qty, done_qty, lot, serial, owner, stock_status, from_bin, to_bin, handling_unit, robot, command, custody, after_task, wave \
    from e_warehouse_task where tenant_id = $1 and id = $2";

impl Task {
    fn state(&self) -> &str {
        self.status.as_deref().unwrap_or("Open")
    }

    /// The stock this task moves, where it is now.
    fn source(&self) -> Result<Key> {
        Ok(Key {
            bin: self.from_bin.ok_or_else(|| Error::invalid("this task has no source bin"))?,
            item: self.item,
            owner: self.owner,
            lot: self.lot.clone().unwrap_or_default(),
            serial: self.serial.clone().unwrap_or_default(),
            unit: self.handling_unit,
            status: self.stock_status.clone().unwrap_or_else(|| "Available".into()),
        })
    }
}

pub async fn load(tx: &mut Tx, tenant: Uuid, id: Uuid) -> Result<Task> {
    sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(format!("{TASK} for update"))).bind(tenant).bind(id).fetch_optional(&mut **tx).await?.ok_or_else(|| Error::NotFound("task".into()))
}

async fn set(k: &Kernel, ctx: &Ctx, tx: &mut Tx, id: Uuid, values: Value) -> Result<()> {
    let values: Doc = values.as_object().cloned().unwrap_or_default();
    k.set_values_tx(&sys(ctx), tx, "WarehouseTask", id, values).await
}

async fn bin_code(tx: &mut Tx, tenant: Uuid, bin: Uuid) -> Result<String> {
    Ok(slotting::bin(tx, tenant, bin).await?.bin_code)
}

/// Promises the stock and writes the task. Nothing else can then take it.
pub async fn create(k: &Kernel, ctx: &Ctx, tx: &mut Tx, t: NewTask) -> Result<Uuid> {
    let goods = slotting::goods(tx, ctx.tenant, t.item).await?;
    let from = bin_code(tx, ctx.tenant, t.from_bin).await?;
    let to = match t.to_bin {
        Some(b) => bin_code(tx, ctx.tenant, b).await?,
        None => "somewhere to be chosen".into(),
    };
    let held: Option<Uuid> = sqlx::query_scalar(
        "select id from wms_position where tenant_id = $1 and bin_id = $2 and item_id = $3 and owner_id is not distinct from $4 and lot = $5 and serial = $6 \
         and unit_id is not distinct from $7 and status = $8 for update",
    )
    .bind(ctx.tenant)
    .bind(t.from_bin)
    .bind(t.item)
    .bind(t.owner)
    .bind(&t.lot)
    .bind(&t.serial)
    .bind(t.unit)
    .bind(if t.status.is_empty() { "Available" } else { &t.status })
    .fetch_optional(&mut **tx)
    .await?;
    let held = held.ok_or_else(|| Error::invalid(format!("{} is not in {from}", goods.item_code)))?;
    position::allocate(tx, ctx.tenant, held, t.qty).await?;
    let summary = format!("{} {} of {} from {from} to {to}", t.kind, t.qty.normalize(), goods.item_code);
    let doc = json!({
        "summary": summary, "warehouse": t.warehouse, "kind": t.kind, "item": t.item, "qty": t.qty, "lot": t.lot, "serial": t.serial, "owner": t.owner,
        "stock_status": if t.status.is_empty() { "Available" } else { &t.status }, "from_bin": t.from_bin, "to_bin": t.to_bin, "handling_unit": t.unit,
        "priority": t.priority, "after_task": t.after, "wave": t.wave, "sales_order": t.sales_order, "so_line": t.so_line,
    });
    let doc = k.insert_tx(&elevated(ctx), tx, "WarehouseTask", doc.as_object().cloned().unwrap_or_default()).await?;
    doc.get("id").and_then(Value::as_str).and_then(|s| s.parse().ok()).ok_or_else(|| Error::internal("a task without an id"))
}

/// Who a released task goes to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Executor {
    /// A person with a scanner, on the platform.
    Person,
    /// A robot, through the command ledger and the site's runtime.
    Robot(Uuid),
}

async fn ready(tx: &mut Tx, tenant: Uuid, t: &Task) -> Result<()> {
    if !matches!(t.state(), "Open" | "Released") {
        return Err(Error::invalid(format!("this task is {}", t.state())));
    }
    if let Some(first) = t.after_task {
        let done: Option<Option<String>> = sqlx::query_scalar("select status from e_warehouse_task where tenant_id = $1 and id = $2").bind(tenant).bind(first).fetch_optional(&mut **tx).await?;
        if !matches!(done.flatten().as_deref(), Some("Done" | "Short")) {
            return Err(Error::invalid("the task this one waits for has not finished"));
        }
    }
    let to = t.to_bin.ok_or_else(|| Error::invalid("choose where it goes"))?;
    let to = slotting::bin(tx, tenant, to).await?;
    slotting::check(tx, tenant, &to, t.item, t.qty).await?;
    if let Some(from) = t.from_bin {
        let from = slotting::bin(tx, tenant, from).await?;
        if from.zone_available == Some(false) {
            return Err(Error::invalid(format!("the zone of {} is not available", from.bin_code)));
        }
    }
    Ok(())
}

/// Releases a task. A person's task may go through the site's station, where it works offline; a
/// robot's always goes through the ledger, written in this transaction, so the mission exists only
/// if this commits.
pub async fn release(k: &Kernel, ctx: &Ctx, tx: &mut Tx, id: Uuid, executor: Executor) -> Result<()> {
    let t = load(tx, ctx.tenant, id).await?;
    ready(tx, ctx.tenant, &t).await?;
    let w = posting::must_manage(tx, ctx.tenant, t.warehouse).await?;
    let from = bin_code(tx, ctx.tenant, t.from_bin.ok_or_else(|| Error::invalid("this task has no source bin"))?).await?;
    let to = bin_code(tx, ctx.tenant, t.to_bin.ok_or_else(|| Error::invalid("choose where it goes"))?).await?;
    let code = slotting::goods(tx, ctx.tenant, t.item).await?.item_code;
    let unit = match t.handling_unit {
        Some(u) => sqlx::query_scalar::<_, String>("select unit_code from e_handling_unit where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(u).fetch_optional(&mut **tx).await?,
        None => None,
    };
    let (target, operation, payload) = match executor {
        Executor::Robot(robot) => (Some(robot), "warehouse.move", json!({"task": {"zone": Value::Null, "station": to}, "item": code, "qty": t.qty, "lot": t.lot, "from": from, "to": to, "unit": unit})),
        Executor::Person => match w.person_station {
            Some(station) => (Some(station), "warehouse.pick", json!({"item": code, "qty": t.qty, "lot": t.lot, "from": from, "to": to, "unit": unit, "verify": to})),
            None => (None, "", Value::Null),
        },
    };
    let Some(target) = target else {
        return set(k, ctx, tx, id, json!({"status": "Released"})).await;
    };
    // A new mission for each release: one that ended cannot be mistaken for this one.
    let attempt: i64 = sqlx::query_scalar("select count(*) from op_command where tenant_id = $1 and workflow_id = $2").bind(ctx.tenant).bind(format!("{WORKFLOW}{id}")).fetch_one(&mut **tx).await?;
    let issued = commands::issue(
        tx,
        ctx,
        NewCommand {
            target,
            operation: operation.into(),
            schema_version: 1,
            payload,
            expires_in_secs: MISSION_SECS,
            dedupe_key: Some(format!("{WORKFLOW}{id}:{attempt}")),
            workflow_id: Some(format!("{WORKFLOW}{id}")),
            step_id: Some(t.kind.clone()),
            issuer: Issuer::Engine,
            ..Default::default()
        },
    )
    .await?;
    if !matches!(issued.state, State::Queued) {
        return Err(Error::invalid("this device needs its work approved first, which a warehouse task cannot wait for"));
    }
    let robot = matches!(executor, Executor::Robot(_)).then_some(target);
    set(k, ctx, tx, id, json!({"status": "Released", "robot": robot, "command": issued.id.to_string()})).await
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct Confirm {
    /// The bin scanned at the source. A robot's task has no source scan.
    #[serde(default)]
    pub from_scan: Option<String>,
    /// The bin scanned at the destination.
    pub to_scan: String,
    /// How much moved. Less than the task's quantity leaves it short, with its reason.
    #[serde(default)]
    pub qty: Option<Decimal>,
    #[serde(default)]
    pub reason: Option<String>,
    /// A repeated scan of this id does nothing.
    #[serde(default)]
    pub scan_id: Option<Uuid>,
}

/// A person says the stock is where it should be. This is the only thing that finishes a move, for
/// people and robots alike: the destination is scanned, and a wrong one finishes nothing.
pub async fn confirm(k: &Kernel, ctx: &Ctx, tx: &mut Tx, id: Uuid, c: &Confirm) -> Result<()> {
    let t = load(tx, ctx.tenant, id).await?;
    let state = t.state().to_string();
    if matches!(state.as_str(), "Done" | "Short") && c.scan_id.is_some() {
        return Ok(());
    }
    if !matches!(state.as_str(), "Open" | "Released" | "Arrived") {
        return Err(Error::invalid(format!("this task is {state}")));
    }
    let arrived = state == "Arrived";
    if t.robot.is_some() && !arrived {
        return Err(Error::invalid("a robot has this: wait for it to arrive, or cancel it first"));
    }
    let source = t.source()?;
    if t.kind == "Pick" && ferrum_quality::hold_on(tx, ctx.tenant, t.item, &source.lot).await?.is_some() {
        return Err(Error::invalid("this stock went on quality hold after it was promised: leave it where it is"));
    }
    let from = slotting::bin(tx, ctx.tenant, source.bin).await?;
    if let Some(scan) = c.from_scan.as_deref().filter(|_| !arrived)
        && scan != from.bin_code
    {
        wrong_scan(tx, ctx, id, &from.bin_code, scan).await?;
        return Err(Error::invalid(format!("that is {scan}, and this task starts at {}", from.bin_code)));
    }
    let planned = t.to_bin.ok_or_else(|| Error::invalid("choose where it goes"))?;
    let planned = slotting::bin(tx, ctx.tenant, planned).await?;
    let to = if c.to_scan == planned.bin_code {
        planned
    } else if t.kind == "Pick" || arrived {
        // Picked stock goes where packing expects it, and a robot went where it was sent.
        wrong_scan(tx, ctx, id, &planned.bin_code, &c.to_scan).await?;
        return Err(Error::invalid(format!("that is {}, and this goes to {}", c.to_scan, planned.bin_code)));
    } else {
        let other = slotting::bin_by_code(tx, ctx.tenant, t.warehouse, &c.to_scan).await?;
        slotting::check(tx, ctx.tenant, &other, t.item, t.qty).await?;
        other
    };
    let qty = c.qty.unwrap_or(t.qty);
    if qty <= Decimal::ZERO || qty > t.qty {
        return Err(Error::field("qty", format!("between nothing and {}", t.qty.normalize())));
    }
    if qty < t.qty && c.reason.as_deref().unwrap_or_default().trim().is_empty() {
        return Err(Error::field("reason", "say why less was moved"));
    }
    let kind = kind_of(&t.kind);
    let mut dest = source.clone();
    dest.bin = to.id;
    let cause = Cause { task: Some(id), reason: c.reason.clone(), operation: c.scan_id.map(|s| format!("scan:{s}")), ..Default::default() };
    position::transfer(tx, ctx, t.warehouse, kind, &source, &dest, qty, qty, &cause).await?;
    if qty < t.qty {
        position::release(tx, ctx.tenant, &source, t.qty - qty).await?;
    }
    let status = if qty < t.qty { "Short" } else { "Done" };
    set(k, ctx, tx, id, json!({"status": status, "done_qty": qty, "custody": "In Place", "reason": c.reason, "to_bin": to.id})).await?;
    if let Some(unit) = t.handling_unit {
        place_unit(tx, ctx.tenant, unit, Some(to.id), "In Place", None).await?;
    }
    if let Some(wave) = t.wave {
        crate::waves::progress(k, ctx, tx, wave).await?;
    }
    Ok(())
}

fn kind_of(kind: &str) -> Kind {
    match kind {
        "Putaway" => Kind::Putaway,
        "Pick" => Kind::Pick,
        "Replenish" => Kind::Replenish,
        _ => Kind::Move,
    }
}

/// A wrong identity at a station is kept: it may be the first sign of a mix-up.
async fn wrong_scan(tx: &mut Tx, ctx: &Ctx, task: Uuid, expected: &str, scanned: &str) -> Result<()> {
    audit::record(tx, ctx, "WarehouseTask", task, "wms.wrong_scan", None, json!({"expected": expected, "scanned": scanned})).await
}

pub async fn place_unit(tx: &mut Tx, tenant: Uuid, unit: Uuid, bin: Option<Uuid>, custody: &str, custodian: Option<&str>) -> Result<()> {
    sqlx::query("update e_handling_unit set bin = coalesce($3, bin), custody = $4, custodian = $5 where tenant_id = $1 and id = $2")
        .bind(tenant)
        .bind(unit)
        .bind(bin)
        .bind(custody)
        .bind(custodian)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Stops a task. What a robot may be carrying can only be asked to stop, and until it answers the
/// stock is where nobody can say.
pub async fn cancel(k: &Kernel, ctx: &Ctx, tx: &mut Tx, id: Uuid, reason: &str) -> Result<String> {
    let t = load(tx, ctx.tenant, id).await?;
    match t.state() {
        "Open" | "Released" if t.command.is_none() => {
            position::release(tx, ctx.tenant, &t.source()?, t.qty).await?;
            set(k, ctx, tx, id, json!({"status": "Cancelled", "reason": reason})).await?;
            Ok("Cancelled".into())
        }
        "Open" | "Released" | "Moving" => {
            let command: Uuid = t.command.as_deref().and_then(|c| c.parse().ok()).ok_or_else(|| Error::internal("a task without its command"))?;
            let state = commands::cancel(tx, ctx, command, reason).await?;
            if state == State::Aborted {
                position::release(tx, ctx.tenant, &t.source()?, t.qty).await?;
                set(k, ctx, tx, id, json!({"status": "Cancelled", "reason": reason})).await?;
                return Ok("Cancelled".into());
            }
            Ok(t.state().to_string())
        }
        other => Err(Error::invalid(format!("this task is {other}"))),
    }
}

/// A person found the stock. Where it was found is now where it is; the task ends, and what
/// nobody could say about its command is settled by the same finding.
pub async fn recover(k: &Kernel, ctx: &Ctx, tx: &mut Tx, id: Uuid, found_bin: &str, qty: Decimal, note: &str) -> Result<()> {
    if note.trim().is_empty() {
        return Err(Error::field("note", "say what you found"));
    }
    let t = load(tx, ctx.tenant, id).await?;
    if t.state() != "Exception" {
        return Err(Error::invalid("this task is not waiting for anyone to find it"));
    }
    let source = t.source()?;
    let bin = slotting::bin_by_code(tx, ctx.tenant, t.warehouse, found_bin).await?;
    if qty > t.qty || qty < Decimal::ZERO {
        return Err(Error::field("qty", format!("between nothing and {}", t.qty.normalize())));
    }
    if bin.id == source.bin || qty.is_zero() {
        position::release(tx, ctx.tenant, &source, t.qty).await?;
    } else {
        let mut dest = source.clone();
        dest.bin = bin.id;
        let cause = Cause { task: Some(id), reason: Some(note.to_string()), ..Default::default() };
        position::transfer(tx, ctx, t.warehouse, Kind::Recover, &source, &dest, qty, qty, &cause).await?;
        if qty < t.qty {
            position::release(tx, ctx.tenant, &source, t.qty - qty).await?;
        }
    }
    let moved = bin.id != source.bin && !qty.is_zero();
    if let Some(command) = t.command.as_deref().and_then(|c| c.parse().ok()) {
        let unknown: Option<bool> = sqlx::query_scalar("select state = 'OutcomeUnknown' from op_command where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(command).fetch_optional(&mut **tx).await?;
        if unknown == Some(true) {
            commands::reconcile(tx, ctx, command, if moved { State::Completed } else { State::Failed }, note).await?;
        }
    }
    if let Some(unit) = t.handling_unit {
        place_unit(tx, ctx.tenant, unit, Some(bin.id), "In Place", None).await?;
    }
    let status = if moved && qty == t.qty { "Done" } else { "Cancelled" };
    set(k, ctx, tx, id, json!({"status": status, "done_qty": qty, "custody": "In Place", "reason": note})).await
}

/// What the ledger established about a task's mission. Delivered at least once, in any order, so
/// each step only looks at where the task stands.
pub async fn on_command(k: &Kernel, ctx: &Ctx, tx: &mut Tx, id: Uuid, state: State, command: Uuid, outcome: &Value) -> Result<()> {
    let t = load(tx, ctx.tenant, id).await?;
    if t.command.as_deref() != Some(command.to_string().as_str()) {
        return Ok(());
    }
    let person = t.robot.is_none();
    let label = robot_label(tx, ctx.tenant, t.robot).await?;
    match (state, t.state()) {
        (State::Accepted | State::Executing, "Released") => {
            let custody = if person { "In Place" } else { "In Transit" };
            set(k, ctx, tx, id, json!({"status": "Moving", "custody": custody})).await?;
            if let Some(unit) = t.handling_unit.filter(|_| !person) {
                place_unit(tx, ctx.tenant, unit, None, "In Transit", label.as_deref()).await?;
            }
        }
        (State::Completed, "Released" | "Moving") => {
            if person {
                // The station made them scan the destination before it would let them finish.
                let to = outcome["evidence"]["found"]["scan"].as_str().unwrap_or_default().to_string();
                let c = Confirm { to_scan: to, scan_id: Some(command), ..Default::default() };
                set(k, ctx, tx, id, json!({"status": "Arrived"})).await?;
                confirm(k, ctx, tx, id, &c).await?;
            } else {
                // The robot says it got there. The stock has not moved until someone scans it there.
                set(k, ctx, tx, id, json!({"status": "Arrived", "custody": "In Transit", "reason": "Arrived: confirm the unit at its destination"})).await?;
            }
        }
        (State::Rejected | State::Expired, "Released") => {
            set(k, ctx, tx, id, json!({"status": "Open", "command": Value::Null, "robot": Value::Null, "reason": "The mission never started"})).await?;
        }
        (State::Aborted, "Released") => {
            position::release(tx, ctx.tenant, &t.source()?, t.qty).await?;
            set(k, ctx, tx, id, json!({"status": "Cancelled", "reason": "Stopped before it started"})).await?;
        }
        (State::Failed | State::Aborted | State::OutcomeUnknown, "Released" | "Moving" | "Arrived") => {
            let why = match state {
                State::OutcomeUnknown => "Nobody can say what happened to this mission. Find the stock",
                State::Aborted => "Stopped on the way. Find the stock and say where it is",
                _ => "The mission failed. Find the stock and say where it is",
            };
            set(k, ctx, tx, id, json!({"status": "Exception", "custody": "Uncertain", "reason": why})).await?;
            if let Some(unit) = t.handling_unit {
                place_unit(tx, ctx.tenant, unit, None, "Uncertain", label.as_deref()).await?;
            }
        }
        _ => {}
    }
    Ok(())
}

async fn robot_label(tx: &mut Tx, tenant: Uuid, robot: Option<Uuid>) -> Result<Option<String>> {
    match robot {
        Some(r) => Ok(sqlx::query_scalar("select device_code from e_device where tenant_id = $1 and id = $2").bind(tenant).bind(r).fetch_optional(&mut **tx).await?),
        None => Ok(None),
    }
}

/// The task a workflow id names.
pub fn task_of(workflow: &str) -> Option<Uuid> {
    workflow.strip_prefix(WORKFLOW)?.parse().ok()
}
