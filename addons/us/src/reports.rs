//! The US reports: sales tax by state and jurisdiction (for each state's return), economic nexus
//! (sales into each state against its threshold) and Forms 1099 (payments to suppliers in a
//! calendar year).

use crate::tax::{Counted, Period, SALES_TAX, STATES, State, state, states};
use crate::util::money;
use async_trait::async_trait;
use chrono::{Datelike, Duration, NaiveDate};
use ferrum_domain::reports::{Params, ReportSource, col, param_def, period, read_tx, resolve_company, today};
use ferrum_kernel::{Ctx, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

pub struct UsReports;

const DEFS: &[(&str, &str, &str, &[&str])] = &[
    ("us_sales_tax", "Sales Tax by Jurisdiction", "SalesInvoice", &["company", "from_date", "to_date"]),
    ("us_economic_nexus", "Economic Nexus", "SalesInvoice", &["company", "as_of"]),
    ("us_1099", "Forms 1099", "PaymentEntry", &["company", "as_of"]),
];

/// Form 1099's reporting threshold for a year's payments: $600 until 2025, $2,000 for payments
/// made from 2026 (One Big Beautiful Bill Act, section 70433; indexed for inflation after 2026,
/// which the IRS announces); royalties $10.
pub fn threshold(form: &str, year: i32) -> Decimal {
    if form == "MISC Royalties" {
        Decimal::from(10)
    } else if year >= 2026 {
        Decimal::from(2000)
    } else {
        Decimal::from(600)
    }
}

/// Where on its form a payment goes.
pub fn form_box(form: &str) -> &'static str {
    match form {
        "NEC" => "1099-NEC box 1",
        "MISC Rents" => "1099-MISC box 1",
        "MISC Royalties" => "1099-MISC box 2",
        "MISC Other Income" => "1099-MISC box 3",
        "MISC Medical" => "1099-MISC box 6",
        "MISC Legal" => "1099-MISC box 10",
        _ => "",
    }
}

/// A TIN shown by its last four digits.
pub fn masked(tin: &str, kind: Option<&str>) -> String {
    let digits: String = tin.chars().filter(char::is_ascii_digit).collect();
    if digits.len() < 4 {
        return String::new();
    }
    let last = &digits[digits.len() - 4..];
    if kind == Some("EIN") { format!("**-***{last}") } else { format!("***-**-{last}") }
}

/// The sales figures of a state over a window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sales {
    pub gross: Decimal,
    pub resale: Decimal,
    pub not_taxable: Decimal,
    pub exempt: Decimal,
    pub transactions: i64,
}

impl Sales {
    pub fn counted(&self, c: Counted) -> Decimal {
        match c {
            Counted::Gross => self.gross,
            Counted::Retail => self.gross - self.resale,
            Counted::Taxable => self.gross - self.resale - self.not_taxable - self.exempt,
        }
    }
}

/// Whether sales over a window reach a state's threshold.
pub fn over(st: &State, sales: &Sales) -> bool {
    if st.nexus == 0 {
        return false;
    }
    let amount = sales.counted(st.counted);
    let limit = Decimal::from(st.nexus);
    let by_amount = if st.over { amount > limit } else { amount >= limit };
    let by_count = st.transactions.is_some_and(|t| sales.transactions >= i64::from(t));
    if st.both { by_amount && by_count } else { by_amount || by_count }
}

pub fn threshold_text(st: &State) -> String {
    let amount =
        format!("${}", st.nexus.to_string().as_bytes().rchunks(3).rev().map(|c| std::str::from_utf8(c).unwrap_or_default()).collect::<Vec<_>>().join(","));
    let amount = if st.over { format!("more than {amount}") } else { amount };
    match (st.transactions, st.both) {
        (Some(t), true) => format!("{amount} and {t} transactions"),
        (Some(t), false) => format!("{amount} or {t} transactions"),
        _ => amount,
    }
}

async fn sales_tax_account(tx: &mut Tx, ctx: &Ctx, company: Uuid) -> Result<Option<Uuid>> {
    Ok(sqlx::query_scalar("select id from e_account where tenant_id = $1 and company = $2 and account_number = $3")
        .bind(ctx.tenant)
        .bind(company)
        .bind(SALES_TAX)
        .fetch_optional(&mut **tx)
        .await?)
}

/// (state, jurisdiction, category, reason) → (taxable, tax, invoices) over submitted invoices.
type Groups = Vec<(Option<String>, Option<String>, String, Option<String>, Decimal, Decimal, i64)>;

async fn groups(tx: &mut Tx, ctx: &Ctx, company: Uuid, account: Uuid, from: NaiveDate, to: NaiveDate) -> Result<Groups> {
    let parent = ferrum_meta::table_for("SalesInvoice");
    let breakdown = ferrum_kernel::ddl::q(&ferrum_meta::child_table(&parent, "tax_breakdown"));
    let parent = ferrum_kernel::ddl::q(&parent);
    let rows: Vec<(Option<String>, Option<String>, Option<String>, Option<String>, Option<Decimal>, Option<Decimal>, i64)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select i.us_state, i.us_jurisdiction, b.category, b.exemption_reason, sum(b.base_taxable_amount), sum(b.base_tax_amount), \
                count(distinct i.id) filter (where not coalesce(i.is_return, false)) \
         from {parent} i join {breakdown} b on b.tenant_id = i.tenant_id and b.parent_id = i.id \
         where i.tenant_id = $1 and i.company = $2 and i.docstatus = 1 and i.posting_date between $3 and $4 and b.account = $5 \
         group by 1, 2, 3, 4"
        )))
        .bind(ctx.tenant)
        .bind(company)
        .bind(from)
        .bind(to)
        .bind(account)
        .fetch_all(&mut **tx)
        .await?;
    Ok(rows.into_iter().map(|(st, j, c, r, a, t, n)| (st, j, c.unwrap_or_default(), r, a.unwrap_or_default(), t.unwrap_or_default(), n)).collect())
}

/// Each state's sales over a window, from the invoices' tax breakdowns.
async fn by_state(tx: &mut Tx, ctx: &Ctx, company: Uuid, account: Uuid, from: NaiveDate, to: NaiveDate) -> Result<HashMap<String, Sales>> {
    let parent = ferrum_kernel::ddl::q(&ferrum_meta::table_for("SalesInvoice"));
    let counts: Vec<(Option<String>, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select us_state, count(*) from {parent} where tenant_id = $1 and company = $2 and docstatus = 1 and posting_date between $3 and $4 \
         and not coalesce(is_return, false) and us_state is not null group by 1"
    )))
    .bind(ctx.tenant)
    .bind(company)
    .bind(from)
    .bind(to)
    .fetch_all(&mut **tx)
    .await?;
    let mut out: HashMap<String, Sales> = HashMap::new();
    for (st, _, category, reason, amount, _, _) in groups(tx, ctx, company, account, from, to).await? {
        let Some(st) = st else { continue };
        let e = out.entry(st).or_default();
        e.gross += amount;
        let reason = reason.unwrap_or_default();
        if category == "Exempt" {
            if reason.starts_with("Not taxable") {
                e.not_taxable += amount;
            } else if reason.ends_with("(Resale)") {
                e.resale += amount;
            } else {
                e.exempt += amount;
            }
        }
    }
    for (st, n) in counts {
        if let Some(st) = st {
            out.entry(st).or_default().transactions = n;
        }
    }
    Ok(out)
}

fn year_start(y: i32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, 1, 1).unwrap_or_default()
}

fn year_end(y: i32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, 12, 31).unwrap_or_default()
}

#[async_trait]
impl ReportSource for UsReports {
    fn catalog(&self) -> Vec<Value> {
        DEFS.iter()
            .map(|(name, label, entity, params)| {
                let params: Vec<Value> = params.iter().map(|p| param_def(p)).collect();
                json!({"name": name, "label": label, "module": "accounting", "entity": entity, "scoped": false, "params": params})
            })
            .collect()
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
        let label = DEFS.iter().find(|(n, ..)| *n == name).map(|(_, l, ..)| *l).ok_or_else(|| Error::NotFound(format!("report {name}")))?;
        let p = Params { raw };
        let mut tx = read_tx(k, ctx).await?;
        let co = resolve_company(&mut tx, ctx, &p).await?;
        let out = match name {
            "us_sales_tax" => {
                let (from, to) = period(&p, &co)?;
                let mut rows: BTreeMap<(String, String), [Decimal; 5]> = BTreeMap::new();
                if let Some(acc) = sales_tax_account(&mut tx, ctx, co.id).await? {
                    for (st, j, category, _, amount, tax, _) in groups(&mut tx, ctx, co.id, acc, from, to).await? {
                        let Some(st) = st else { continue };
                        let r = rows.entry((st, j.unwrap_or_default())).or_default();
                        r[0] += amount;
                        match category.as_str() {
                            "Standard" | "Reduced" => r[3] += amount,
                            "Exempt" => r[1] += amount,
                            _ => r[2] += amount,
                        }
                        r[4] += tax;
                    }
                }
                let rows: Vec<Value> = rows
                    .into_iter()
                    .map(|((st, j), r)| {
                        json!({"state": st, "jurisdiction": j, "gross": money(r[0]), "exempt": money(r[1]), "not_collected": money(r[2]), "taxable": money(r[3]), "tax": money(r[4])})
                    })
                    .collect();
                json!({"name": name, "label": label, "rows": rows, "meta": {"from_date": from, "to_date": to},
                       "columns": [col("state", "State", "data"), col("jurisdiction", "Jurisdiction", "data"), col("gross", "Gross Sales", "currency"),
                                   col("exempt", "Exempt and Not Taxable", "currency"), col("not_collected", "Not Collected", "currency"),
                                   col("taxable", "Taxable Sales", "currency"), col("tax", "Tax Collected", "currency")]})
            }
            "us_economic_nexus" => {
                let as_of = p.date("as_of")?.unwrap_or_else(today);
                let y = as_of.year();
                let company = k.get_tx(ctx, &mut tx, "Company", co.id, false).await?;
                let registered = states(company.get("us_registered_states").and_then(Value::as_str).unwrap_or_default());
                let windows = match sales_tax_account(&mut tx, ctx, co.id).await? {
                    Some(acc) => [
                        by_state(&mut tx, ctx, co.id, acc, year_start(y), as_of).await?,
                        by_state(&mut tx, ctx, co.id, acc, year_start(y - 1), year_end(y - 1)).await?,
                        by_state(&mut tx, ctx, co.id, acc, as_of - Duration::days(364), as_of).await?,
                    ],
                    None => Default::default(),
                };
                let mut rows = Vec::new();
                for st in STATES {
                    let get = |w: usize| windows[w].get(st.code).cloned().unwrap_or_default();
                    let measured: Vec<(String, Sales)> = match st.period {
                        Period::Calendar => vec![(format!("{y} to date"), get(0)), (format!("{}", y - 1), get(1))],
                        Period::Previous => vec![(format!("{}", y - 1), get(1))],
                        Period::Rolling => vec![("the last 12 months".to_string(), get(2))],
                    };
                    let reached = measured.iter().find(|(_, s)| over(st, s));
                    let shown = reached.or_else(|| measured.iter().max_by_key(|(_, s)| s.counted(st.counted))).cloned().unwrap_or_default();
                    let is_registered = registered.contains(&st.code);
                    if shown.1.gross.is_zero() && shown.1.transactions == 0 && !is_registered {
                        continue;
                    }
                    let status = if st.nexus == 0 {
                        "No sales tax"
                    } else if is_registered {
                        "Registered"
                    } else if reached.is_some() {
                        "Threshold reached: register"
                    } else {
                        "Below the threshold"
                    };
                    let counted = match st.counted {
                        Counted::Gross => "gross",
                        Counted::Retail => "retail",
                        Counted::Taxable => "taxable",
                    };
                    rows.push(json!({"state": st.code, "name": st.name, "threshold": if st.nexus == 0 { String::new() } else { threshold_text(st) },
                                     "period": shown.0, "counted": counted, "sales": money(shown.1.counted(st.counted)), "transactions": shown.1.transactions, "status": status}));
                }
                json!({"name": name, "label": label, "rows": rows, "meta": {"as_of": as_of},
                       "columns": [col("state", "State", "data"), col("name", "", "data"), col("threshold", "Threshold", "data"), col("period", "Measured Over", "data"),
                                   col("counted", "Sales Counted", "data"), col("sales", "Sales", "currency"), col("transactions", "Transactions", "int"),
                                   col("status", "Status", "data")]})
            }
            _ => {
                let as_of = p.date("as_of")?.unwrap_or_else(today);
                let y = as_of.year();
                let paid: Vec<(Uuid, Option<String>, Option<String>, Option<String>, Option<NaiveDate>, Option<bool>, Option<Decimal>)> = sqlx::query_as(
                    "select s.id, s.supplier_name, s.us_1099_form, s.us_tin_type, s.us_w9_received, s.us_backup_withholding, sum(p.base_paid_amount) \
                     from e_payment_entry p join e_supplier s on s.tenant_id = p.tenant_id and s.id = p.supplier \
                     where p.tenant_id = $1 and p.company = $2 and p.docstatus = 1 and p.payment_type = 'Pay' and p.posting_date between $3 and $4 \
                     and not coalesce(p.us_card_payment, false) and s.us_1099_form is not null and s.us_1099_form <> '' \
                     group by 1, 2, 3, 4, 5, 6 order by 2",
                )
                .bind(ctx.tenant)
                .bind(co.id)
                .bind(year_start(y))
                .bind(year_end(y))
                .fetch_all(&mut *tx)
                .await?;
                let mut rows = Vec::new();
                for (sid, supplier, form, tin_type, w9, backup, amount) in paid {
                    let form = form.unwrap_or_default();
                    let amount = amount.unwrap_or_default();
                    let tin = k.secret(&mut tx, ctx.tenant, "Supplier", sid, "us_tin").await?;
                    let limit = threshold(&form, y);
                    let mut notes: Vec<&str> = Vec::new();
                    if tin.as_deref().is_none_or(|t| t.chars().filter(char::is_ascii_digit).count() != 9) {
                        notes.push("No valid TIN: request Form W-9 (backup withholding applies without one)");
                    }
                    if w9.is_none() {
                        notes.push("No W-9 on file");
                    }
                    rows.push(json!({
                        "supplier": supplier, "form": form, "box": form_box(&form), "tin": tin.as_deref().map(|t| masked(t, tin_type.as_deref())),
                        "paid": money(amount), "threshold": money(limit), "reportable": amount >= limit, "backup_withholding": backup.unwrap_or(false),
                        "notes": notes.join("; "),
                    }));
                }
                json!({"name": name, "label": label, "rows": rows, "meta": {"year": y},
                       "columns": [col("supplier", "Supplier", "data"), col("form", "Form", "data"), col("box", "Box", "data"), col("tin", "TIN", "data"),
                                   col("paid", "Paid in the Year", "currency"), col("threshold", "Threshold", "currency"), col("reportable", "Reportable", "check"),
                                   col("backup_withholding", "Backup Withholding", "check"), col("notes", "To Do", "data")]})
            }
        };
        tx.rollback().await?;
        Ok(out)
    }
}

/// A state as the reports name it.
pub fn state_name(code: &str) -> &'static str {
    state(code).map(|s| s.name).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_and_forms() {
        assert_eq!(
            (threshold("NEC", 2025), threshold("NEC", 2026), threshold("MISC Royalties", 2026)),
            (Decimal::from(600), Decimal::from(2000), Decimal::from(10))
        );
        assert_eq!(form_box("MISC Legal"), "1099-MISC box 10");
        assert_eq!(
            (masked("12-3456789", Some("EIN")), masked("123-45-6789", Some("SSN")), masked("12", None)),
            ("**-***6789".into(), "***-**-6789".into(), String::new())
        );
        let ny = state("NY").unwrap();
        assert_eq!(threshold_text(ny), "$500,000 and 101 transactions");
        assert_eq!(threshold_text(state("WA").unwrap()), "more than $100,000");
        let s = |gross: i64, n: i64| Sales { gross: Decimal::from(gross), transactions: n, ..Default::default() };
        assert!(!over(ny, &s(600_000, 50)) && over(ny, &s(600_000, 101)), "New York needs both");
        let nv = state("NV").unwrap();
        assert!(over(nv, &s(10, 200)) && over(nv, &s(100_000, 1)) && !over(nv, &s(99_999, 199)));
        let wa = state("WA").unwrap();
        assert!(!over(wa, &s(100_000, 0)) && over(wa, &s(100_001, 0)), "Washington's is exceeded, not met");
        let fl = state("FL").unwrap();
        let mixed =
            Sales { gross: Decimal::from(150_000), resale: Decimal::from(30_000), not_taxable: Decimal::from(25_000), exempt: Decimal::ZERO, transactions: 3 };
        assert!(!over(fl, &mixed), "Florida counts taxable sales: 95,000");
        assert_eq!(state_name("TX"), "Texas");
    }
}
