//! Lot genealogy. Forward from a material to everything made with it, for a recall; back from a
//! product to everything in it, for an investigation. Rework feeds output back in as input, so the
//! walk carries the path it took and stops at anything already on it.

use ferrum_kernel::{Result, Tx};
use serde_json::Value;
use uuid::Uuid;

const FORWARD: &str = "with recursive walk as ( \
      select g.input_item, g.input_lot, g.output_item, g.output_lot, g.input_qty, g.run_id, 1 as depth, array[g.output_item::text || '/' || g.output_lot] as path \
      from mes_genealogy g where g.tenant_id = $1 and g.input_item = $2 and g.input_lot = $3 \
      union all \
      select g.input_item, g.input_lot, g.output_item, g.output_lot, g.input_qty, g.run_id, w.depth + 1, w.path || (g.output_item::text || '/' || g.output_lot) \
      from mes_genealogy g join walk w on g.input_item = w.output_item and g.input_lot = w.output_lot \
      where g.tenant_id = $1 and w.output_lot <> '' and w.depth < 50 and not (g.output_item::text || '/' || g.output_lot) = any(w.path)) \
    select jsonb_build_object('item', i.item_code, 'lot', w.input_lot, 'into_item', o.item_code, 'into_lot', w.output_lot, 'qty', w.input_qty, 'run', w.run_id, 'depth', w.depth, 'scrapped', w.output_lot = '') \
    from walk w join e_item i on i.tenant_id = $1 and i.id = w.input_item join e_item o on o.tenant_id = $1 and o.id = w.output_item order by w.depth";

const BACK: &str = "with recursive walk as ( \
      select g.input_item, g.input_lot, g.output_item, g.output_lot, g.input_qty, g.run_id, 1 as depth, array[g.input_item::text || '/' || g.input_lot] as path \
      from mes_genealogy g where g.tenant_id = $1 and g.output_item = $2 and g.output_lot = $3 \
      union all \
      select g.input_item, g.input_lot, g.output_item, g.output_lot, g.input_qty, g.run_id, w.depth + 1, w.path || (g.input_item::text || '/' || g.input_lot) \
      from mes_genealogy g join walk w on g.output_item = w.input_item and g.output_lot = w.input_lot \
      where g.tenant_id = $1 and w.input_lot <> '' and w.depth < 50 and not (g.input_item::text || '/' || g.input_lot) = any(w.path)) \
    select jsonb_build_object('item', i.item_code, 'lot', w.input_lot, 'into_item', o.item_code, 'into_lot', w.output_lot, 'qty', w.input_qty, 'run', w.run_id, 'depth', w.depth, 'scrapped', w.output_lot = '') \
    from walk w join e_item i on i.tenant_id = $1 and i.id = w.input_item join e_item o on o.tenant_id = $1 and o.id = w.output_item order by w.depth";

pub async fn follow(tx: &mut Tx, tenant: Uuid, item: Uuid, lot: &str, forward: bool) -> Result<Vec<Value>> {
    Ok(sqlx::query_scalar(if forward { FORWARD } else { BACK }).bind(tenant).bind(item).bind(lot).fetch_all(&mut **tx).await?)
}
