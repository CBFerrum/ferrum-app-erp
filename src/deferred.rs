//! Revenue and expense over a service period (Ferrum's FERRUM-ERP-006), as ERPNext defers them
//! (its design in erpnext/accounts/deferred_revenue.py; none of its code). An invoice line marked
//! deferred books to a deferred revenue account (a liability) or a deferred expense account (an
//! asset) instead of its income or expense account. Recognition moves what each day of the
//! service period earned or used to the line's own account, one entry per invoice and run, posted
//! under the invoice: cancelling the invoice reverses what was recognised with the rest. A credit
//! note's deferred lines are recognised the same way, negative, over the days they credit: a refund
//! takes from what is earned from then on, not from what was earned before. Days are counted
//! inclusive of both ends, and the last day takes what rounding left.

use crate::ledger::{self, Entry, Line};
use crate::util::{Company, b, company, d, id, round, rows, s};
use chrono::NaiveDate;
use ferrum_kernel::ddl::q;
use ferrum_kernel::value::parse_date;
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

/// The invoices whose lines may be deferred, with each side's fields.
struct Kind {
    invoice: &'static str,
    flag: &'static str,
    account: &'static str,
    company_default: &'static str,
    what: &'static str,
    own_account: &'static str,
    selling: bool,
}

const KINDS: [Kind; 2] = [
    Kind {
        invoice: "SalesInvoice",
        flag: "enable_deferred_revenue",
        account: "deferred_revenue_account",
        company_default: "default_deferred_revenue_account",
        what: "deferred revenue account",
        own_account: "income_account",
        selling: true,
    },
    Kind {
        invoice: "PurchaseInvoice",
        flag: "enable_deferred_expense",
        account: "deferred_expense_account",
        company_default: "default_deferred_expense_account",
        what: "deferred expense account",
        own_account: "expense_account",
        selling: false,
    },
];

fn kind(selling: bool) -> &'static Kind {
    &KINDS[if selling { 0 } else { 1 }]
}

fn day(row: &Doc, field: &str) -> Option<NaiveDate> {
    s(row, field).and_then(|v| parse_date(v).ok())
}

/// A deferred line's service period, checked; `None` when the line is not deferred.
pub(crate) fn period(row: &Doc, selling: bool, line: usize) -> Result<Option<(NaiveDate, NaiveDate)>> {
    if !b(row, kind(selling).flag) {
        return Ok(None);
    }
    let at = |field: &str, msg: &str| Error::field(format!("items.{line}.{field}"), msg.to_string());
    let start = day(row, "service_start_date").ok_or_else(|| at("service_start_date", "a deferred line needs the day its service starts"))?;
    let end = day(row, "service_end_date").ok_or_else(|| at("service_end_date", "a deferred line needs the day its service ends"))?;
    if end < start {
        return Err(at("service_end_date", "the service ends before it starts"));
    }
    Ok(Some((start, end)))
}

/// Where a deferred line waits: its own deferred account, else the company's; `None` when the line
/// is not deferred.
pub(crate) fn account(row: &Doc, co: &Company, selling: bool, line: usize) -> Result<Option<Uuid>> {
    if period(row, selling, line)?.is_none() {
        return Ok(None);
    }
    let k = kind(selling);
    Ok(Some(match id(row, k.account) {
        Some(a) => a,
        None => co.require(k.company_default, k.what)?,
    }))
}

/// What `amount`, spread evenly over the days from `start` to `end`, has earned by the end of
/// `through`: none before it starts, all of it from its last day.
pub fn earned(amount: Decimal, start: NaiveDate, end: NaiveDate, through: NaiveDate, precision: u32) -> Decimal {
    if through < start {
        return Decimal::ZERO;
    }
    if through >= end {
        return amount;
    }
    let days = Decimal::from((end - start).num_days() + 1);
    let elapsed = Decimal::from((through - start).num_days() + 1);
    round(amount * elapsed / days, precision)
}

/// Recognises, for every submitted invoice of `only` (a company) or every company, what its
/// deferred lines earned or used through `through`. Answers how many lines moved.
pub async fn recognise(k: &Kernel, ctx: &Ctx, tx: &mut Tx, only: Option<Uuid>, through: NaiveDate) -> Result<usize> {
    let mut moved = 0;
    for kd in &KINDS {
        let Some(ent) = ctx.schema.entity(kd.invoice) else { continue };
        let Some(lines) = ent.children.get("items").map(|c| c.table.clone()) else { continue };
        let invoice_table = ent.table.clone();
        let waiting: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select distinct i.id from {invoice_table} i join {lines} l on l.tenant_id = i.tenant_id and l.parent_id = i.id \
             where i.tenant_id = $1 and i.docstatus = 1 and ($2::uuid is null or i.company = $2) \
             and coalesce(l.{flag}, false) and l.service_start_date <= $3 \
             and (l.deferred_booked_till is null or l.deferred_booked_till < l.service_end_date) and (l.deferred_booked_till is null or l.deferred_booked_till < $3) \
             order by i.id",
            invoice_table = q(&invoice_table),
            lines = q(&lines),
            flag = kd.flag,
        )))
        .bind(ctx.tenant)
        .bind(only)
        .bind(through)
        .fetch_all(&mut **tx)
        .await?;
        for iid in waiting {
            moved += recognise_invoice(k, ctx, tx, kd, iid, through).await?;
        }
    }
    Ok(moved)
}

async fn recognise_invoice(k: &Kernel, ctx: &Ctx, tx: &mut Tx, kd: &Kind, iid: Uuid, through: NaiveDate) -> Result<usize> {
    // Locked, and taken in the order of their ids: two runs at once (on two replicas, or an
    // accountant's and the monthly one) recognise each day once, the second finding it done.
    let doc = k.get_tx(ctx, tx, kd.invoice, iid, true).await?;
    let co = company(k, ctx, tx, id(&doc, "company").unwrap_or_default()).await?;
    let mut entry: Option<(Entry, NaiveDate)> = None;
    let mut updates: Vec<(Uuid, Decimal, NaiveDate)> = Vec::new();
    for (i, row) in rows(&doc, "items").iter().enumerate() {
        let Some((start, end)) = period(row, kd.selling, i)? else { continue };
        let (Some(line_id), Some(deferred)) = (id(row, "id"), account(row, &co, kd.selling, i)?) else { continue };
        let amount = if row.get("base_net_amount").is_some_and(|v| !v.is_null()) { d(row, "base_net_amount") } else { d(row, "net_amount") };
        let booked = d(row, "deferred_booked");
        let till = through.min(end);
        let due = earned(amount, start, end, till, co.precision) - booked;
        if due.is_zero() {
            continue;
        }
        let own = match id(row, kd.own_account) {
            Some(a) => a,
            None => co.require(
                if kd.selling { "default_income_account" } else { "default_expense_account" },
                if kd.selling { "default income account" } else { "default expense account" },
            )?,
        };
        let (e, _) = entry.get_or_insert_with(|| {
            let mut e = Entry::new(co.id, till, kd.invoice, iid, s(&doc, "number").map(|n| format!("{n} (deferred)")));
            e.memo = Some(format!("{} recognised through {till}", if kd.selling { "Deferred revenue" } else { "Deferred expense" }));
            (e, till)
        });
        // Revenue leaves the liability for income; an expense leaves the asset for the expense.
        let sign = if kd.selling { Decimal::ONE } else { -Decimal::ONE };
        let cost_center = id(row, "cost_center");
        e.lines.push(Line::new(deferred, sign * due, &co.currency_code).cost_center(cost_center).remarks("Deferred"));
        e.lines.push(Line::new(own, -sign * due, &co.currency_code).cost_center(cost_center).project(id(&doc, "project")).remarks("Recognised"));
        updates.push((line_id, booked + due, till));
    }
    let Some((mut e, _)) = entry else { return Ok(0) };
    // One posting date per entry: the latest line's, never past `through`.
    e.posting_date = updates.iter().map(|u| u.2).max().unwrap_or(through);
    ledger::post(tx, ctx, e, co.precision).await?;
    for (line, booked, till) in &updates {
        let mut v = Doc::new();
        v.insert("deferred_booked".into(), json!(booked.normalize().to_string()));
        v.insert("deferred_booked_till".into(), json!(till.to_string()));
        k.set_child_values_tx(ctx, tx, kd.invoice, "items", *line, v).await?;
    }
    Ok(updates.len())
}

/// The company action `recognise_deferred {through?}`: recognises through a day (by default
/// yesterday), as an accountant does before closing a period.
pub async fn action(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, params: &Value) -> Result<Value> {
    let through = match params.get("through").and_then(Value::as_str).filter(|v| !v.is_empty()) {
        Some(v) => parse_date(v).map_err(|_| Error::field("through", "is a day"))?,
        None => ctx.today().pred_opt().unwrap_or(ctx.today()),
    };
    if through > ctx.today() {
        return Err(Error::field("through", "deferred amounts are recognised up to a day that has come"));
    }
    let moved = recognise(k, ctx, tx, Some(company), through).await?;
    Ok(
        json!({"lines": moved, "through": through.to_string(), "message": format!("{moved} deferred line{} recognised through {through}", if moved == 1 { "" } else { "s" })}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ymd(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn earned_by_the_day() {
        let (start, end) = (ymd(2026, 1, 1), ymd(2026, 12, 31));
        let year = Decimal::from(12_000);
        assert_eq!(earned(year, start, end, ymd(2025, 12, 31), 2), Decimal::ZERO, "before it starts");
        assert_eq!(earned(year, start, end, ymd(2026, 1, 1), 2), Decimal::new(3288, 2), "its first day: 12000 / 365");
        assert_eq!(earned(year, start, end, ymd(2026, 3, 31), 2), Decimal::new(295890, 2), "90 days of 365");
        assert_eq!(earned(year, start, end, ymd(2026, 12, 31), 2), year, "all of it on its last day");
        assert_eq!(earned(year, start, end, ymd(2027, 6, 1), 2), year, "and after");
        assert_eq!(earned(Decimal::from(100), ymd(2026, 2, 1), ymd(2026, 2, 1), ymd(2026, 2, 1), 2), Decimal::from(100), "a one-day service");
        // Thirds of a hundred over three days: what rounding left goes to the last day.
        let (a, b) = (ymd(2026, 5, 1), ymd(2026, 5, 3));
        let firsts = [earned(Decimal::from(100), a, b, a, 2), earned(Decimal::from(100), a, b, ymd(2026, 5, 2), 2), earned(Decimal::from(100), a, b, b, 2)];
        assert_eq!(firsts, [Decimal::new(3333, 2), Decimal::new(6667, 2), Decimal::from(100)]);
    }
}
