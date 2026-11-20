//! The EU's returns, from submitted invoices' tax breakdowns (in the company's currency, as the
//! books hold them): the EC sales list (Art. 262: supplies of goods and services to businesses
//! elsewhere in the EU, per customer VAT number) and the One-Stop Shop's return (Art. 369g: sales
//! to consumers elsewhere in the EU, per member state of consumption and rate, in euro at the
//! European Central Bank's rate of the period's last day, Art. 369h); and the Intrastat figures (goods that moved
//! between member states: dispatches and arrivals by partner country, commodity code, origin and nature, from the
//! documents that move goods).

use crate::util::{exchange_rate, money};
use crate::vat::vies_code;
use ferrum_domain::reports::{Params, ReportSource, col, param_def, period, read_tx, resolve_company};
use ferrum_kernel::{Ctx, Error, Filter, Kernel, Result};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

pub struct EuReports;

const DEFS: &[(&str, &str)] = &[("eu_ec_sales_list", "EC Sales List"), ("eu_oss_return", "OSS Return"), ("eu_intrastat", "Intrastat")];

#[async_trait::async_trait]
impl ReportSource for EuReports {
    fn catalog(&self) -> Vec<Value> {
        DEFS.iter()
            .map(|(name, label)| {
                let mut params: Vec<Value> = ["company", "from_date", "to_date"].iter().map(|p| param_def(p)).collect();
                // Intrastat counts goods that moved: who may read delivery notes may run it.
                let entity = if *name == "eu_intrastat" {
                    params.push(json!({"name": "flow", "label": "Flow", "type": "select", "options": ["", "Dispatches", "Arrivals"]}));
                    "DeliveryNote"
                } else {
                    "SalesInvoice"
                };
                json!({"name": name, "label": label, "module": "accounting", "entity": entity, "scoped": false, "params": params})
            })
            .collect()
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
        let label = DEFS.iter().find(|(n, _)| *n == name).map(|(_, l)| *l).ok_or_else(|| Error::NotFound(format!("report {name}")))?;
        let p = Params { raw };
        let mut tx = read_tx(k, ctx).await?;
        let co = resolve_company(&mut tx, ctx, &p).await?;
        let (from, to) = period(&p, &co)?;
        if name == "eu_intrastat" {
            let out = intrastat(ctx, &mut tx, co.id, (from, to), raw.get("flow").map(String::as_str).unwrap_or_default()).await?;
            tx.rollback().await?;
            return Ok(finish(out, name, label, from, to));
        }
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
        let out = if name == "eu_ec_sales_list" {
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
        Ok(finish(out, name, label, from, to))
    }
}

fn finish(mut out: Value, name: &str, label: &str, from: chrono::NaiveDate, to: chrono::NaiveDate) -> Value {
    if let Value::Object(m) = &mut out {
        m.insert("name".into(), json!(name));
        m.insert("label".into(), json!(label));
        let meta = m.entry("meta").or_insert(json!({}));
        meta["from_date"] = json!(from);
        meta["to_date"] = json!(to);
    }
    out
}

/// One goods line of a document that moved goods, as Intrastat reads it.
#[derive(sqlx::FromRow)]
struct GoodsLine {
    dispatch: bool,
    number: Option<String>,
    is_return: Option<bool>,
    partner: Option<String>,
    partner_vat: Option<String>,
    item: Option<String>,
    cn: Option<String>,
    mass: Option<Decimal>,
    supplementary: Option<Decimal>,
    origin: Option<String>,
    qty: Option<Decimal>,
    value: Option<Decimal>,
}

/// The goods that moved between the company's member state and another in the period: delivery notes and sales
/// invoices that update stock (dispatches, partner = where the goods went), purchase receipts and purchase invoices that
/// update stock (arrivals, partner = the supplier's address); a return is the other flow with nature 21. Stock items only,
/// submitted documents only; values in the company's currency.
async fn intrastat(ctx: &Ctx, tx: &mut ferrum_kernel::Tx, company_id: Uuid, (from, to): (chrono::NaiveDate, chrono::NaiveDate), flow: &str) -> Result<Value> {
    let country: Option<String> = sqlx::query_scalar("select country_code from e_company where tenant_id = $1 and id = $2")
        .bind(ctx.tenant)
        .bind(company_id)
        .fetch_optional(&mut **tx)
        .await?
        .flatten();
    let home = country.filter(|c| crate::vat::member(c)).ok_or_else(|| Error::invalid("Intrastat is for companies in an EU member state"))?;
    let q = ferrum_kernel::ddl::q;
    let mut parts = Vec::new();
    for (entity, dispatch, partner, vat, stock_only) in [
        ("DeliveryNote", true, "upper(d.delivery_country)", "pt.ext->>'eu_vat_number'", false),
        ("SalesInvoice", true, "upper(d.delivery_country)", "pt.ext->>'eu_vat_number'", true),
        ("PurchaseReceipt", false, "upper(a.country_code)", "null::text", false),
        ("PurchaseInvoice", false, "upper(a.country_code)", "null::text", true),
    ] {
        let docs = ferrum_meta::table_for(entity);
        let lines = ferrum_meta::child_table(&docs, "items");
        let (party_table, party, address) =
            if dispatch { ("e_customer", "customer", "d.customer_address") } else { ("e_supplier", "supplier", "d.supplier_address") };
        parts.push(format!(
            "select {dispatch} as dispatch, d.number, d.is_return, {partner} as partner, {vat} as partner_vat, i.item_name as item, i.ext->>'eu_cn_code' as cn, \
             nullif(i.ext->>'eu_net_mass', '')::numeric as mass, nullif(i.ext->>'eu_supplementary_factor', '')::numeric as supplementary, \
             upper(i.ext->>'eu_origin_country') as origin, \
             abs(coalesce(l.stock_qty, l.qty * coalesce(nullif(l.conversion_factor, 0), 1))) as qty, \
             abs(l.net_amount * coalesce(nullif(d.conversion_rate, 0), 1)) as value \
             from {d} d join {l} l on l.tenant_id = d.tenant_id and l.parent_id = d.id \
             join e_item i on i.tenant_id = l.tenant_id and i.id = l.item \
             left join {party_table} pt on pt.tenant_id = d.tenant_id and pt.id = d.{party} \
             left join e_address a on a.tenant_id = d.tenant_id and a.id = {address} \
             where d.tenant_id = $1 and d.company = $2 and d.docstatus = 1 and d.posting_date between $3 and $4 and coalesce(i.is_stock_item, false){stock}",
            d = q(&docs),
            l = q(&lines),
            stock = if stock_only { " and coalesce(d.update_stock, false)" } else { "" },
        ));
    }
    let lines: Vec<GoodsLine> =
        sqlx::query_as(sqlx::AssertSqlSafe(parts.join(" union all "))).bind(ctx.tenant).bind(company_id).bind(from).bind(to).fetch_all(&mut **tx).await?;
    // (flow, partner, code, origin, nature, partner's VAT number) -> value, mass, supplementary units, lines.
    let mut rows: BTreeMap<(String, String, String, String, String, String), (Decimal, Decimal, Option<Decimal>, u32)> = BTreeMap::new();
    let mut incomplete = Vec::new();
    let mut missing_count = 0usize;
    for l in lines {
        let ret = l.is_return.unwrap_or(false);
        let dispatch = l.dispatch != ret;
        let f = if dispatch { "Dispatches" } else { "Arrivals" };
        if !flow.is_empty() && flow != f {
            continue;
        }
        let partner = l.partner.clone().filter(|p| !p.is_empty());
        if partner.as_deref().is_some_and(|p| p == home || !crate::vat::member(p)) {
            continue;
        }
        let mut missing = Vec::new();
        if l.cn.as_deref().is_none_or(str::is_empty) {
            missing.push("commodity code");
        }
        if l.mass.is_none() && l.supplementary.is_none() {
            missing.push("net mass or supplementary units");
        }
        if partner.is_none() {
            missing.push("partner country");
        }
        if !missing.is_empty() {
            missing_count += 1;
            if incomplete.len() < 200 {
                incomplete.push(json!({"document": l.number, "item": l.item, "missing": missing.join(", ")}));
            }
            continue;
        }
        let partner = partner.unwrap_or_default();
        let qty = l.qty.unwrap_or_default();
        let origin = l.origin.filter(|o| !o.is_empty()).unwrap_or_else(|| if dispatch { home.clone() } else { partner.clone() });
        let nature = if ret { "21" } else { "11" };
        let vat = if dispatch { l.partner_vat.unwrap_or_default() } else { String::new() };
        let e =
            rows.entry((f.to_string(), partner, l.cn.unwrap_or_default(), origin, nature.to_string(), vat)).or_insert((Decimal::ZERO, Decimal::ZERO, None, 0));
        e.0 += l.value.unwrap_or_default();
        e.1 += qty * l.mass.unwrap_or_default();
        if let Some(s) = l.supplementary {
            e.2 = Some(e.2.unwrap_or_default() + qty * s);
        }
        e.3 += 1;
    }
    let out: Vec<Value> = rows
        .iter()
        .map(|((f, partner, cn, origin, nature, vat), (value, mass, sup, n))| {
            json!({"flow": f, "partner": partner, "cn_code": cn, "origin": origin, "nature": nature, "partner_vat": vat, "value": money(value.round_dp(2)),
                   "net_mass": money(mass.round_dp(3)), "supplementary": sup.map(|s| money(s.round_dp(3))), "lines": n})
        })
        .collect();
    Ok(json!({
        "columns": [col("flow", "Flow", "data"), col("partner", "Partner Country", "data"), col("cn_code", "Commodity Code", "data"), col("origin", "Origin", "data"),
                    col("nature", "Nature", "data"), col("partner_vat", "Partner VAT Number", "data"), col("value", "Invoiced Value", "currency"),
                    col("net_mass", "Net Mass (kg)", "decimal"), col("supplementary", "Supplementary Units", "decimal"), col("lines", "Lines", "int")],
        "rows": out,
        "totals": {"value": money(rows.values().map(|v| v.0).sum::<Decimal>().round_dp(2)), "net_mass": money(rows.values().map(|v| v.1).sum::<Decimal>().round_dp(3))},
        "incomplete": incomplete,
        "meta": {"incomplete": missing_count},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_returns_are_listed() {
        let c = EuReports.catalog();
        assert_eq!(c.iter().map(|e| e["name"].as_str().unwrap()).collect::<Vec<_>>(), ["eu_ec_sales_list", "eu_oss_return", "eu_intrastat"]);
        assert_eq!(c[2]["entity"], "DeliveryNote", "who may read delivery notes may run Intrastat");
        assert!(c.iter().all(|e| e["params"].as_array().unwrap().iter().any(|p| p["name"] == "company")));
    }
}
