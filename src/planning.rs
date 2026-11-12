//! Planning that says what to do (OP-39): the netting both planners use (the requirements run here and the production
//! plan in Manufacturing Operations), lot sizing by an item's minimum, multiple and maximum per warehouse, and the
//! time-bucketed plan of one item in one warehouse with action messages on the orders already there. Pure: the callers
//! read the demand, the supply and the parameters.

use chrono::{Datelike, Duration, NaiveDate};
use rust_decimal::Decimal;

/// An item's planning parameters in a warehouse.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Params {
    pub safety: Decimal,
    pub min: Decimal,
    pub multiple: Decimal,
    /// Zero: no most.
    pub max: Decimal,
    pub lead_days: i64,
}

/// The orders that cover `need`: at least the minimum, rounded up to the multiple, split at the maximum (each part a
/// multiple, the last at least the minimum). Nothing for no need.
pub fn lot(need: Decimal, p: &Params) -> Vec<Decimal> {
    if need <= Decimal::ZERO {
        return vec![];
    }
    let up = |q: Decimal| if p.multiple > Decimal::ZERO { (q / p.multiple).ceil() * p.multiple } else { q };
    let total = up(need.max(p.min));
    let cap = if p.max > Decimal::ZERO && p.multiple > Decimal::ZERO { (p.max / p.multiple).floor() * p.multiple } else { p.max };
    if cap <= Decimal::ZERO || total <= cap {
        return vec![total.round_dp(9)];
    }
    let mut out = Vec::new();
    let mut left = total;
    while left > cap {
        out.push(cap.round_dp(9));
        left -= cap;
    }
    out.push(up(left.max(p.min)).min(cap).round_dp(9));
    out
}

/// The netting both planners use: what is short of gross need plus safety stock after what is there and what is
/// coming, and the orders that cover it.
pub fn net(gross: Decimal, available: Decimal, incoming: Decimal, p: &Params) -> (Decimal, Vec<Decimal>) {
    let short = gross + p.safety - available.max(Decimal::ZERO) - incoming;
    (short, lot(short, p))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Day,
    Week,
    Month,
}

impl Bucket {
    pub fn parse(s: &str) -> Option<Bucket> {
        match s {
            "Day" => Some(Bucket::Day),
            "Week" => Some(Bucket::Week),
            "Month" => Some(Bucket::Month),
            _ => None,
        }
    }

    /// The first day of the bucket a day falls in (weeks start on Monday).
    pub fn start(self, d: NaiveDate) -> NaiveDate {
        match self {
            Bucket::Day => d,
            Bucket::Week => d - Duration::days(d.weekday().num_days_from_monday() as i64),
            Bucket::Month => d.with_day(1).unwrap_or(d),
        }
    }

    fn next(self, d: NaiveDate) -> NaiveDate {
        match self {
            Bucket::Day => d + Duration::days(1),
            Bucket::Week => d + Duration::days(7),
            Bucket::Month => d.checked_add_months(chrono::Months::new(1)).unwrap_or(d),
        }
    }

    /// Bucket starts from `today`'s to `until`'s.
    pub fn starts(self, today: NaiveDate, until: NaiveDate) -> Vec<NaiveDate> {
        let mut out = vec![self.start(today)];
        while let Some(last) = out.last().copied() {
            let n = self.next(last);
            if n > until || out.len() >= 1000 {
                break;
            }
            out.push(n);
        }
        out
    }
}

/// An order already there: its reference, the day it brings its goods and how many.
#[derive(Debug, Clone, PartialEq)]
pub struct Supply {
    pub key: String,
    pub date: NaiveDate,
    pub qty: Decimal,
}

/// What to do with an order already there.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub key: String,
    /// Bring Forward, Push Out, Cancel, Increase, Decrease.
    pub kind: &'static str,
    pub date: NaiveDate,
    pub qty: Decimal,
}

/// One item's plan in one warehouse.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Plan {
    /// New orders: the bucket they are due in, the day to release them (lead time before) and how many.
    pub orders: Vec<(NaiveDate, NaiveDate, Decimal)>,
    pub messages: Vec<Message>,
    /// What is short over the horizon before lot sizing (for transfers between warehouses).
    pub short: Decimal,
    /// What is on hand beyond the horizon's demand and safety stock (a surplus to transfer: never goods still coming).
    pub surplus: Decimal,
}

/// Plans an item in a warehouse by bucket: what must be there by each bucket (demand to date plus safety stock less
/// what is on hand) is covered first by the orders already there, earliest first. An order needed in an earlier bucket
/// than it comes is to be brought forward, one not needed until a later bucket pushed out, one not needed in the
/// horizon cancelled, one needed only in part decreased; what is still short is new orders, lot-sized, except that a
/// shortfall in the bucket of the last order there increases that order.
pub fn plan_item(on_hand: Decimal, demand: &[(NaiveDate, Decimal)], supplies: &[Supply], p: &Params, b: Bucket, today: NaiveDate, until: NaiveDate) -> Plan {
    let starts = b.starts(today, until);
    let idx = |d: NaiveDate| starts.iter().rposition(|s| *s <= b.start(d)).unwrap_or(0);
    let mut per = vec![Decimal::ZERO; starts.len()];
    for (d, q) in demand.iter().filter(|(d, _)| *d <= until) {
        per[idx(*d)] += *q;
    }
    // What must have come by each bucket.
    let mut required = Vec::with_capacity(starts.len());
    let mut cum = Decimal::ZERO;
    for q in &per {
        cum += *q;
        let r = (cum + p.safety - on_hand.max(Decimal::ZERO)).max(Decimal::ZERO);
        required.push(required.last().copied().unwrap_or(Decimal::ZERO).max(r));
    }
    let need_at = |covered: Decimal| required.iter().position(|r| *r > covered);
    let mut plan = Plan::default();
    let mut sorted: Vec<&Supply> = supplies.iter().filter(|s| s.date <= until).collect();
    sorted.sort_by(|a, b| a.date.cmp(&b.date).then(a.key.cmp(&b.key)));
    let total_required = required.last().copied().unwrap_or_default();
    let mut covered = Decimal::ZERO;
    let mut last_kept: Option<(usize, String)> = None;
    for s in sorted {
        let at = idx(s.date);
        match need_at(covered) {
            None => plan.messages.push(Message { key: s.key.clone(), kind: "Cancel", date: s.date, qty: Decimal::ZERO }),
            Some(t) => {
                let useful = (total_required - covered).min(s.qty);
                if t < at {
                    plan.messages.push(Message { key: s.key.clone(), kind: "Bring Forward", date: starts[t], qty: s.qty });
                } else if t > at {
                    plan.messages.push(Message { key: s.key.clone(), kind: "Push Out", date: starts[t], qty: s.qty });
                }
                if useful < s.qty {
                    plan.messages.push(Message { key: s.key.clone(), kind: "Decrease", date: starts[t.max(at)], qty: useful });
                }
                covered += useful;
                last_kept = Some((t.max(at), s.key.clone()));
            }
        }
    }
    plan.short = (total_required - covered).max(Decimal::ZERO);
    plan.surplus = (on_hand.max(Decimal::ZERO) - per.iter().copied().sum::<Decimal>() - p.safety).max(Decimal::ZERO);
    let mut planned = Decimal::ZERO;
    for (t, r) in required.iter().enumerate() {
        let short = *r - covered - planned;
        if short <= Decimal::ZERO {
            continue;
        }
        if let Some((at, key)) = &last_kept
            && *at == t
            && planned.is_zero()
        {
            let q = short;
            plan.messages.push(Message { key: key.clone(), kind: "Increase", date: starts[t], qty: q });
            planned += q;
            continue;
        }
        for q in lot(short, p) {
            let release = (starts[t] - Duration::days(p.lead_days.max(0))).max(today);
            plan.orders.push((starts[t], release, q));
            planned += q;
        }
    }
    plan
}

/// An item's planning parameters in a warehouse, as both planners read them: its Planning by Warehouse row for the
/// warehouse, else its row without one, else its own safety stock, minimum order and lead time.
pub async fn params(tx: &mut ferrum_kernel::Tx, ctx: &ferrum_kernel::Ctx, item: uuid::Uuid, warehouse: Option<uuid::Uuid>) -> ferrum_kernel::Result<Params> {
    let base: Option<(Option<Decimal>, Option<Decimal>, Option<i64>)> =
        sqlx::query_as("select safety_stock, min_order_qty, lead_time_days::int8 from e_item where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(item)
            .fetch_optional(&mut **tx)
            .await?;
    let (safety, min, lead) = base.unwrap_or_default();
    let mut p = Params { safety: safety.unwrap_or_default(), min: min.unwrap_or_default(), lead_days: lead.unwrap_or(0), ..Default::default() };
    let Some(table) = ctx.schema.entity("Item").and_then(|e| e.children.get("mfg_planning").map(|c| c.table.clone())) else { return Ok(p) };
    let row: Option<(Option<Decimal>, Option<Decimal>, Option<Decimal>, Option<Decimal>, Option<i64>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select safety_stock, min_order_qty, order_multiple, max_order_qty, lead_time_days::int8 from {} where tenant_id = $1 and parent_id = $2 \
         and (warehouse = $3 or warehouse is null) order by warehouse is null, idx limit 1",
        ferrum_kernel::ddl::q(&table)
    )))
    .bind(ctx.tenant)
    .bind(item)
    .bind(warehouse)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((s, mn, mul, mx, ld)) = row {
        p.safety = s.unwrap_or(p.safety);
        p.min = mn.unwrap_or(p.min);
        p.multiple = mul.unwrap_or_default();
        p.max = mx.unwrap_or_default();
        p.lead_days = ld.unwrap_or(p.lead_days);
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> Decimal {
        s.parse().unwrap()
    }
    fn day(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    #[test]
    fn lots_respect_minimum_multiple_and_maximum() {
        let p = Params { min: n("10"), multiple: n("6"), max: n("30"), ..Default::default() };
        assert_eq!(lot(n("3"), &p), vec![n("12")], "the minimum, rounded up to the multiple");
        assert_eq!(lot(n("31"), &p), vec![n("30"), n("12")], "split at the maximum");
        assert_eq!(lot(Decimal::ZERO, &p), Vec::<Decimal>::new());
        assert_eq!(net(n("20"), n("5"), n("3"), &Params { safety: n("2"), ..Default::default() }), (n("14"), vec![n("14")]));
    }

    #[test]
    fn buckets_net_and_say_what_to_do_with_orders() {
        let today = day("2026-11-02"); // a Monday
        let p = Params { safety: n("5"), min: n("0"), multiple: n("10"), max: n("0"), lead_days: 7 };
        let demand = [(day("2026-11-03"), n("20")), (day("2026-11-18"), n("30")), (day("2026-12-20"), n("1"))];
        // On hand 15: by week 1 need 10 (20 + 5 - 15), by week 3 need 40.
        let supplies = [
            Supply { key: "late".into(), date: day("2026-11-25"), qty: n("10") }, // needed in week 1: bring forward
            Supply { key: "early".into(), date: day("2026-11-10"), qty: n("40") }, // covers week 3 partly, too early? needed week 3
        ];
        let plan = plan_item(n("15"), &demand, &supplies, &p, Bucket::Week, today, day("2026-12-06"));
        let kinds: Vec<(&str, &str)> = plan.messages.iter().map(|m| (m.key.as_str(), m.kind)).collect();
        // Earliest first: "early" (10 Nov) covers the 10 needed in week 1 and 30 of week 3: bring it forward.
        assert!(kinds.contains(&("early", "Bring Forward")), "{kinds:?}");
        // "late" is not needed in the horizon (the December demand is beyond it).
        assert!(kinds.contains(&("late", "Cancel")), "{kinds:?}");
        assert!(plan.orders.is_empty());
        assert_eq!(plan.surplus, n("0"));
        let spare = plan_item(n("60"), &demand, &[], &p, Bucket::Week, today, day("2026-12-06"));
        assert_eq!((spare.surplus, spare.short), (n("5"), n("0")));

        // Nothing on order: new orders by bucket, lot-sized, released a lead time before.
        let plan = plan_item(n("15"), &demand, &[], &p, Bucket::Week, today, day("2026-12-06"));
        assert_eq!(plan.orders, vec![(day("2026-11-02"), day("2026-11-02"), n("10")), (day("2026-11-16"), day("2026-11-09"), n("30"))]);
        assert_eq!(plan.short, n("40"));

        // An order in the bucket of the shortfall is increased rather than a new one proposed; one more than needed decreased.
        let one = [Supply { key: "po".into(), date: day("2026-11-17"), qty: n("25") }];
        let plan = plan_item(n("25"), &demand, &one, &p, Bucket::Week, today, day("2026-12-06"));
        assert_eq!(plan.messages, vec![Message { key: "po".into(), kind: "Increase", date: day("2026-11-16"), qty: n("5") }]);
        let big = [Supply { key: "po".into(), date: day("2026-11-17"), qty: n("60") }];
        let plan = plan_item(n("25"), &demand, &big, &p, Bucket::Week, today, day("2026-12-06"));
        assert_eq!(plan.messages, vec![Message { key: "po".into(), kind: "Decrease", date: day("2026-11-16"), qty: n("30") }]);
        // Months.
        assert_eq!(Bucket::Month.starts(today, day("2027-01-15")), vec![day("2026-11-01"), day("2026-12-01"), day("2027-01-01")]);
    }
}
