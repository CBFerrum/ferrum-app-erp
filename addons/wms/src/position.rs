//! Positions: what is where, by bin, item, owner, lot, serial, unit and status. They change only
//! here, each change with its row in `wms_move`, so the ledger always explains the position.

use chrono::NaiveDate;
use ferrum_kernel::{Ctx, Error, Result, Tx};
use rust_decimal::Decimal;
use serde_json::Value;
use uuid::Uuid;

/// One stock dimension of one bin.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Key {
    pub bin: Uuid,
    pub item: Uuid,
    pub owner: Option<Uuid>,
    pub lot: String,
    pub serial: String,
    pub unit: Option<Uuid>,
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Receive,
    Issue,
    Putaway,
    Pick,
    Replenish,
    Move,
    Status,
    Adjust,
    Count,
    Return,
    Recover,
    Opening,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Kind::Receive => "Receive",
            Kind::Issue => "Issue",
            Kind::Putaway => "Putaway",
            Kind::Pick => "Pick",
            Kind::Replenish => "Replenish",
            Kind::Move => "Move",
            Kind::Status => "Status",
            Kind::Adjust => "Adjust",
            Kind::Count => "Count",
            Kind::Return => "Return",
            Kind::Recover => "Recover",
            Kind::Opening => "Opening",
        }
    }
}

/// Why a change happened, for the ledger.
#[derive(Debug, Clone, Default)]
pub struct Cause {
    pub task: Option<Uuid>,
    pub source: Option<(String, Uuid)>,
    pub reason: Option<String>,
    /// The same operation lands once.
    pub operation: Option<String>,
}

impl Cause {
    pub fn task(id: Uuid) -> Cause {
        Cause { task: Some(id), ..Default::default() }
    }

    pub fn why(reason: &str) -> Cause {
        Cause { reason: Some(reason.to_string()), ..Default::default() }
    }

    pub fn source(entity: &str, id: Uuid) -> Cause {
        Cause { source: Some((entity.to_string(), id)), ..Default::default() }
    }
}

struct Side<'a> {
    bin: Option<Uuid>,
    unit: Option<Uuid>,
    status: Option<&'a str>,
}

#[derive(sqlx::FromRow, Debug, Clone)]
pub struct Held {
    pub id: Uuid,
    pub qty: Decimal,
    pub allocated: Decimal,
    pub expiry: Option<NaiveDate>,
    pub received_on: NaiveDate,
}

const FIND: &str = "select id, qty, allocated, expiry, received_on from wms_position where tenant_id = $1 and bin_id = $2 and item_id = $3 \
    and owner_id is not distinct from $4 and lot = $5 and serial = $6 and unit_id is not distinct from $7 and status = $8 for update";

async fn find(tx: &mut Tx, tenant: Uuid, k: &Key) -> Result<Option<Held>> {
    Ok(sqlx::query_as(FIND).bind(tenant).bind(k.bin).bind(k.item).bind(k.owner).bind(&k.lot).bind(&k.serial).bind(k.unit).bind(&k.status).fetch_optional(&mut **tx).await?)
}

/// Adds to a position, creating it if it is new. The oldest expiry of what is merged stands, so
/// nothing is ever shown as lasting longer than it does.
pub async fn add(tx: &mut Tx, tenant: Uuid, warehouse: Uuid, k: &Key, qty: Decimal, expiry: Option<NaiveDate>, received_on: Option<NaiveDate>) -> Result<()> {
    if qty <= Decimal::ZERO {
        return Err(Error::internal("a position is added to by a positive quantity"));
    }
    sqlx::query(
        "insert into wms_position (tenant_id, id, warehouse_id, bin_id, item_id, owner_id, lot, serial, unit_id, status, expiry, received_on, qty) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, coalesce($12, current_date), $13) \
         on conflict (tenant_id, bin_id, item_id, coalesce(owner_id, '00000000-0000-0000-0000-000000000000'), lot, serial, coalesce(unit_id, '00000000-0000-0000-0000-000000000000'), status) \
         do update set qty = wms_position.qty + excluded.qty, expiry = least(wms_position.expiry, excluded.expiry), received_on = least(wms_position.received_on, excluded.received_on)",
    )
    .bind(tenant)
    .bind(Uuid::now_v7())
    .bind(warehouse)
    .bind(k.bin)
    .bind(k.item)
    .bind(k.owner)
    .bind(&k.lot)
    .bind(&k.serial)
    .bind(k.unit)
    .bind(&k.status)
    .bind(expiry)
    .bind(received_on)
    .bind(qty)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Takes from a position. `released` is how much of it was allocated to the work taking it.
pub async fn take(tx: &mut Tx, tenant: Uuid, k: &Key, qty: Decimal, released: Decimal) -> Result<Held> {
    let held = find(tx, tenant, k).await?.ok_or_else(|| Error::invalid("that stock is not in that bin"))?;
    if held.qty < qty {
        return Err(Error::invalid(format!("only {} of it is in that bin", held.qty.normalize())));
    }
    if held.qty - qty < held.allocated - released {
        return Err(Error::invalid("what is left in that bin is promised to other tasks"));
    }
    sqlx::query("update wms_position set qty = qty - $3, allocated = allocated - $4 where tenant_id = $1 and id = $2").bind(tenant).bind(held.id).bind(qty).bind(released).execute(&mut **tx).await?;
    Ok(held)
}

/// Promises stock to a task.
pub async fn allocate(tx: &mut Tx, tenant: Uuid, id: Uuid, qty: Decimal) -> Result<()> {
    let done = sqlx::query("update wms_position set allocated = allocated + $3 where tenant_id = $1 and id = $2 and allocated + $3 <= qty").bind(tenant).bind(id).bind(qty).execute(&mut **tx).await?;
    if done.rows_affected() == 0 {
        return Err(Error::Conflict("that stock has just been promised to something else".into()));
    }
    Ok(())
}

/// Takes back a promise, for a task that did not happen.
pub async fn release(tx: &mut Tx, tenant: Uuid, k: &Key, qty: Decimal) -> Result<()> {
    if let Some(held) = find(tx, tenant, k).await? {
        sqlx::query("update wms_position set allocated = greatest(allocated - $3, 0) where tenant_id = $1 and id = $2").bind(tenant).bind(held.id).bind(qty).execute(&mut **tx).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn record(tx: &mut Tx, ctx: &Ctx, warehouse: Uuid, kind: Kind, k: &Key, qty: Decimal, from: Side<'_>, to: Side<'_>, cause: &Cause) -> Result<bool> {
    let (entity, source) = cause.source.as_ref().map(|(e, s)| (Some(e.as_str()), Some(*s))).unwrap_or((None, None));
    let done = sqlx::query(
        "insert into wms_move (tenant_id, kind, warehouse_id, item_id, qty, from_bin, to_bin, owner_id, lot, serial, from_unit, to_unit, from_status, to_status, \
         task_id, source_entity, source_id, actor, reason, operation) \
         values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20) \
         on conflict (tenant_id, operation) where operation is not null do nothing",
    )
    .bind(ctx.tenant)
    .bind(kind.as_str())
    .bind(warehouse)
    .bind(k.item)
    .bind(qty)
    .bind(from.bin)
    .bind(to.bin)
    .bind(k.owner)
    .bind(&k.lot)
    .bind(&k.serial)
    .bind(from.unit)
    .bind(to.unit)
    .bind(from.status)
    .bind(to.status)
    .bind(cause.task)
    .bind(entity)
    .bind(source)
    .bind((!ctx.user.is_nil()).then_some(ctx.user))
    .bind(&cause.reason)
    .bind(&cause.operation)
    .execute(&mut **tx)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Stock arrives from outside this warehouse's bins. Returns false when this operation already landed.
pub async fn arrive(tx: &mut Tx, ctx: &Ctx, warehouse: Uuid, kind: Kind, k: &Key, qty: Decimal, expiry: Option<NaiveDate>, cause: &Cause) -> Result<bool> {
    let to = Side { bin: Some(k.bin), unit: k.unit, status: Some(&k.status) };
    if !record(tx, ctx, warehouse, kind, k, qty, Side { bin: None, unit: None, status: None }, to, cause).await? {
        return Ok(false);
    }
    add(tx, ctx.tenant, warehouse, k, qty, expiry, None).await?;
    Ok(true)
}

/// Stock leaves this warehouse's bins.
pub async fn depart(tx: &mut Tx, ctx: &Ctx, warehouse: Uuid, kind: Kind, k: &Key, qty: Decimal, cause: &Cause) -> Result<bool> {
    let from = Side { bin: Some(k.bin), unit: k.unit, status: Some(&k.status) };
    if !record(tx, ctx, warehouse, kind, k, qty, from, Side { bin: None, unit: None, status: None }, cause).await? {
        return Ok(false);
    }
    take(tx, ctx.tenant, k, qty, Decimal::ZERO).await?;
    Ok(true)
}

/// Moves stock between positions: from `from` to `to`, which may differ in bin, unit or status.
/// `released` of it was allocated to this move.
pub async fn transfer(tx: &mut Tx, ctx: &Ctx, warehouse: Uuid, kind: Kind, from: &Key, to: &Key, qty: Decimal, released: Decimal, cause: &Cause) -> Result<bool> {
    if (from.item, from.owner, &from.lot, &from.serial) != (to.item, to.owner, &to.lot, &to.serial) {
        return Err(Error::internal("a transfer does not change what the stock is"));
    }
    let a = Side { bin: Some(from.bin), unit: from.unit, status: Some(&from.status) };
    let b = Side { bin: Some(to.bin), unit: to.unit, status: Some(&to.status) };
    if !record(tx, ctx, warehouse, kind, from, qty, a, b, cause).await? {
        return Ok(false);
    }
    // Locked in one order, so two opposite moves cannot deadlock.
    if from <= to {
        let held = take(tx, ctx.tenant, from, qty, released).await?;
        add(tx, ctx.tenant, warehouse, to, qty, held.expiry, Some(held.received_on)).await?;
    } else {
        let peek = find(tx, ctx.tenant, from).await?.ok_or_else(|| Error::invalid("that stock is not in that bin"))?;
        add(tx, ctx.tenant, warehouse, to, qty, peek.expiry, Some(peek.received_on)).await?;
        take(tx, ctx.tenant, from, qty, released).await?;
    }
    Ok(true)
}

/// Everything in one bin, for scanners and counts.
pub async fn in_bin(tx: &mut Tx, tenant: Uuid, bin: Uuid) -> Result<Vec<Value>> {
    Ok(sqlx::query_scalar(
        "select jsonb_build_object('id', p.id, 'item', p.item_id, 'item_code', i.item_code, 'item_name', i.item_name, 'owner', p.owner_id, 'lot', p.lot, 'serial', p.serial, \
           'unit', p.unit_id, 'status', p.status, 'expiry', p.expiry, 'qty', p.qty, 'allocated', p.allocated) \
         from wms_position p join e_item i on i.tenant_id = p.tenant_id and i.id = p.item_id where p.tenant_id = $1 and p.bin_id = $2 and p.qty > 0 order by i.item_code, p.lot",
    )
    .bind(tenant)
    .bind(bin)
    .fetch_all(&mut **tx)
    .await?)
}
