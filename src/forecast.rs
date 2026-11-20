//! Demand forecasts (docs/forecast.md): each item's expected monthly demand in a warehouse, worked out from what left
//! it on deliveries and stock-updating invoices by one of three classical methods, in exact decimals. A requirements
//! run that names a forecast plans for what is left of each month's figure after that month's open orders.

use crate::util::{d, id, money, rows, s, sys};
use chrono::{Datelike, Months, NaiveDate};
use ferrum_kernel::{Doc, Error, HookCtx, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

/// The most items one forecast works out: past it, the planner names an item group.
const MAX_ITEMS: usize = 2000;

fn mean(xs: &[Decimal]) -> Decimal {
    if xs.is_empty() { Decimal::ZERO } else { xs.iter().copied().sum::<Decimal>() / Decimal::from(xs.len()) }
}

/// The mean of the last three months (fewer when there are fewer), for each month ahead.
pub fn moving_average(h: &[Decimal], ahead: usize) -> Vec<Decimal> {
    vec![mean(&h[h.len().saturating_sub(3)..]); ahead]
}

/// Simple exponential smoothing: the level after the last month, for each month ahead.
pub fn exponential(h: &[Decimal], alpha: Decimal, ahead: usize) -> Vec<Decimal> {
    let level = h.iter().skip(1).fold(h.first().copied().unwrap_or_default(), |l, x| alpha * x + (Decimal::ONE - alpha) * l);
    vec![level; ahead]
}

/// Each calendar month's share of the overall mean, applied to the last twelve months' mean. `first` is the calendar
/// month (1 to 12) of `h[0]` and `next` that of the first month ahead. Needs 24 months.
pub fn seasonal(h: &[Decimal], first: u32, next: u32, ahead: usize) -> Result<Vec<Decimal>> {
    if h.len() < 24 {
        return Err(Error::field("history_months", "a seasonal forecast needs at least 24 months of history"));
    }
    let overall = mean(h);
    if overall.is_zero() {
        return Ok(vec![Decimal::ZERO; ahead]);
    }
    let month_of = |i: usize| ((first as usize - 1 + i) % 12) as u32 + 1;
    let factor = |m: u32| mean(&h.iter().enumerate().filter(|(i, _)| month_of(*i) == m).map(|(_, x)| *x).collect::<Vec<_>>()) / overall;
    let level = mean(&h[h.len() - 12..]);
    Ok((0..ahead).map(|i| level * factor(((next as usize - 1 + i) % 12) as u32 + 1)).collect())
}

/// What a month's forecast adds to what its orders already ask for.
pub fn net_forecast(planned: Decimal, ordered: Decimal) -> Decimal {
    (planned - ordered).max(Decimal::ZERO)
}

fn month_start(d: NaiveDate) -> NaiveDate {
    d.with_day(1).unwrap_or(d)
}

/// What left `warehouse` of `company` on deliveries and stock-updating invoices in each month from `from` to `to`
/// (first days), in stock units: item -> month -> quantity. Reversals and returns are positive rows and net out.
async fn history(
    hc: &mut HookCtx<'_>,
    company: Uuid,
    warehouse: Uuid,
    from: NaiveDate,
    to: NaiveDate,
    group: Option<Uuid>,
) -> Result<HashMap<Uuid, BTreeMap<NaiveDate, Decimal>>> {
    let rows: Vec<(Uuid, NaiveDate, Decimal)> = sqlx::query_as(
        "select m.item_id, date_trunc('month', m.posting_date)::date, greatest(sum(-m.qty), 0) from stock_move m \
         join e_item i on i.tenant_id = m.tenant_id and i.id = m.item_id \
         where m.tenant_id = $1 and m.company_id = $2 and m.warehouse_id = $3 and m.source_entity in ('DeliveryNote', 'SalesInvoice') \
         and m.posting_date >= $4 and m.posting_date < $5 \
         and ($6::uuid is null or i.item_group in (with recursive g as (select id from e_item_group where tenant_id = $1 and id = $6 \
              union select c.id from e_item_group c join g on c.parent_group = g.id where c.tenant_id = $1) select id from g)) \
         group by 1, 2",
    )
    .bind(hc.ctx.tenant)
    .bind(company)
    .bind(warehouse)
    .bind(from)
    .bind(to)
    .bind(group)
    .fetch_all(&mut **hc.tx)
    .await?;
    let mut out: HashMap<Uuid, BTreeMap<NaiveDate, Decimal>> = HashMap::new();
    for (item, month, qty) in rows {
        out.entry(item).or_default().insert(month, qty);
    }
    Ok(out)
}

/// `compute`: replaces the forecast's lines from the stock ledger as it is now, keeping each figure a person changed.
pub async fn compute(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
    let fid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let company = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
    let warehouse = id(doc, "warehouse").ok_or_else(|| Error::field("warehouse", "is required"))?;
    let months = d(doc, "history_months").to_string().parse::<u32>().unwrap_or(12).clamp(3, 60);
    let ahead = d(doc, "periods").to_string().parse::<usize>().unwrap_or(3).clamp(1, 24);
    let method = s(doc, "method").unwrap_or("Moving Average").to_string();
    let alpha = Some(d(doc, "smoothing")).filter(|a| !a.is_zero()).unwrap_or(Decimal::new(3, 1));
    let current = month_start(hc.ctx.today());
    let from = current - Months::new(months);
    let past: Vec<NaiveDate> = (0..months).map(|i| from + Months::new(i)).collect();
    let next: Vec<NaiveDate> = (0..ahead as u32).map(|i| current + Months::new(i)).collect();
    let sold = history(hc, company, warehouse, from, current, id(doc, "item_group")).await?;
    if sold.len() > MAX_ITEMS {
        return Err(Error::field(
            "item_group",
            format!("{} items were sold from this warehouse: forecast an item group at a time (at most {MAX_ITEMS})", sold.len()),
        ));
    }
    // A figure a person changed: (item, month) -> theirs.
    let kept: HashMap<(Option<Uuid>, Option<String>), Value> = rows(doc, "lines")
        .iter()
        .filter(|l| d(l, "planned_qty") != d(l, "forecast_qty"))
        .map(|l| ((id(l, "item"), s(l, "period_start").map(str::to_string)), l.get("planned_qty").cloned().unwrap_or(Value::Null)))
        .collect();
    let mut items: Vec<&Uuid> = sold.keys().collect();
    items.sort();
    let mut lines = Vec::new();
    for item in items {
        let h: Vec<Decimal> = past.iter().map(|m| sold[item].get(m).copied().unwrap_or_default()).collect();
        let figures = match method.as_str() {
            "Exponential Smoothing" => exponential(&h, alpha, ahead),
            "Seasonal" => seasonal(&h, from.month(), current.month(), ahead)?,
            _ => moving_average(&h, ahead),
        };
        let avg = mean(&h).round_dp(3);
        for (month, f) in next.iter().zip(figures) {
            let f = f.max(Decimal::ZERO).round_dp(3);
            let key = (Some(*item), Some(month.to_string()));
            let planned = kept.get(&key).cloned().unwrap_or_else(|| money(f));
            lines.push(json!({"item": item.to_string(), "period_start": month.to_string(), "history_avg": money(avg), "forecast_qty": money(f), "planned_qty": planned}));
        }
    }
    let mut v = Doc::new();
    let count = sold.len();
    let n = lines.len();
    v.insert("lines".into(), Value::Array(lines));
    v.insert("status".into(), json!("Computed"));
    hc.kernel.update_tx(&crate::util::elevated(hc.ctx), hc.tx, "DemandForecast", fid, v, None).await?;
    Ok(json!({"items": count, "lines": n, "from": next[0].to_string(), "to": next[ahead - 1].to_string()}))
}

/// A run's forecast, checked against the run: (item, month) -> planned quantity, within `until` when given.
pub async fn planned(hc: &mut HookCtx<'_>, run: &Doc, warehouse: Option<Uuid>) -> Result<Option<(Uuid, BTreeMap<(Uuid, NaiveDate), Decimal>)>> {
    let Some(fid) = id(run, "forecast") else { return Ok(None) };
    let f = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "DemandForecast", fid, false).await?;
    let number = s(&f, "number").unwrap_or_default().to_string();
    if id(&f, "company") != id(run, "company") || s(&f, "status") != Some("Computed") {
        return Err(Error::field("forecast", format!("{number} is not a worked-out forecast of this company")));
    }
    if warehouse.is_some() && id(&f, "warehouse") != warehouse {
        return Err(Error::field("forecast", format!("{number} is another warehouse's forecast than this run's")));
    }
    let mut out = BTreeMap::new();
    for l in rows(&f, "lines") {
        let (Some(item), Some(month)) = (id(&l, "item"), s(&l, "period_start").and_then(|m| m.parse::<NaiveDate>().ok())) else { continue };
        *out.entry((item, month)).or_insert(Decimal::ZERO) += d(&l, "planned_qty");
    }
    Ok(Some((id(&f, "warehouse").unwrap_or_default(), out)))
}

pub struct ForecastHook;

#[async_trait::async_trait]
impl ferrum_kernel::DocHook for ForecastHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !doc.contains_key("status") || doc.get("status").is_some_and(Value::is_null) {
            doc.insert("status".into(), json!("Draft"));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name == "compute" { compute(hc, doc).await.map(Some) } else { Ok(None) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: &[i64]) -> Vec<Decimal> {
        v.iter().map(|x| Decimal::from(*x)).collect()
    }

    #[test]
    fn a_moving_average_is_the_mean_of_the_last_three() {
        assert_eq!(moving_average(&n(&[100, 10, 20, 30]), 2), n(&[20, 20]));
        assert_eq!(moving_average(&n(&[7]), 1), n(&[7]), "one month of history is that month");
    }

    #[test]
    fn smoothing_weighs_recent_months() {
        // L = 10; 0.3·20 + 0.7·10 = 13; 0.3·40 + 0.7·13 = 21.1
        assert_eq!(exponential(&n(&[10, 20, 40]), Decimal::new(3, 1), 1), vec![Decimal::new(211, 1)]);
        // L = 10; 15; 27.5
        assert_eq!(exponential(&n(&[10, 20, 40]), Decimal::new(5, 1), 2), vec![Decimal::new(275, 1); 2]);
    }

    #[test]
    fn a_seasonal_forecast_follows_last_years_shape() {
        // Two years from January: 10 a month, December 30 each year. Mean 140/12; December's factor 30 / (140/12).
        let mut h = n(&[10; 24]);
        h[11] = Decimal::from(30);
        h[23] = Decimal::from(30);
        let f = seasonal(&h, 1, 12, 2).unwrap();
        assert_eq!((f[0].round_dp(6), f[1].round_dp(6)), (Decimal::from(30), Decimal::from(10)), "December three times, January as usual");
        assert_eq!(seasonal(&n(&[5; 24]), 3, 7, 3).unwrap(), n(&[5, 5, 5]), "flat history, flat forecast");
        assert_eq!(seasonal(&n(&[0; 24]), 1, 1, 2).unwrap(), n(&[0, 0]));
        assert!(seasonal(&n(&[5; 23]), 1, 1, 1).is_err(), "23 months are not enough");
    }

    #[test]
    fn orders_use_up_the_forecast() {
        assert_eq!(net_forecast(Decimal::from(10), Decimal::from(4)), Decimal::from(6));
        assert_eq!(net_forecast(Decimal::from(10), Decimal::from(10)), Decimal::ZERO);
        assert_eq!(net_forecast(Decimal::from(10), Decimal::from(12)), Decimal::ZERO);
    }
}
