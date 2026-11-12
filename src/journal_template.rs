//! Recurring journals (FN-06, FN-07): a journal template holds an entry's lines and makes one every
//! period from its start until its end date or its count, as a draft or submitted, by the app's
//! upkeep. The platform has no recurring documents, so this is journals only. Each period is made
//! once: the template is locked while one is made, and a journal entry's (template, period) is
//! unique in the database, so two replicas at once make it once. Periods missed while the upkeep did
//! not run are made when it next does, each dated on its own day. An entry that reverses itself (an
//! accrual) is taken out again on its reversal day by the upkeep that already does that.

use crate::util::{b, date, id, rows, s, sys};
use async_trait::async_trait;
use chrono::{Months, NaiveDate};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, HookCtx, Kernel, Result};
use serde_json::{Value, json};
use uuid::Uuid;

const ENTITY: &str = "JournalTemplate";

fn months(frequency: Option<&str>) -> u32 {
    match frequency {
        Some("Quarterly") => 3,
        Some("Half-yearly") => 6,
        Some("Yearly") => 12,
        _ => 1,
    }
}

/// The day of period `n` (from 0): the start date moved `n` periods, the month's last day when
/// the start's day is past it (31 January, 28 February, 31 March).
pub fn period(start: NaiveDate, frequency: Option<&str>, n: u32) -> Option<NaiveDate> {
    start.checked_add_months(Months::new(months(frequency) * n))
}

/// The day the next entry is due, or `None` when the template has made all it will.
fn next(t: &Doc) -> Option<NaiveDate> {
    if !b(t, "repeat") {
        return None;
    }
    let made = t.get("made_count").and_then(Value::as_i64).unwrap_or(0).max(0) as u32;
    let limit = t.get("occurrences").and_then(Value::as_i64).filter(|n| *n > 0);
    if limit.is_some_and(|l| made as i64 >= l) {
        return None;
    }
    let day = period(date(t, "start_date").ok()?, s(t, "frequency"), made)?;
    match date(t, "end_date") {
        Ok(end) if day > end => None,
        _ => Some(day),
    }
}

pub struct TemplateHook;

#[async_trait]
impl DocHook for TemplateHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let due = next(doc).map(|d| json!(d.to_string())).unwrap_or(Value::Null);
        doc.insert("next_date".into(), due);
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if let (Ok(start), Ok(end)) = (date(doc, "start_date"), date(doc, "end_date"))
            && end < start
        {
            return Err(Error::field("end_date", "cannot be before the start"));
        }
        if rows(doc, "accounts").is_empty() {
            return Err(Error::field("accounts", "a template needs its lines"));
        }
        Ok(())
    }
}

/// Makes every entry that is due by `today`, each period in a transaction of its own. Returns how
/// many were made.
pub async fn upkeep(k: &Kernel, ctx: &Ctx, today: NaiveDate) -> Result<usize> {
    let Some(ent) = ctx.schema.entity(ENTITY) else { return Ok(0) };
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let due: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select id from {} where tenant_id = $1 and not coalesce(disabled, false) and next_date <= $2 order by next_date, id",
        ferrum_kernel::ddl::q(&ent.table)
    )))
    .bind(ctx.tenant)
    .bind(today)
    .fetch_all(&mut *tx)
    .await?;
    tx.rollback().await?;
    let mut made = 0;
    for tid in due {
        // One period a round of the loop, until the template has nothing more due.
        for _ in 0..1000 {
            let mut tx = k.db.tenant_tx(ctx.tenant).await?;
            match make_one(k, ctx, &mut tx, tid, today).await {
                Ok(true) => {
                    tx.commit().await?;
                    made += 1;
                }
                Ok(false) => {
                    tx.rollback().await?;
                    break;
                }
                Err(e) => {
                    tx.rollback().await?;
                    tracing::warn!(tenant = %ctx.tenant, template = %tid, "recurring journal not made: {e}");
                    break;
                }
            }
        }
    }
    Ok(made)
}

/// Makes the template's next entry when it is due: locked, so a second replica waits and then
/// finds it made.
async fn make_one(k: &Kernel, ctx: &Ctx, tx: &mut ferrum_kernel::Tx, tid: Uuid, today: NaiveDate) -> Result<bool> {
    let sctx = sys(ctx);
    let t = k.get_tx(&sctx, tx, ENTITY, tid, true).await?;
    let Some(day) = next(&t).filter(|d| *d <= today && !b(&t, "disabled")) else { return Ok(false) };
    let lines: Vec<Value> = rows(&t, "accounts")
        .into_iter()
        .map(|mut l| {
            for f in ["id", "idx", "parent_id"] {
                l.remove(f);
            }
            Value::Object(l)
        })
        .collect();
    let mut je = Doc::new();
    je.insert("company".into(), json!(id(&t, "company")));
    je.insert("posting_date".into(), json!(day.to_string()));
    je.insert("accounts".into(), Value::Array(lines));
    je.insert("remark".into(), json!(s(&t, "remark").map(str::to_string).unwrap_or_else(|| s(&t, "template_name").unwrap_or_default().to_string())));
    je.insert("auto_reverse".into(), json!(b(&t, "auto_reverse")));
    je.insert("journal_template".into(), json!(tid));
    je.insert("template_period".into(), json!(day.to_string()));
    let made = k.insert_tx(&sctx, tx, "JournalEntry", je).await?;
    if b(&t, "submit") {
        k.submit_tx(&sctx, tx, "JournalEntry", id(&made, "id").unwrap_or_default(), None).await?;
    }
    let count = t.get("made_count").and_then(Value::as_i64).unwrap_or(0) + 1;
    let mut after = t.clone();
    after.insert("made_count".into(), json!(count));
    let mut v = Doc::new();
    v.insert("made_count".into(), json!(count));
    v.insert("last_made".into(), json!(day.to_string()));
    v.insert("next_date".into(), next(&after).map(|d| json!(d.to_string())).unwrap_or(Value::Null));
    k.set_values_tx(&sctx, tx, ENTITY, tid, v).await?;
    Ok(true)
}
