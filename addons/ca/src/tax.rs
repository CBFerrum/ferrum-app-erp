//! Canadian sales taxes on ERP's documents, for a company in Canada.
//!
//! * The province of supply: goods where they are delivered (the ship-to), services by the
//!   customer's address (its billing address); outside Canada, an export (zero-rated).
//! * GST 5%, or HST in the participating provinces; zero-rated items (basic groceries) and exempt
//!   ones (most health and financial services) carry none.
//! * PST (British Columbia, Saskatchewan) and RST (Manitoba) where the company collects it, unless
//!   the item is not subject to it or the customer buys for resale; QST in Quebec when the company
//!   is registered for it, on what GST applies to.
//! * Purchases: the GST/HST (and QST in Quebec) the company recovers, at its own province's rates.

use crate::util::{b, find, get, id, money, rows, s, set, sys};
use async_trait::async_trait;
use ferrum_app_erp::localization::Localization;
use ferrum_app_erp::pricing::Side;
use ferrum_domain::reports::{Params, ReportSource, col, param_def, period, read_tx, resolve_company};
use ferrum_kernel::{Ctx, Doc, Error, Filter, HookCtx, Kernel, Result, Tx};
use ferrum_meta::EffEntity;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::str::FromStr;
use uuid::Uuid;

/// GST/HST collected and recovered post to the standard chart's output and input tax.
pub const GST_PAYABLE: &str = "2131";
pub const GST_RECOVERABLE: &str = "1151";
pub const PST_PAYABLE: &str = "2161";
pub const QST_PAYABLE: &str = "2162";
pub const QST_RECOVERABLE: &str = "1158";
const ADDED: &[(&str, &str, &str, &str)] = &[
    (PST_PAYABLE, "PST Payable", "Duties and Taxes", "Liability"),
    (QST_PAYABLE, "QST Payable", "Duties and Taxes", "Liability"),
    (QST_RECOVERABLE, "QST Recoverable (ITR)", "Tax Assets", "Asset"),
];

pub const GST: &str = "5";
pub const QST: &str = "9.975";
/// HST by province, from the day each rate took effect.
const HST: &[(&str, &str, &str)] = &[
    ("ON", "2010-07-01", "13"),
    ("NB", "2016-07-01", "15"),
    ("NL", "2016-07-01", "15"),
    ("PE", "2016-10-01", "15"),
    ("NS", "2010-07-01", "15"),
    ("NS", "2025-04-01", "14"),
];
/// PST and RST by province, from the day each rate took effect.
const PST: &[(&str, &str, &str)] = &[("BC", "2013-04-01", "7"), ("MB", "2019-07-01", "7"), ("SK", "2017-03-23", "6")];

const PROVINCES: &[(&str, &[&str])] = &[
    ("AB", &["alberta"]),
    ("BC", &["british columbia", "colombie-britannique"]),
    ("MB", &["manitoba"]),
    ("NB", &["new brunswick", "nouveau-brunswick"]),
    ("NL", &["newfoundland and labrador", "newfoundland", "labrador", "terre-neuve-et-labrador"]),
    ("NS", &["nova scotia", "nouvelle-écosse"]),
    ("NT", &["northwest territories", "territoires du nord-ouest"]),
    ("NU", &["nunavut"]),
    ("ON", &["ontario"]),
    ("PE", &["prince edward island", "pei", "île-du-prince-édouard"]),
    ("QC", &["quebec", "québec"]),
    ("SK", &["saskatchewan"]),
    ("YT", &["yukon"]),
];

/// A province's two-letter code from its code or name.
pub fn province(text: &str) -> Option<&'static str> {
    let t = text.trim().to_lowercase();
    PROVINCES.iter().find(|(code, names)| t == code.to_lowercase() || names.contains(&t.as_str())).map(|(code, _)| *code)
}

fn dated(table: &[(&str, &str, &str)], province: &str, on: &str) -> Option<Decimal> {
    let mut found = None;
    for (p, from, rate) in table {
        if *p == province && *from <= on {
            found = Decimal::from_str(rate).ok();
        }
    }
    found
}

/// The GST or HST rate of a province on a day.
pub fn gst_hst(province: &str, on: &str) -> Decimal {
    dated(HST, province, on).unwrap_or_else(|| Decimal::from_str(GST).unwrap_or_default())
}

pub fn is_hst(province: &str, on: &str) -> bool {
    dated(HST, province, on).is_some()
}

pub fn pst(province: &str, on: &str) -> Option<Decimal> {
    dated(PST, province, on)
}

/// Where a line is supplied: a province, or `None` outside Canada.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    Province(String),
    Abroad,
}

struct Plan {
    side: Side,
    on: String,
    home: String,
    /// Goods' place (the ship-to) and services' (the customer's address).
    goods: Place,
    services: Place,
    pst_registered: Vec<String>,
    qst_registered: bool,
    resale: bool,
}

async fn address(k: &Kernel, ctx: &Ctx, tx: &mut Tx, a: Option<Uuid>) -> Result<Option<Doc>> {
    let Some(a) = a else { return Ok(None) };
    Ok(find(k, ctx, tx, "Address", vec![Filter::eq("id", a.to_string())], None, 1).await?.into_iter().next())
}

fn place_of(a: Option<&Doc>, home: &str) -> Place {
    match a {
        Some(a) if s(a, "country_code").is_some_and(|c| c != "CA") => Place::Abroad,
        Some(a) => Place::Province(s(a, "state").and_then(province).unwrap_or(home).to_string()),
        None => Place::Province(home.to_string()),
    }
}

async fn plan(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc, side: Side) -> Result<Option<(Plan, Doc)>> {
    let Some(cid) = id(doc, "company") else { return Ok(None) };
    let company = get(k, ctx, tx, "Company", cid).await?;
    if s(&company, "country_code") != Some("CA") {
        return Ok(None);
    }
    let home = s(&company, "ca_province").and_then(province).unwrap_or("ON").to_string();
    let on = s(doc, "posting_date").map(str::to_string).unwrap_or_else(|| ctx.today().format("%Y-%m-%d").to_string());
    let (goods, services, resale) = match side {
        Side::Selling => {
            let billing = address(k, ctx, tx, id(doc, "customer_address")).await?;
            let ship = ferrum_app_erp::localization::ship_to(k, ctx, tx, doc).await?;
            let resale = match id(doc, "customer") {
                Some(c) => b(&get(k, ctx, tx, "Customer", c).await?, "ca_pst_exempt"),
                None => false,
            };
            (place_of(ship.as_ref(), &home), place_of(billing.as_ref(), &home), resale)
        }
        // Purchases are supplied where the company is; from abroad, imports.
        Side::Buying => {
            let from = address(k, ctx, tx, id(doc, "supplier_address")).await?;
            let p = if from.as_ref().and_then(|a| s(a, "country_code")).is_some_and(|c| c != "CA") { Place::Abroad } else { Place::Province(home.clone()) };
            (p.clone(), p, false)
        }
    };
    let pst_registered: Vec<String> =
        s(&company, "ca_pst_provinces").unwrap_or_default().split([',', ' ', ';']).filter_map(province).map(str::to_string).collect();
    let qst_registered = s(&company, "ca_qst_number").is_some();
    Ok(Some((Plan { side, on, home, goods, services, pst_registered, qst_registered, resale }, company)))
}

fn goods(item: &Doc) -> bool {
    match s(item, "ca_kind") {
        Some("Goods") => true,
        Some("Services") => false,
        _ => b(item, "is_stock_item"),
    }
}

/// A line's taxes: (GST/HST rate, PST rate, QST rate, category, reason).
fn line_taxes(plan: &Plan, item: &Doc) -> (Decimal, Decimal, Decimal, &'static str, Option<&'static str>) {
    let place = if goods(item) { &plan.goods } else { &plan.services };
    let Place::Province(p) = place else { return (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, "Export", Some("Zero-rated export")) };
    match s(item, "ca_gst_category").unwrap_or("Taxable") {
        "Zero Rated" => return (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, "Zero Rated", None),
        "Exempt" => return (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, "Exempt", Some("Exempt supply (Excise Tax Act, Schedule V)")),
        _ => {}
    }
    let gst = gst_hst(p, &plan.on);
    if plan.side == Side::Buying {
        let qst = if p == "QC" && plan.qst_registered { Decimal::from_str(QST).unwrap_or_default() } else { Decimal::ZERO };
        return (gst, Decimal::ZERO, qst, "Standard", None);
    }
    let pst = if !plan.resale && !b(item, "ca_pst_exempt") && plan.pst_registered.iter().any(|r| r == p) {
        pst(p, &plan.on).unwrap_or_default()
    } else {
        Decimal::ZERO
    };
    let qst = if p == "QC" && plan.qst_registered { Decimal::from_str(QST).unwrap_or_default() } else { Decimal::ZERO };
    (gst, pst, qst, "Standard", None)
}

async fn account(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, number: &str) -> Result<String> {
    let found = find(k, ctx, tx, "Account", vec![Filter::eq("company", company.to_string()), Filter::eq("account_number", number)], None, 1).await?;
    found
        .first()
        .and_then(|a| s(a, "id"))
        .map(str::to_string)
        .ok_or_else(|| Error::invalid(format!("the company has no account {number} for Canadian sales taxes: create its chart of accounts first")))
}

async fn items_of(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc) -> Result<Vec<Doc>> {
    let mut cache: HashMap<Uuid, Doc> = HashMap::new();
    let mut out = Vec::new();
    for r in rows(doc, "items") {
        let Some(i) = id(&r, "item") else {
            out.push(Doc::new());
            continue;
        };
        if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(i) {
            e.insert(get(k, ctx, tx, "Item", i).await?);
        }
        out.push(cache[&i].clone());
    }
    Ok(out)
}

/// The rows Canada's taxes take: (account number, description).
fn row_accounts(side: Side) -> [(&'static str, &'static str); 3] {
    match side {
        Side::Selling => [(GST_PAYABLE, "GST/HST"), (PST_PAYABLE, "PST"), (QST_PAYABLE, "QST")],
        Side::Buying => [(GST_RECOVERABLE, "GST/HST"), ("", "PST"), (QST_RECOVERABLE, "QST")],
    }
}

pub struct Canada;

#[async_trait]
impl Localization for Canada {
    fn package(&self) -> &'static str {
        crate::PACKAGE
    }

    async fn determine(&self, k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()> {
        if doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 || id(doc, "tax_template").is_some() || ent.field("taxes").is_none() {
            return Ok(());
        }
        let Some((plan, company)) = plan(k, ctx, tx, doc, side).await? else { return Ok(()) };
        let items = items_of(k, ctx, tx, doc).await?;
        if items.is_empty() {
            return Ok(());
        }
        let cid = id(&company, "id").ok_or_else(|| Error::internal("company without id"))?;
        let taxes: Vec<(Decimal, Decimal, Decimal, &str, Option<&str>)> = items.iter().map(|i| line_taxes(&plan, i)).collect();
        let accounts = row_accounts(side);
        let needed = [true, taxes.iter().any(|t| t.1 > Decimal::ZERO), taxes.iter().any(|t| t.2 > Decimal::ZERO)];
        let mut ids = Vec::new();
        for ((number, _), need) in accounts.iter().zip(needed) {
            ids.push(if need && !number.is_empty() { Some(account(k, ctx, tx, cid, number).await?) } else { None });
        }
        let all: Vec<String> = {
            let mut v = Vec::new();
            for (number, _) in accounts.iter().filter(|(n, _)| !n.is_empty()) {
                if let Ok(a) = account(k, ctx, tx, cid, number).await {
                    v.push(a);
                }
            }
            v
        };
        let mut rows_now = rows(doc, "taxes");
        let current: Vec<String> = rows_now.iter().filter_map(|r| s(r, "account").filter(|a| all.iter().any(|x| x == a)).map(str::to_string)).collect();
        let wanted: Vec<String> = ids.iter().flatten().cloned().collect();
        if current == wanted {
            return Ok(());
        }
        // Canada's rows, in order, then any others the user added.
        rows_now.retain(|r| s(r, "account").is_none_or(|a| !all.iter().any(|x| x == a)));
        let home_rate = gst_hst(&plan.home, &plan.on);
        let mut ours: Vec<Value> = Vec::new();
        for (i, acc) in ids.iter().enumerate() {
            let Some(acc) = acc else { continue };
            let rate = match i {
                0 => home_rate,
                1 => pst(&plan.home, &plan.on).unwrap_or_default(),
                _ => Decimal::from_str(QST).unwrap_or_default(),
            };
            ours.push(json!({"id": Uuid::now_v7().to_string(), "charge_type": "On Net Total", "account": acc, "description": accounts[i].1, "rate": rate.to_string()}));
        }
        ours.extend(rows_now.into_iter().map(Value::Object));
        set(doc, "taxes", Value::Array(ours));
        Ok(())
    }

    async fn line_rates(
        &self,
        k: &Kernel,
        ctx: &Ctx,
        tx: &mut Tx,
        doc: &Doc,
        lines: &[Doc],
        items: &[Doc],
        taxes: &[Doc],
        rates: &mut [Vec<Doc>],
        side: Side,
    ) -> Result<()> {
        let Some((plan, company)) = plan(k, ctx, tx, doc, side).await? else { return Ok(()) };
        let Some(cid) = id(&company, "id") else { return Ok(()) };
        let mut ids: Vec<Option<String>> = Vec::new();
        for (number, _) in row_accounts(side) {
            ids.push(if number.is_empty() { None } else { account(k, ctx, tx, cid, number).await.ok() });
        }
        let present = |a: &Option<String>| a.as_deref().is_some_and(|a| taxes.iter().any(|t| s(t, "account") == Some(a)));
        for (i, (line, item)) in lines.iter().zip(items).enumerate() {
            if id(line, "item").is_none() {
                continue;
            }
            let (gst, pst, qst, category, reason) = line_taxes(&plan, item);
            for (acc, rate) in ids.iter().zip([gst, pst, qst]) {
                if !present(acc) {
                    continue;
                }
                let acc = acc.clone().unwrap_or_default();
                let Value::Object(entry) = json!({"tax_account": acc, "rate": rate.to_string(), "category": category, "exemption_reason": reason}) else {
                    continue;
                };
                rates[i].retain(|r| s(r, "tax_account") != Some(acc.as_str()));
                rates[i].push(entry);
            }
        }
        Ok(())
    }

    async fn setup_company(&self, hc: &mut HookCtx<'_>, company: Uuid, accounts: &HashMap<&str, String>) -> Result<()> {
        let sctx = sys(hc.ctx);
        let co = hc.kernel.get_tx(&sctx, hc.tx, "Company", company, false).await?;
        if s(&co, "country_code") != Some("CA") {
            return Ok(());
        }
        for (number, name, parent, root) in ADDED {
            let Some(parent) = accounts.get(parent) else { continue };
            let Value::Object(a) = json!({"account_name": name, "account_number": number, "company": company.to_string(), "is_group": false, "root_type": root,
                                          "account_type": "Tax", "parent_account": parent, "account_currency": co.get("default_currency")})
            else {
                continue;
            };
            hc.kernel.insert_tx(&sctx, hc.tx, "Account", a).await?;
        }
        Ok(())
    }
}

// ───────────────────────────── returns ─────────────────────────────

/// The GST/HST return (the CRA's GST34 lines) and the QST return, from submitted invoices.
pub struct CaReports;

const DEFS: &[(&str, &str)] = &[("ca_gst_hst_return", "GST/HST Return"), ("ca_qst_return", "QST Return")];

async fn tax_total(tx: &mut Tx, tenant: Uuid, company: Uuid, entity: &str, account: Uuid, from: chrono::NaiveDate, to: chrono::NaiveDate) -> Result<Decimal> {
    let parent = ferrum_meta::table_for(entity);
    let taxes = ferrum_kernel::ddl::q(&ferrum_meta::child_table(&parent, "taxes"));
    let parent = ferrum_kernel::ddl::q(&parent);
    let v: Option<Decimal> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select sum(t.base_tax_amount) from {parent} i join {taxes} t on t.tenant_id = i.tenant_id and t.parent_id = i.id \
         where i.tenant_id = $1 and i.company = $2 and i.docstatus = 1 and i.posting_date between $3 and $4 and t.account = $5"
    )))
    .bind(tenant)
    .bind(company)
    .bind(from)
    .bind(to)
    .bind(account)
    .fetch_one(&mut **tx)
    .await?;
    Ok(v.unwrap_or_default())
}

#[async_trait]
impl ReportSource for CaReports {
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
        let acc = async |tx: &mut Tx, number: &str| -> Result<Option<Uuid>> {
            let v: Option<Uuid> = sqlx::query_scalar("select id from e_account where tenant_id = $1 and company = $2 and account_number = $3")
                .bind(ctx.tenant)
                .bind(co.id)
                .bind(number)
                .fetch_optional(&mut **tx)
                .await?;
            Ok(v)
        };
        let (collected, recovered) = if name == "ca_gst_hst_return" { (GST_PAYABLE, GST_RECOVERABLE) } else { (QST_PAYABLE, QST_RECOVERABLE) };
        let sum = async |tx: &mut Tx, entity: &str, number: &str| -> Result<Decimal> {
            match acc(tx, number).await? {
                Some(a) => tax_total(tx, ctx.tenant, co.id, entity, a, from, to).await,
                None => Ok(Decimal::ZERO),
            }
        };
        let tax = sum(&mut tx, "SalesInvoice", collected).await?;
        let credits = sum(&mut tx, "PurchaseInvoice", recovered).await?;
        let sales: Option<Decimal> = sqlx::query_scalar(
            "select sum(base_net_total) from e_sales_invoice where tenant_id = $1 and company = $2 and docstatus = 1 and posting_date between $3 and $4",
        )
        .bind(ctx.tenant)
        .bind(co.id)
        .bind(from)
        .bind(to)
        .fetch_one(&mut *tx)
        .await?;
        tx.rollback().await?;
        // The GST34's lines (the QST return's follow the same steps).
        let rows = vec![
            json!({"line": "101", "label": "Sales and other revenue", "amount": money(sales.unwrap_or_default())}),
            json!({"line": "103", "label": if name == "ca_gst_hst_return" { "GST/HST collected" } else { "QST collected" }, "amount": money(tax)}),
            json!({"line": "106", "label": if name == "ca_gst_hst_return" { "Input tax credits" } else { "Input tax refunds" }, "amount": money(credits)}),
            json!({"line": "109", "label": "Net tax (negative: a refund)", "amount": money(tax - credits)}),
        ];
        Ok(
            json!({"name": name, "label": label, "columns": [col("line", "Line", "data"), col("label", "", "data"), col("amount", "Amount", "currency")], "rows": rows,
                  "meta": {"from_date": from, "to_date": to}}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_by_province_and_day() {
        let d = |v: &str| Decimal::from_str(v).unwrap();
        assert_eq!((gst_hst("ON", "2026-10-31"), gst_hst("AB", "2026-10-31"), gst_hst("QC", "2026-10-31")), (d("13"), d("5"), d("5")));
        assert_eq!((gst_hst("NS", "2025-03-31"), gst_hst("NS", "2025-04-01")), (d("15"), d("14")), "Nova Scotia's HST fell on 1 April 2025");
        assert_eq!((pst("BC", "2026-10-31"), pst("SK", "2026-10-31"), pst("ON", "2026-10-31")), (Some(d("7")), Some(d("6")), None));
        assert_eq!((province("Ontario"), province(" qc "), province("Québec"), province("Texas")), (Some("ON"), Some("QC"), Some("QC"), None));
        assert!(is_hst("PE", "2026-01-01") && !is_hst("BC", "2026-01-01"));
    }

    #[test]
    fn what_each_line_takes() {
        let d = |v: &str| Decimal::from_str(v).unwrap();
        let item = |v: Value| v.as_object().cloned().unwrap();
        let widget = item(json!({"ca_kind": "Goods"}));
        let plan = |to: &str| Plan {
            side: Side::Selling,
            on: "2026-10-31".into(),
            home: "ON".into(),
            goods: if to == "US" { Place::Abroad } else { Place::Province(to.into()) },
            services: Place::Province("ON".into()),
            pst_registered: vec!["BC".into()],
            qst_registered: true,
            resale: false,
        };
        assert_eq!(line_taxes(&plan("ON"), &widget), (d("13"), d("0"), d("0"), "Standard", None));
        assert_eq!(line_taxes(&plan("BC"), &widget), (d("5"), d("7"), d("0"), "Standard", None));
        assert_eq!(line_taxes(&plan("SK"), &widget), (d("5"), d("0"), d("0"), "Standard", None), "not registered for Saskatchewan's PST");
        assert_eq!(line_taxes(&plan("QC"), &widget), (d("5"), d("0"), d("9.975"), "Standard", None));
        assert_eq!(line_taxes(&plan("US"), &widget).3, "Export");
        assert_eq!(line_taxes(&plan("ON"), &item(json!({"ca_kind": "Goods", "ca_gst_category": "Zero Rated"}))).0, d("0"));
        let resale = Plan { resale: true, ..plan("BC") };
        assert_eq!(line_taxes(&resale, &widget), (d("5"), d("0"), d("0"), "Standard", None), "bought for resale: no PST");
    }
}
