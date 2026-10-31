//! The EU's returns, from submitted invoices' tax breakdowns (in the company's currency, as the
//! books hold them): the EC sales list (Art. 262: supplies of goods and services to businesses
//! elsewhere in the EU, per customer VAT number) and the One-Stop Shop's return (Art. 369g: sales
//! to consumers elsewhere in the EU, per member state of consumption and rate, in euro at the
//! European Central Bank's rate of the period's last day, Art. 369h).

use crate::util::{exchange_rate, money};
use crate::vat::vies_code;
use ferrum_domain::reports::{Params, ReportSource, col, param_def, period, read_tx, resolve_company};
use ferrum_kernel::{Ctx, Error, Filter, Kernel, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

pub struct EuReports;

const DEFS: &[(&str, &str)] = &[("eu_ec_sales_list", "EC Sales List"), ("eu_oss_return", "OSS Return")];

#[async_trait::async_trait]
impl ReportSource for EuReports {
    fn catalog(&self) -> Vec<Value> {
        DEFS.iter()
            .map(|(name, label)| {
                let params: Vec<Value> = ["company", "from_date", "to_date"].iter().map(|p| param_def(p)).collect();
                json!({"name": name, "label": label, "module": "accounting", "entity": "SalesInvoice", "scoped": false, "params": params})
            })
            .collect()
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
        let label = DEFS.iter().find(|(n, _)| *n == name).map(|(_, l)| *l).ok_or_else(|| Error::NotFound(format!("report {name}")))?;
        let p = Params { raw };
        let mut tx = read_tx(k, ctx).await?;
        let co = resolve_company(&mut tx, ctx, &p).await?;
        let (from, to) = period(&p, &co)?;
        let invoices_table = ferrum_meta::table_for("SalesInvoice");
        let breakdown = ferrum_kernel::ddl::q(&ferrum_meta::child_table(&invoices_table, "tax_breakdown"));
        let invoices = ferrum_kernel::ddl::q(&invoices_table);
        let supply = if name == "eu_ec_sales_list" { "Intra-EU B2B" } else { "Intra-EU B2C (OSS)" };
        let data: Vec<(Option<String>, Option<String>, Option<String>, Option<Decimal>, Option<Decimal>, Option<Decimal>)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "select i.eu_customer_vat, i.eu_destination, b.category, b.rate, sum(b.base_taxable_amount), sum(b.base_tax_amount) \
             from {invoices} i join {breakdown} b on b.tenant_id = i.tenant_id and b.parent_id = i.id \
             where i.tenant_id = $1 and i.company = $2 and i.docstatus = 1 and i.posting_date between $3 and $4 and i.eu_supply = $5 \
             group by 1, 2, 3, 4 order by 1, 2, 3, 4"
            )))
            .bind(ctx.tenant)
            .bind(co.id)
            .bind(from)
            .bind(to)
            .bind(supply)
            .fetch_all(&mut *tx)
            .await?;
        let mut out = if name == "eu_ec_sales_list" {
            // Goods (intra-Community supplies) and services (reverse-charged) per customer.
            let mut per: BTreeMap<String, (Decimal, Decimal)> = BTreeMap::new();
            for (vat, _, category, _, taxable, _) in data {
                let e = per.entry(vat.unwrap_or_default()).or_default();
                match category.as_deref() {
                    Some("Intra-Community") => e.0 += taxable.unwrap_or_default(),
                    Some("Reverse Charge") => e.1 += taxable.unwrap_or_default(),
                    _ => {}
                }
            }
            let rows: Vec<Value> = per
                .iter()
                .map(|(vat, (goods, services))| {
                    json!({"country": vat.get(..2), "vat_number": vat.get(2..), "goods": money(*goods), "services": money(*services), "total": money(*goods + *services)})
                })
                .collect();
            json!({
                "columns": [col("country", "Member State", "data"), col("vat_number", "Customer VAT Number", "data"), col("goods", "Goods", "currency"),
                            col("services", "Services", "currency"), col("total", "Total", "currency")],
                "rows": rows,
                "totals": {"goods": money(per.values().map(|v| v.0).sum()), "services": money(per.values().map(|v| v.1).sum())},
            })
        } else {
            // In euro: the company's currency at the ECB's rate of the period's last day.
            let eur = {
                let found = crate::util::find(k, ctx, &mut tx, "Currency", vec![Filter::eq("code", "EUR")], None, 1).await?;
                found.first().and_then(|c| crate::util::id(c, "id"))
            };
            let base_currency: Option<Uuid> = sqlx::query_scalar("select default_currency from e_company where tenant_id = $1 and id = $2")
                .bind(ctx.tenant)
                .bind(co.id)
                .fetch_optional(&mut *tx)
                .await?
                .flatten();
            let rate = match (base_currency, eur) {
                (Some(b), Some(e)) if b == e => Decimal::ONE,
                (Some(b), Some(e)) => exchange_rate(k, ctx, &mut tx, b, e, to).await.map_err(|_| {
                    Error::invalid(format!("the OSS return is in euro: add the European Central Bank's rate of the company's currency to EUR on {to}"))
                })?,
                _ => return Err(Error::invalid("the OSS return is in euro: the workspace needs the EUR currency")),
            };
            let mut rows = Vec::new();
            let (mut total_taxable, mut total_vat) = (Decimal::ZERO, Decimal::ZERO);
            for (_, dest, _, rate_pct, taxable, tax) in data {
                let (taxable, tax) = ((taxable.unwrap_or_default() * rate).round_dp(2), (tax.unwrap_or_default() * rate).round_dp(2));
                total_taxable += taxable;
                total_vat += tax;
                rows.push(json!({"member_state": dest.as_deref().map(vies_code), "rate": money(rate_pct.unwrap_or_default()), "taxable": money(taxable), "vat": money(tax)}));
            }
            json!({
                "columns": [col("member_state", "Member State of Consumption", "data"), col("rate", "VAT Rate", "percent"), col("taxable", "Taxable Amount (EUR)", "currency"),
                            col("vat", "VAT (EUR)", "currency")],
                "rows": rows,
                "totals": {"taxable": money(total_taxable), "vat": money(total_vat)},
                "meta": {"eur_rate": rate.to_string()},
            })
        };
        tx.rollback().await?;
        if let Value::Object(m) = &mut out {
            m.insert("name".into(), json!(name));
            m.insert("label".into(), json!(label));
            let meta = m.entry("meta").or_insert(json!({}));
            meta["from_date"] = json!(from);
            meta["to_date"] = json!(to);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_returns_are_listed() {
        let c = EuReports.catalog();
        assert_eq!(c.iter().map(|e| e["name"].as_str().unwrap()).collect::<Vec<_>>(), ["eu_ec_sales_list", "eu_oss_return"]);
        assert!(c.iter().all(|e| e["params"].as_array().unwrap().iter().any(|p| p["name"] == "company")));
    }
}
