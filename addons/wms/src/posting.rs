//! The contract with ERP. Moves inside a warehouse are positions only. Anything that crosses its
//! wall goes through an ERP document, whose stock moves this mirrors in the same transaction:
//! what ERP receives lands at the inbound bin, what ERP issues must be waiting at the outbound bin,
//! or for a stock entry at the production bin, and a receipt taken back must still be at the inbound
//! bin. So a document posted from anywhere keeps the two in step, or is refused saying why. The
//! database checks the sum at every commit, for anything that gets past this.

use crate::position::{self, Cause, Key, Kind};
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_domain::util::id;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, HookCtx, Result, Tx};
use rust_decimal::Decimal;
use serde_json::Value;
use uuid::Uuid;

/// A warehouse this app runs.
#[derive(sqlx::FromRow, Debug, Clone)]
pub struct Managed {
    pub warehouse: Uuid,
    pub inbound_bin: Option<Uuid>,
    pub outbound_bin: Option<Uuid>,
    pub quarantine_bin: Option<Uuid>,
    pub production_bin: Option<Uuid>,
    pub allocation_rule: Option<String>,
    pub person_station: Option<Uuid>,
    pub status: Option<String>,
}

const MANAGED: &str = "select warehouse, inbound_bin, outbound_bin, quarantine_bin, production_bin, allocation_rule, person_station, status from e_managed_warehouse where tenant_id = $1";

pub async fn managed(tx: &mut Tx, tenant: Uuid, warehouse: Uuid) -> Result<Option<Managed>> {
    Ok(sqlx::query_as::<_, Managed>(sqlx::AssertSqlSafe(format!("{MANAGED} and warehouse = $2 and status = 'Managed'")))
        .bind(tenant)
        .bind(warehouse)
        .fetch_optional(&mut **tx)
        .await?)
}

/// `Managed` or an error a person can act on.
pub async fn must_manage(tx: &mut Tx, tenant: Uuid, warehouse: Uuid) -> Result<Managed> {
    managed(tx, tenant, warehouse).await?.ok_or_else(|| Error::invalid("this warehouse is not managed here yet"))
}

#[derive(sqlx::FromRow)]
struct StockMove {
    id: Uuid,
    item_id: Uuid,
    warehouse_id: Uuid,
    qty: Decimal,
    batch_no: Option<String>,
    serial_no: Option<String>,
    reversal_of: Option<Uuid>,
}

/// The status stock takes on arrival, by its item.
async fn receive_status(tx: &mut Tx, tenant: Uuid, item: Uuid) -> Result<String> {
    let status: Option<Option<String>> =
        sqlx::query_scalar("select wms_receive_status from e_item where tenant_id = $1 and id = $2").bind(tenant).bind(item).fetch_optional(&mut **tx).await?;
    Ok(status.flatten().unwrap_or_else(|| "Available".into()))
}

async fn expiry_of(tx: &mut Tx, tenant: Uuid, item: Uuid, lot: &str) -> Result<Option<NaiveDate>> {
    if lot.is_empty() {
        return Ok(None);
    }
    Ok(sqlx::query_scalar("select expiry_date from e_batch where tenant_id = $1 and item = $2 and batch_id = $3")
        .bind(tenant)
        .bind(item)
        .bind(lot)
        .fetch_optional(&mut **tx)
        .await?
        .flatten())
}

/// Mirrors what ERP just posted for a document. Safe to repeat: each stock move lands once.
pub async fn mirror(tx: &mut Tx, ctx: &Ctx, entity: &str, doc: Uuid) -> Result<()> {
    let moves: Vec<StockMove> = sqlx::query_as(
        "select m.id, m.item_id, m.warehouse_id, m.qty, m.batch_no, m.serial_no, m.reversal_of from stock_move m \
         join e_managed_warehouse w on w.tenant_id = m.tenant_id and w.warehouse = m.warehouse_id and w.status = 'Managed' \
         where m.tenant_id = $1 and m.source_entity = $2 and m.source_id = $3 \
         and not exists (select 1 from wms_move w where w.tenant_id = m.tenant_id and w.operation = 'stock:' || m.id) order by m.seq",
    )
    .bind(ctx.tenant)
    .bind(entity)
    .bind(doc)
    .fetch_all(&mut **tx)
    .await?;
    for m in moves {
        let Some(w) = managed(tx, ctx.tenant, m.warehouse_id).await? else { continue };
        let lot = m.batch_no.clone().unwrap_or_default();
        let serial = m.serial_no.clone().unwrap_or_default();
        let cause = Cause { source: Some((entity.to_string(), doc)), operation: Some(format!("stock:{}", m.id)), ..Default::default() };
        if m.qty > Decimal::ZERO {
            let bin = w.inbound_bin.ok_or_else(|| Error::invalid("this warehouse has no inbound bin"))?;
            let status = receive_status(tx, ctx.tenant, m.item_id).await?;
            let expiry = expiry_of(tx, ctx.tenant, m.item_id, &lot).await?;
            let key = Key { bin, item: m.item_id, owner: None, lot, serial, unit: None, status };
            position::arrive(tx, ctx, m.warehouse_id, Kind::Receive, &key, m.qty, expiry, &cause).await?;
        } else if m.reversal_of.is_some() {
            // A receipt taken back. What came in by the door goes out by it, whatever its status, and
            // if it has been put away the warehouse has to bring it back first.
            let inbound = w.inbound_bin.ok_or_else(|| Error::invalid("this warehouse has no inbound bin"))?;
            issue(tx, ctx, m.warehouse_id, &[inbound], "left at the inbound bin", true, m.item_id, &lot, &serial, -m.qty, &cause).await?;
        } else {
            let outbound = w.outbound_bin.ok_or_else(|| Error::invalid("this warehouse has no outbound bin"))?;
            // Production takes through stock entries, and what it takes waits at the line.
            let line = w.production_bin.filter(|_| entity == "StockEntry");
            let place = if line.is_some() { "brought to the line" } else { "picked to the outbound bin" };
            let bins: Vec<Uuid> = line.into_iter().chain([outbound]).collect();
            issue(tx, ctx, m.warehouse_id, &bins, place, false, m.item_id, &lot, &serial, -m.qty, &cause).await?;
        }
    }
    Ok(())
}

/// Issues from `bins`, in order, across units: what ERP issues is an item, and it may sit on several.
/// Only available stock goes, unless `any_status` says the status does not matter.
#[allow(clippy::too_many_arguments)]
async fn issue(
    tx: &mut Tx,
    ctx: &Ctx,
    warehouse: Uuid,
    bins: &[Uuid],
    place: &str,
    any_status: bool,
    item: Uuid,
    lot: &str,
    serial: &str,
    qty: Decimal,
    cause: &Cause,
) -> Result<()> {
    let rows: Vec<(Uuid, Option<Uuid>, String, Decimal)> = sqlx::query_as(
        "select bin_id, unit_id, status, qty from wms_position where tenant_id = $1 and bin_id = any($2) and item_id = $3 and owner_id is null and lot = $4 and serial = $5 and qty > 0 \
         order by array_position($2, bin_id), unit_id nulls first, status for update",
    )
    .bind(ctx.tenant)
    .bind(bins)
    .bind(item)
    .bind(lot)
    .bind(serial)
    .fetch_all(&mut **tx)
    .await?;
    let usable = |status: &str| any_status || status == "Available";
    let waiting: Decimal = rows.iter().filter(|r| usable(&r.2)).map(|r| r.3).sum();
    if waiting < qty {
        let code: Option<String> =
            sqlx::query_scalar("select item_code from e_item where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(item).fetch_optional(&mut **tx).await?;
        return Err(Error::field("items", format!("{}: only {} of {} has been {place}", code.unwrap_or_default(), waiting.normalize(), qty.normalize())));
    }
    let mut left = qty;
    for (n, (bin, unit, status, have)) in rows.into_iter().filter(|r| usable(&r.2)).enumerate() {
        if left <= Decimal::ZERO {
            break;
        }
        let part = left.min(have);
        let key = Key { bin, item, owner: None, lot: lot.to_string(), serial: serial.to_string(), unit, status };
        let cause = Cause { operation: cause.operation.as_ref().map(|o| if n == 0 { o.clone() } else { format!("{o}:{n}") }), ..cause.clone() };
        position::depart(tx, ctx, warehouse, Kind::Issue, &key, part, &cause).await?;
        left -= part;
    }
    Ok(())
}

/// Reverses the mirror of a cancelled document, from ERP's reversal moves. What was received can be
/// taken back only if it is still at the inbound bin.
pub struct MirrorHook(pub &'static str);

#[async_trait]
impl DocHook for MirrorHook {
    fn name(&self) -> &'static str {
        "wms.mirror"
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if let Some(id) = id(doc, "id") {
            mirror(hc.tx, hc.ctx, self.0, id).await?;
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if let Some(id) = id(doc, "id") {
            mirror(hc.tx, hc.ctx, self.0, id).await?;
        }
        Ok(())
    }
}

/// The documents that move ERP's stock.
pub const STOCK_DOCUMENTS: [&str; 6] = ["PurchaseReceipt", "PurchaseInvoice", "DeliveryNote", "SalesInvoice", "StockEntry", "StockReconciliation"];

/// Places what ERP already holds into the inbound bin, when a warehouse becomes managed. The bins
/// must be walked and counted after this: until then this is where the books say it is.
pub async fn start_managing(tx: &mut Tx, ctx: &Ctx, record: Uuid, warehouse: Uuid, inbound: Uuid) -> Result<usize> {
    let rows: Vec<(Uuid, Option<String>, Option<String>, Decimal)> = sqlx::query_as(
        "select item_id, nullif(batch_no, ''), nullif(serial_no, ''), sum(qty) from stock_move where tenant_id = $1 and warehouse_id = $2 \
         group by item_id, nullif(batch_no, ''), nullif(serial_no, '') having sum(qty) > 0",
    )
    .bind(ctx.tenant)
    .bind(warehouse)
    .fetch_all(&mut **tx)
    .await?;
    let count = rows.len();
    for (item, lot, serial, qty) in rows {
        let lot = lot.unwrap_or_default();
        let expiry = expiry_of(tx, ctx.tenant, item, &lot).await?;
        let key = Key { bin: inbound, item, owner: None, lot, serial: serial.unwrap_or_default(), unit: None, status: "Available".into() };
        position::arrive(tx, ctx, warehouse, Kind::Opening, &key, qty, expiry, &Cause::why("what ERP held when the warehouse became managed")).await?;
    }
    sqlx::query("update e_managed_warehouse set status = 'Managed' where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(record).execute(&mut **tx).await?;
    Ok(count)
}

pub fn text(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or_default().to_string()
}
