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
    /// The production run this brings material to.
    pub run: Option<Uuid>,
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
    pub production_run: Option<String>,
}

const TASK: &str = "select id, warehouse, kind, status, item, qty, done_qty, lot, serial, owner, stock_status, from_bin, to_bin, handling_unit, robot, command, custody, after_task, wave, production_run \
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
    sqlx::query_as::<_, Task>(sqlx::AssertSqlSafe(format!("{TASK} for update")))
        .bind(tenant)
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Error::NotFound("task".into()))
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
        "priority": t.priority, "after_task": t.after, "wave": t.wave, "sales_order": t.sales_order, "so_line": t.so_line, "production_run": t.run.map(|r| r.to_string()),
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
        let done: Option<Option<String>> = sqlx::query_scalar("select status from e_warehouse_task where tenant_id = $1 and id = $2")
            .bind(tenant)
            .bind(first)
            .fetch_optional(&mut **tx)
            .await?;
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

/// What a release came to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Released {
    pub command: Option<Uuid>,
    /// It had been released already, so nothing new went out.
    pub repeated: bool,
}

/// What a load is known by, for whoever has to scan it: its unit's label, or else what it is. An item
/// goes by its code or its barcode.
async fn load_of(tx: &mut Tx, tenant: Uuid, t: &Task) -> Result<Value> {
    if let Some(unit) = t.handling_unit {
        let label: String =
            sqlx::query_scalar("select unit_code from e_handling_unit where tenant_id = $1 and id = $2").bind(tenant).bind(unit).fetch_one(&mut **tx).await?;
        return Ok(json!({"unit": label}));
    }
    let (code, barcode): (String, Option<String>) = sqlx::query_as("select item_code, nullif(barcode, '') from e_item where tenant_id = $1 and id = $2")
        .bind(tenant)
        .bind(t.item)
        .fetch_one(&mut **tx)
        .await?;
    let mut load = json!({"item": barcode.into_iter().chain([code]).collect::<Vec<_>>()});
    for (key, value) in [("lot", &t.lot), ("serial", &t.serial)] {
        if let Some(value) = value.as_deref().filter(|v| !v.is_empty()) {
            load[key] = json!(value);
        }
    }
    Ok(load)
}

/// Releases a task. A person's task may go through the site's station, where it works offline; a
/// robot's always goes through the ledger, written in this transaction, so the mission exists only
/// if this commits.
///
/// Released is a fact about the task, so saying it again changes nothing: a lost answer must not
/// become a second mission. A new attempt comes only from Open, which is where a mission that never
/// started leaves a task. One that may have moved anything ends with someone finding the stock.
pub async fn release(k: &Kernel, ctx: &Ctx, tx: &mut Tx, id: Uuid, executor: Executor) -> Result<Released> {
    let t = load(tx, ctx.tenant, id).await?;
    let person = executor == Executor::Person;
    if matches!(t.state(), "Released" | "Moving" | "Arrived") {
        let same = match executor {
            Executor::Robot(robot) => t.robot == Some(robot),
            Executor::Person => t.robot.is_none(),
        };
        if let Some(command) = t.command.as_deref() {
            if !same {
                return Err(Error::invalid("this is already on its way: cancel it before giving it to someone else"));
            }
            return Ok(Released { command: Some(command.parse().map_err(|_| Error::internal("a task with a command that is not one"))?), repeated: true });
        }
        if person {
            return Ok(Released { command: None, repeated: true });
        }
    }
    ready(tx, ctx.tenant, &t).await?;
    let w = posting::must_manage(tx, ctx.tenant, t.warehouse).await?;
    let from = bin_code(tx, ctx.tenant, t.from_bin.ok_or_else(|| Error::invalid("this task has no source bin"))?).await?;
    let to = bin_code(tx, ctx.tenant, t.to_bin.ok_or_else(|| Error::invalid("choose where it goes"))?).await?;
    let code = slotting::goods(tx, ctx.tenant, t.item).await?.item_code;
    let identity = load_of(tx, ctx.tenant, &t).await?;
    let unit = identity["unit"].as_str().map(str::to_string);
    let (target, operation, payload) = match executor {
        Executor::Robot(robot) => (
            Some(robot),
            "warehouse.move",
            json!({"task": {"zone": Value::Null, "station": to}, "item": code, "qty": t.qty, "lot": t.lot, "from": from, "to": to, "unit": unit}),
        ),
        Executor::Person => match w.person_station {
            Some(station) => (
                Some(station),
                "warehouse.pick",
                json!({"item": code, "qty": t.qty, "lot": t.lot, "from": from, "to": to, "unit": unit, "verify": to, "load": identity}),
            ),
            None => (None, "", Value::Null),
        },
    };
    let Some(target) = target else {
        set(k, ctx, tx, id, json!({"status": "Released"})).await?;
        return Ok(Released { command: None, repeated: false });
    };
    // A new mission for each attempt: one that ended cannot be mistaken for this one.
    let workflow = format!("{WORKFLOW}{id}");
    let attempt: i64 = sqlx::query_scalar("select count(*) from op_command where tenant_id = $1 and workflow_id = $2")
        .bind(ctx.tenant)
        .bind(&workflow)
        .fetch_one(&mut **tx)
        .await?;
    let issued = commands::issue(
        tx,
        ctx,
        NewCommand {
            target,
            operation: operation.into(),
            schema_version: 1,
            payload,
            expires_in_secs: MISSION_SECS,
            dedupe_key: Some(format!("{workflow}:{attempt}")),
            exclusive_key: Some(workflow.clone()),
            workflow_id: Some(workflow),
            step_id: Some(t.kind.clone()),
            issuer: Issuer::Engine,
            ..Default::default()
        },
    )
    .await?;
    if issued.repeated {
        return Err(Error::Conflict("a mission for this task is still alive: settle it before sending another".into()));
    }
    if !matches!(issued.state, State::Queued) {
        return Err(Error::invalid("this device needs its work approved first, which a warehouse task cannot wait for"));
    }
    let robot = matches!(executor, Executor::Robot(_)).then_some(target);
    set(k, ctx, tx, id, json!({"status": "Released", "robot": robot, "command": issued.id.to_string()})).await?;
    Ok(Released { command: Some(issued.id), repeated: false })
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct Confirm {
    /// The bin scanned at the source. A robot's task has no source scan.
    #[serde(default)]
    pub from_scan: Option<String>,
    /// The bin scanned at the destination.
    pub to_scan: String,
    /// The label on the handling unit, when the stock is in one.
    #[serde(default)]
    pub unit_scan: Option<String>,
    /// The item's code or barcode, for loose stock.
    #[serde(default)]
    pub item_scan: Option<String>,
    #[serde(default)]
    pub lot_scan: Option<String>,
    #[serde(default)]
    pub serial_scan: Option<String>,
    /// A manager saying why the load cannot be scanned. It finishes the task with that on record, and
    /// never looks like a scan.
    #[serde(default)]
    pub manual: Option<String>,
    /// How much moved. Less than the task's quantity leaves it short, with its reason.
    #[serde(default)]
    pub qty: Option<Decimal>,
    #[serde(default)]
    pub reason: Option<String>,
    /// A repeated scan of this id does nothing.
    #[serde(default)]
    pub scan_id: Option<Uuid>,
    /// Nobody here scanned: this is what a site's station recorded.
    #[serde(skip)]
    pub station: bool,
}

/// A request refused with something on record. Nothing else in its transaction changed, so the
/// caller commits and then says no: an error would roll the record back with it.
#[derive(Debug, PartialEq)]
#[must_use]
pub struct Refused(pub String);

impl From<Refused> for Error {
    fn from(r: Refused) -> Error {
        Error::invalid(r.0)
    }
}

fn scanned(v: &Option<String>) -> &str {
    v.as_deref().map_or("", str::trim)
}

/// Whether what was scanned is what this task moves. The location alone says nothing about which
/// tote reached it. `Ok(Err(..))` is a wrong identity, which is kept.
async fn identified(tx: &mut Tx, ctx: &Ctx, t: &Task, c: &Confirm) -> Result<std::result::Result<Value, Refused>> {
    let want = load_of(tx, ctx.tenant, t).await?;
    let wrong = async |tx: &mut Tx, expected: &str, got: &str| -> Result<std::result::Result<Value, Refused>> {
        wrong_scan(tx, ctx, t.id, expected, got).await?;
        Ok(Err(Refused(format!("that is {got}, and this task moves {expected}"))))
    };
    if let Some(label) = want["unit"].as_str() {
        if scanned(&c.unit_scan).is_empty() {
            return Err(Error::field("unit_scan", format!("scan the label on {label}")));
        }
        if scanned(&c.unit_scan) != label {
            return wrong(tx, label, scanned(&c.unit_scan)).await;
        }
        return Ok(Ok(json!({"unit": label})));
    }
    let code = want["item"].as_array().and_then(|a| a.last()).and_then(Value::as_str).unwrap_or_default().to_string();
    if scanned(&c.item_scan).is_empty() {
        return Err(Error::field("item_scan", format!("scan {code}")));
    }
    if slotting::item_by_scan(tx, ctx.tenant, scanned(&c.item_scan)).await? != Some(t.item) {
        return wrong(tx, &code, scanned(&c.item_scan)).await;
    }
    for (key, field, got) in [("lot", "lot_scan", &c.lot_scan), ("serial", "serial_scan", &c.serial_scan)] {
        let Some(expected) = want[key].as_str() else { continue };
        if scanned(got).is_empty() {
            return Err(Error::field(field, format!("scan the {key}: {expected}")));
        }
        if scanned(got) != expected {
            return wrong(tx, expected, scanned(got)).await;
        }
    }
    Ok(Ok(json!({"item": code, "lot": want["lot"], "serial": want["serial"]})))
}

/// Says the stock is where it should be. This is the only thing that finishes a move, for people and
/// robots alike: the right thing is scanned at the right place, and anything else finishes nothing.
/// Whatever is refused for a wrong identity comes back as [`Refused`], with the attempt on record.
pub async fn confirm(k: &Kernel, ctx: &Ctx, tx: &mut Tx, id: Uuid, c: &Confirm) -> Result<Option<Refused>> {
    let t = load(tx, ctx.tenant, id).await?;
    let state = t.state().to_string();
    if matches!(state.as_str(), "Done" | "Short") {
        // The same scan again is a lost answer. Anything else is someone who thinks it is still open.
        if let Some(scan) = c.scan_id
            && sqlx::query_scalar::<_, i32>("select 1 from wms_move where tenant_id = $1 and task_id = $2 and operation = $3")
                .bind(ctx.tenant)
                .bind(id)
                .bind(format!("scan:{scan}"))
                .fetch_optional(&mut **tx)
                .await?
                .is_some()
        {
            return Ok(None);
        }
    }
    if !matches!(state.as_str(), "Open" | "Released" | "Arrived") {
        return Err(Error::invalid(format!("this task is {state}")));
    }
    let arrived = state == "Arrived";
    if t.command.is_some() && !arrived {
        return Err(Error::invalid(if t.robot.is_some() {
            "a robot has this: wait for it to arrive, or cancel it first"
        } else {
            "this is at the station: finish it there, or cancel it first"
        }));
    }
    let source = t.source()?;
    if (t.kind == "Pick" || t.production_run.is_some()) && ferrum_quality::hold_on(tx, ctx.tenant, t.item, &source.lot).await?.is_some() {
        return Err(Error::invalid("this stock went on quality hold after it was promised: leave it where it is"));
    }
    let from = slotting::bin(tx, ctx.tenant, source.bin).await?;
    if let Some(scan) = c.from_scan.as_deref().filter(|_| !arrived)
        && scan != from.bin_code
    {
        wrong_scan(tx, ctx, id, &from.bin_code, scan).await?;
        return Ok(Some(Refused(format!("that is {scan}, and this task starts at {}", from.bin_code))));
    }
    let planned = t.to_bin.ok_or_else(|| Error::invalid("choose where it goes"))?;
    let planned = slotting::bin(tx, ctx.tenant, planned).await?;
    let to = if c.to_scan == planned.bin_code {
        planned
    } else if t.kind == "Pick" || t.production_run.is_some() || arrived {
        // Picked stock goes where packing expects it, the line's where the run expects it, and a
        // robot went where it was sent.
        wrong_scan(tx, ctx, id, &planned.bin_code, &c.to_scan).await?;
        return Ok(Some(Refused(format!("that is {}, and this goes to {}", c.to_scan, planned.bin_code))));
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
    let manual = c.manual.as_deref().map(str::trim).filter(|m| !m.is_empty());
    let (evidence, evidence_source) = if let Some(why) = manual {
        if !ctx.is_admin && !ctx.has_role("Warehouse Manager") {
            return Err(Error::Forbidden("only a manager can finish this without scanning the load".into()));
        }
        (json!({"manual": why, "by": ctx.email}), "Manual")
    } else {
        match identified(tx, ctx, &t, c).await? {
            Ok(load) => (load, if c.station { "Station" } else { "Scan" }),
            Err(refused) => return Ok(Some(refused)),
        }
    };
    let kind = kind_of(&t.kind);
    let mut dest = source.clone();
    dest.bin = to.id;
    let cause = Cause { task: Some(id), reason: c.reason.clone(), operation: c.scan_id.map(|s| format!("scan:{s}")), ..Default::default() };
    if !position::transfer(tx, ctx, t.warehouse, kind, &source, &dest, qty, qty, &cause).await? {
        return Err(Error::invalid("that scan was already used for something else, and moved nothing here"));
    }
    if manual.is_some() {
        audit::record(tx, ctx, "WarehouseTask", id, "wms.manual_confirm", None, json!({"why": manual, "qty": qty, "to": to.bin_code})).await?;
    }
    // What a person left behind is still where it was. What a robot did not bring could be anywhere,
    // so it stays promised and out of reach until someone finds it.
    let lost = qty < t.qty && t.robot.is_some();
    if qty < t.qty && !lost {
        position::release(tx, ctx.tenant, &source, t.qty - qty).await?;
    }
    let (status, custody) = match (qty < t.qty, lost) {
        (_, true) => ("Exception", "Uncertain"),
        (true, _) => ("Short", "In Place"),
        _ => ("Done", "In Place"),
    };
    let evidence = json!({"load": evidence, "qty": qty, "to": to.bin_code});
    let reason = if lost { Some(format!("Only {} arrived: find the rest and say where it is", qty.normalize())) } else { c.reason.clone() };
    set(k, ctx, tx, id, json!({"status": status, "done_qty": qty, "custody": custody, "reason": reason, "to_bin": to.id, "evidence": evidence, "evidence_source": evidence_source}))
        .await?;
    if let Some(unit) = t.handling_unit {
        place_unit(tx, ctx.tenant, unit, Some(to.id), "In Place", None).await?;
    }
    if let Some(run) = run_of(&t) {
        crate::waves::arrived_at_line(tx, ctx, run, t.warehouse, t.item, qty).await?;
    }
    finished(k, ctx, tx, &t).await?;
    Ok(None)
}

fn run_of(t: &Task) -> Option<Uuid> {
    t.production_run.as_deref().and_then(|r| r.parse().ok())
}

/// What a task's end means for what it was part of.
async fn finished(k: &Kernel, ctx: &Ctx, tx: &mut Tx, t: &Task) -> Result<()> {
    if let Some(wave) = t.wave {
        crate::waves::progress(k, ctx, tx, wave).await?;
    }
    if let Some(run) = run_of(t) {
        crate::waves::supply_progress(tx, ctx, run).await?;
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

/// A wrong identity is kept: it may be the first sign of a mix-up.
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
            finished(k, ctx, tx, &t).await?;
            Ok("Cancelled".into())
        }
        "Open" | "Released" | "Moving" => {
            let command: Uuid = t.command.as_deref().and_then(|c| c.parse().ok()).ok_or_else(|| Error::internal("a task without its command"))?;
            let state = commands::cancel(tx, ctx, command, reason).await?;
            if state == State::Aborted {
                position::release(tx, ctx.tenant, &t.source()?, t.qty).await?;
                set(k, ctx, tx, id, json!({"status": "Cancelled", "reason": reason})).await?;
                finished(k, ctx, tx, &t).await?;
                return Ok("Cancelled".into());
            }
            Ok(t.state().to_string())
        }
        other => Err(Error::invalid(format!("this task is {other}"))),
    }
}

/// A person found the stock. Where it was found is now where it is; the task ends, and what
/// nobody could say about its command is settled by the same finding. What already arrived is not
/// part of what was lost.
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
    let done = t.done_qty.unwrap_or_default();
    let left = t.qty - done;
    if qty > left || qty < Decimal::ZERO {
        return Err(Error::field("qty", format!("between nothing and {}", left.normalize())));
    }
    if bin.id == source.bin || qty.is_zero() {
        position::release(tx, ctx.tenant, &source, left).await?;
    } else {
        let mut dest = source.clone();
        dest.bin = bin.id;
        let cause = Cause { task: Some(id), reason: Some(note.to_string()), ..Default::default() };
        position::transfer(tx, ctx, t.warehouse, Kind::Recover, &source, &dest, qty, qty, &cause).await?;
        if qty < left {
            position::release(tx, ctx.tenant, &source, left - qty).await?;
        }
    }
    let moved = bin.id != source.bin && !qty.is_zero();
    if let Some(command) = t.command.as_deref().and_then(|c| c.parse().ok()) {
        let unknown: Option<bool> = sqlx::query_scalar("select state = 'OutcomeUnknown' from op_command where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(command)
            .fetch_optional(&mut **tx)
            .await?;
        if unknown == Some(true) {
            commands::reconcile(tx, ctx, command, if moved { State::Completed } else { State::Failed }, note).await?;
        }
    }
    if let Some(unit) = t.handling_unit {
        place_unit(tx, ctx.tenant, unit, Some(bin.id), "In Place", None).await?;
    }
    let total = done + if moved { qty } else { Decimal::ZERO };
    let status = if total == t.qty {
        "Done"
    } else if total > Decimal::ZERO {
        "Short"
    } else {
        "Cancelled"
    };
    let evidence = json!({"found": note, "by": ctx.email, "bin": bin.bin_code, "qty": qty});
    set(
        k,
        ctx,
        tx,
        id,
        json!({"status": status, "done_qty": total, "custody": "In Place", "reason": note, "evidence": evidence, "evidence_source": "Recovery"}),
    )
    .await?;
    finished(k, ctx, tx, &t).await
}

/// A report that cannot finish a task, and will not change if it is tried again.
pub async fn stuck(k: &Kernel, ctx: &Ctx, tx: &mut Tx, id: Uuid, why: &str) -> Result<()> {
    let t = load(tx, ctx.tenant, id).await?;
    if !matches!(t.state(), "Done" | "Short" | "Cancelled") {
        set(k, ctx, tx, id, json!({"status": "Exception", "custody": "Uncertain", "reason": why})).await?;
        finished(k, ctx, tx, &t).await?;
    }
    Ok(())
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
                // The station made them scan the place and the load before it would let them finish.
                let found = &outcome["evidence"]["found"];
                let said = |key: &str| found["load"][key].as_str().map(str::to_string);
                let c = Confirm {
                    to_scan: found["scan"].as_str().unwrap_or_default().to_string(),
                    unit_scan: said("unit"),
                    item_scan: said("item"),
                    lot_scan: said("lot"),
                    serial_scan: said("serial"),
                    scan_id: Some(command),
                    station: true,
                    ..Default::default()
                };
                set(k, ctx, tx, id, json!({"status": "Arrived"})).await?;
                if let Some(Refused(why)) = confirm(k, ctx, tx, id, &c).await? {
                    stuck(k, ctx, tx, id, &format!("The station's scan does not match: {why}")).await?;
                }
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
            finished(k, ctx, tx, &t).await?;
        }
        // Turned down after it was taken: it may already have been moving.
        (State::Failed | State::Aborted | State::OutcomeUnknown | State::Rejected, "Released" | "Moving" | "Arrived") => {
            let why = match state {
                State::OutcomeUnknown => "Nobody can say what happened to this mission. Find the stock",
                State::Aborted => "Stopped on the way. Find the stock and say where it is",
                State::Rejected => "The mission was turned down after it was taken. Find the stock and say where it is",
                _ => "The mission failed. Find the stock and say where it is",
            };
            set(k, ctx, tx, id, json!({"status": "Exception", "custody": "Uncertain", "reason": why})).await?;
            if let Some(unit) = t.handling_unit {
                place_unit(tx, ctx.tenant, unit, None, "Uncertain", label.as_deref()).await?;
            }
            finished(k, ctx, tx, &t).await?;
        }
        _ => {}
    }
    Ok(())
}

async fn robot_label(tx: &mut Tx, tenant: Uuid, robot: Option<Uuid>) -> Result<Option<String>> {
    match robot {
        Some(r) => {
            Ok(sqlx::query_scalar("select device_code from e_device where tenant_id = $1 and id = $2").bind(tenant).bind(r).fetch_optional(&mut **tx).await?)
        }
        None => Ok(None),
    }
}

/// The task a workflow id names.
pub fn task_of(workflow: &str) -> Option<Uuid> {
    workflow.strip_prefix(WORKFLOW)?.parse().ok()
}
