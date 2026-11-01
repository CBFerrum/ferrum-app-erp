//! What the warehouse says against what the books say, and against the ledger. The database refuses
//! to commit a difference between positions and ERP's balance, so this finding anything means
//! something got around it, which is what a report like this is for.

use ferrum_kernel::{Result, Tx};
use serde_json::{Value, json};
use uuid::Uuid;

/// Every difference in a warehouse: books against positions, and positions against the ledger.
pub async fn differences(tx: &mut Tx, tenant: Uuid, warehouse: Uuid) -> Result<Value> {
    let books: Vec<Value> = sqlx::query_scalar(
        "with places as (select item_id, sum(qty) as qty from wms_position where tenant_id = $1 and warehouse_id = $2 and owner_id is null group by item_id), \
              books as (select item_id, qty from stock_balance where tenant_id = $1 and warehouse_id = $2) \
         select jsonb_build_object('item', i.item_code, 'on_books', coalesce(b.qty, 0), 'in_place', coalesce(p.qty, 0)) \
         from books b full join places p on p.item_id = b.item_id join e_item i on i.tenant_id = $1 and i.id = coalesce(b.item_id, p.item_id) \
         where coalesce(b.qty, 0) <> coalesce(p.qty, 0)",
    )
    .bind(tenant)
    .bind(warehouse)
    .fetch_all(&mut **tx)
    .await?;
    let ledger: Vec<Value> = sqlx::query_scalar(
        "with moved as ( \
           select item_id, bin, lot, serial, owner_id, sum(delta) as qty from ( \
             select item_id, to_bin as bin, lot, serial, owner_id, qty as delta from wms_move where tenant_id = $1 and warehouse_id = $2 and to_bin is not null \
             union all select item_id, from_bin, lot, serial, owner_id, -qty from wms_move where tenant_id = $1 and warehouse_id = $2 and from_bin is not null) m \
           group by item_id, bin, lot, serial, owner_id), \
         held as (select item_id, bin_id as bin, lot, serial, owner_id, sum(qty) as qty from wms_position where tenant_id = $1 and warehouse_id = $2 group by item_id, bin_id, lot, serial, owner_id) \
         select jsonb_build_object('item', coalesce(m.item_id, h.item_id), 'bin', coalesce(m.bin, h.bin), 'ledger', coalesce(m.qty, 0), 'position', coalesce(h.qty, 0)) \
         from moved m full join held h on h.item_id = m.item_id and h.bin = m.bin and h.lot = m.lot and h.serial = m.serial and h.owner_id is not distinct from m.owner_id \
         where coalesce(m.qty, 0) <> coalesce(h.qty, 0)",
    )
    .bind(tenant)
    .bind(warehouse)
    .fetch_all(&mut **tx)
    .await?;
    Ok(json!({"books": books, "ledger": ledger}))
}

/// Where a lot went: every move of it, for a recall.
pub async fn trace(tx: &mut Tx, tenant: Uuid, item: Uuid, lot: &str) -> Result<Vec<Value>> {
    Ok(sqlx::query_scalar("select to_jsonb(m) - 'tenant_id' from wms_move m where tenant_id = $1 and item_id = $2 and lot = $3 order by seq")
        .bind(tenant)
        .bind(item)
        .bind(lot)
        .fetch_all(&mut **tx)
        .await?)
}
