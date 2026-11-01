//! A quality hold, on the floor. Stock under a hold takes the status `Hold` where it stands, which
//! allocation will not touch, and what a task had already been promised stays promised but cannot
//! finish a pick. Releasing puts back exactly what the hold took.

use crate::position::{self, Cause, Key, Kind};
use async_trait::async_trait;
use ferrum_domain::util::{id, s};
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use uuid::Uuid;

pub struct HoldHook;

#[derive(sqlx::FromRow)]
struct Free {
    warehouse_id: Uuid,
    bin_id: Uuid,
    owner_id: Option<Uuid>,
    lot: String,
    serial: String,
    unit_id: Option<Uuid>,
    free: Decimal,
}

async fn apply(tx: &mut Tx, ctx: &Ctx, hold: Uuid, item: Uuid, lot: &str) -> Result<()> {
    let rows: Vec<Free> = sqlx::query_as(
        "select warehouse_id, bin_id, owner_id, lot, serial, unit_id, qty - allocated as free from wms_position \
         where tenant_id = $1 and item_id = $2 and status = 'Available' and qty > allocated and ($3 = '' or lot = $3) order by id for update",
    )
    .bind(ctx.tenant)
    .bind(item)
    .bind(lot)
    .fetch_all(&mut **tx)
    .await?;
    for r in rows {
        let from = Key { bin: r.bin_id, item, owner: r.owner_id, lot: r.lot, serial: r.serial, unit: r.unit_id, status: "Available".into() };
        let to = Key { status: "Hold".into(), ..from.clone() };
        let cause = Cause { source: Some(("QualityHold".into(), hold)), reason: Some("Quality hold".into()), ..Default::default() };
        position::transfer(tx, ctx, r.warehouse_id, Kind::Status, &from, &to, r.free, Decimal::ZERO, &cause).await?;
    }
    Ok(())
}

#[async_trait]
impl DocHook for HoldHook {
    fn name(&self) -> &'static str {
        "wms.hold"
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        if old.is_some() {
            return Ok(());
        }
        let hold = id(doc, "id").ok_or_else(|| Error::internal("a hold without an id"))?;
        let item = id(doc, "item").ok_or_else(|| Error::internal("a hold without an item"))?;
        apply(hc.tx, hc.ctx, hold, item, s(doc, "lot").unwrap_or_default()).await
    }
}

/// Gives back what a released hold took, position by position. Delivered at least once: each move
/// of the hold is undone once, by its own operation key.
pub async fn released(k: &Kernel, ev: &Event) -> Result<()> {
    let Some(hold) = ev.record_id else { return Ok(()) };
    let ctx = Ctx::system(ev.tenant_id, k.registry.schema(ev.tenant_id).await?);
    let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
    let moves: Vec<(i64, Uuid, Uuid, Uuid, Option<Uuid>, String, String, Option<Uuid>, Decimal)> = sqlx::query_as(
        "select seq, warehouse_id, to_bin, item_id, owner_id, lot, serial, to_unit, qty from wms_move \
         where tenant_id = $1 and source_entity = 'QualityHold' and source_id = $2 and to_status = 'Hold' order by seq",
    )
    .bind(ev.tenant_id)
    .bind(hold)
    .fetch_all(&mut *tx)
    .await?;
    for (seq, warehouse, bin, item, owner, lot, serial, unit, qty) in moves {
        let from = Key { bin, item, owner, lot, serial, unit, status: "Hold".into() };
        let to = Key { status: "Available".into(), ..from.clone() };
        let cause = Cause {
            source: Some(("QualityHold".into(), hold)),
            reason: Some("Hold released".into()),
            operation: Some(format!("release:{hold}:{seq}")),
            ..Default::default()
        };
        // Someone may since have disposed of part of it: what is still there goes back.
        let there: Option<Decimal> = sqlx::query_scalar(
            "select qty - allocated from wms_position where tenant_id = $1 and bin_id = $2 and item_id = $3 and owner_id is not distinct from $4 and lot = $5 and serial = $6 \
             and unit_id is not distinct from $7 and status = 'Hold'",
        )
        .bind(ev.tenant_id)
        .bind(bin)
        .bind(item)
        .bind(owner)
        .bind(&from.lot)
        .bind(&from.serial)
        .bind(unit)
        .fetch_optional(&mut *tx)
        .await?;
        let qty = qty.min(there.unwrap_or_default());
        if qty > Decimal::ZERO {
            position::transfer(&mut tx, &ctx, warehouse, Kind::Status, &from, &to, qty, Decimal::ZERO, &cause).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}
