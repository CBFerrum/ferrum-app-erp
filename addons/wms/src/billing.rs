//! What customers pay for storage: bin-days counted once a day, and units handled from the ledger.
//! The figures are the warehouse's own records, so a customer's report and their invoice agree.

use chrono::NaiveDate;
use ferrum_domain::util::{d, elevated, id};
use ferrum_kernel::{Ctx, Doc, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

/// Today's count of bins holding each customer's stock. Repeating it in a day changes nothing.
pub async fn snapshot(tx: &mut Tx, tenant: Uuid, day: NaiveDate) -> Result<()> {
    sqlx::query(
        "insert into wms_storage_day (tenant_id, day, customer_id, bins) \
         select tenant_id, $2, owner_id, count(distinct bin_id) from wms_position where tenant_id = $1 and owner_id is not null and qty > 0 group by tenant_id, owner_id \
         on conflict (tenant_id, day, customer_id) do nothing",
    )
    .bind(tenant)
    .bind(day)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// A customer's bill for a period: a draft sales invoice to review.
pub async fn bill(k: &Kernel, ctx: &Ctx, tx: &mut Tx, customer: Uuid, from: NaiveDate, to: NaiveDate) -> Result<Value> {
    if to < from {
        return Err(Error::invalid("the period ends before it starts"));
    }
    let contract: Option<(Uuid, Uuid, Decimal, Option<Decimal>)> =
        sqlx::query_as("select company, service_item, rate_per_bin_day, rate_per_unit_moved from e_storage_contract where tenant_id = $1 and customer = $2 and active").bind(ctx.tenant).bind(customer).fetch_optional(&mut **tx).await?;
    let Some((company, service, per_bin, per_unit)) = contract else { return Err(Error::invalid("this customer has no storage contract")) };
    let bin_days: i64 = sqlx::query_scalar("select coalesce(sum(bins), 0)::int8 from wms_storage_day where tenant_id = $1 and customer_id = $2 and day between $3 and $4").bind(ctx.tenant).bind(customer).bind(from).bind(to).fetch_one(&mut **tx).await?;
    let handled: Decimal = sqlx::query_scalar(
        "select coalesce(sum(qty), 0) from wms_move where tenant_id = $1 and owner_id = $2 and kind in ('Receive', 'Issue') and at::date between $3 and $4",
    )
    .bind(ctx.tenant)
    .bind(customer)
    .bind(from)
    .bind(to)
    .fetch_one(&mut **tx)
    .await?;
    let mut items = vec![json!({"item": service, "qty": bin_days.max(0), "rate": per_bin, "price_list_rate": per_bin, "description": format!("Storage {from} to {to}: {bin_days} bin-days")})];
    if let Some(rate) = per_unit.filter(|r| *r > Decimal::ZERO)
        && handled > Decimal::ZERO
    {
        items.push(json!({"item": service, "qty": handled, "rate": rate, "price_list_rate": rate, "description": format!("Handling {from} to {to}")}));
    }
    if bin_days <= 0 && items.len() == 1 {
        return Err(Error::invalid("nothing was stored for this customer in that period"));
    }
    items.retain(|i| i["qty"].as_i64() != Some(0));
    let doc = json!({"company": company, "customer": customer, "items": items, "remarks": format!("Warehouse charges, {from} to {to}")});
    let made: Doc = k.insert_tx(&elevated(ctx), tx, "SalesInvoice", doc.as_object().cloned().unwrap_or_default()).await?;
    let total = d(&made, "grand_total");
    Ok(json!({"invoice": id(&made, "id"), "bin_days": bin_days, "handled": handled, "total": total}))
}
