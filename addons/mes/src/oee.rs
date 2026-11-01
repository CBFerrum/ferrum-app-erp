//! OEE from what was recorded: equipment states for time, reported quantities for output, and the
//! approved revision's ideal cycle. Its definition and its treatment of missing data are the
//! contract's (`ferrum_ops_contract::oee`), so figures can be explained and recomputed.

use chrono::{DateTime, Utc};
use ferrum_kernel::{Error, Result, Tx};
use ferrum_ops::contract::oee::{self, Input, StateAt};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde_json::{Value, json};
use uuid::Uuid;

pub async fn for_workstation(tx: &mut Tx, tenant: Uuid, workstation: Uuid, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<Value> {
    if to <= from {
        return Err(Error::invalid("the period ends before it starts"));
    }
    let device: Option<Option<Uuid>> = sqlx::query_scalar("select mes_device from e_workstation where tenant_id = $1 and id = $2").bind(tenant).bind(workstation).fetch_optional(&mut **tx).await?;
    let device = device.flatten().ok_or_else(|| Error::invalid("this workstation has no equipment to measure"))?;
    let events: Vec<(DateTime<Utc>, String, Option<String>)> = sqlx::query_as("select at, state, reason from op_equipment_event where tenant_id = $1 and device_id = $2 and at < $3 and at >= $4 order by at").bind(tenant).bind(device).bind(to).bind(from).fetch_all(&mut **tx).await?;
    let before: Option<String> = sqlx::query_scalar("select state from op_equipment_event where tenant_id = $1 and device_id = $2 and at < $3 order by at desc limit 1").bind(tenant).bind(device).bind(from).fetch_optional(&mut **tx).await?;
    let planned_stops: Vec<String> = sqlx::query_scalar("select reason from e_downtime_reason where tenant_id = $1 and planned").bind(tenant).fetch_all(&mut **tx).await?;
    let states: Vec<StateAt> = events.iter().map(|(at, state, reason)| StateAt { at: *at, state: if reason.as_ref().is_some_and(|r| planned_stops.contains(r)) { "Planned".into() } else { state.clone() } }).collect();
    let t = oee::time_in_states(before.as_deref(), &states, from, to);
    let (good, scrap, weighted): (Option<Decimal>, Option<Decimal>, Option<Decimal>) = sqlx::query_as(
        "select sum(e.qty), sum(e.scrap), sum((coalesce(e.qty, 0) + coalesce(e.scrap, 0)) * coalesce(v.ideal_cycle_secs, 0)) from mes_event e \
         join e_production_run r on r.tenant_id = e.tenant_id and r.id = e.run_id join e_production_revision v on v.tenant_id = r.tenant_id and v.id = r.revision \
         where e.tenant_id = $1 and r.workstation = $2 and e.kind = 'Report' and e.at >= $3 and e.at < $4",
    )
    .bind(tenant)
    .bind(workstation)
    .bind(from)
    .bind(to)
    .fetch_one(&mut **tx)
    .await?;
    let (good, scrap) = (good.unwrap_or_default(), scrap.unwrap_or_default());
    let total = good + scrap;
    let ideal = if total.is_zero() { Decimal::ZERO } else { weighted.unwrap_or_default() / total };
    let input = Input {
        planned_secs: (to - from).num_seconds() as f64 - t.planned_stop_secs,
        run_secs: t.run_secs,
        unknown_secs: t.unknown_secs,
        ideal_cycle_secs: ideal.to_f64().unwrap_or(0.0),
        total_count: total.to_f64().unwrap_or(0.0),
        good_count: good.to_f64().unwrap_or(0.0),
    };
    Ok(json!({"input": input, "figures": oee::figures(&input)}))
}
