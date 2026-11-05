//! Company setup (standard chart of accounts, cost center, warehouses; a country's own accounts
//! and tax templates from its localization) and fiscal years with monthly accounting periods that
//! can be closed and reopened.

use crate::util::{date, id, rows, s, sys};
use async_trait::async_trait;
use chrono::{Datelike, NaiveDate};
use ferrum_kernel::{Doc, DocHook, Error, Filter, HookCtx, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

/// (number, name, parent name, is_group, root_type, account_type)
const COA: &[(&str, &str, &str, bool, &str, &str)] = &[
    ("1000", "Assets", "", true, "Asset", ""),
    ("1100", "Current Assets", "Assets", true, "Asset", ""),
    ("1110", "Accounts Receivable", "Current Assets", true, "Asset", ""),
    ("1111", "Debtors", "Accounts Receivable", false, "Asset", "Receivable"),
    ("1120", "Bank Accounts", "Current Assets", true, "Asset", ""),
    ("1121", "Primary Bank", "Bank Accounts", false, "Asset", "Bank"),
    ("1130", "Cash In Hand", "Current Assets", true, "Asset", ""),
    ("1131", "Cash", "Cash In Hand", false, "Asset", "Cash"),
    ("1140", "Stock Assets", "Current Assets", true, "Asset", ""),
    ("1141", "Stock In Hand", "Stock Assets", false, "Asset", "Stock"),
    ("1142", "Work In Progress", "Stock Assets", false, "Asset", "Stock"),
    ("1150", "Tax Assets", "Current Assets", true, "Asset", ""),
    ("1151", "Input Tax", "Tax Assets", false, "Asset", "Tax"),
    ("1152", "TDS Receivable", "Tax Assets", false, "Asset", "Tax"),
    ("1160", "Advances to Suppliers", "Current Assets", false, "Asset", ""),
    ("1170", "Prepaid Expenses", "Current Assets", false, "Asset", ""),
    ("1180", "Accrued Income", "Current Assets", false, "Asset", ""),
    ("1200", "Fixed Assets", "Assets", true, "Asset", ""),
    ("1210", "Plant and Machinery", "Fixed Assets", false, "Asset", "Fixed Asset"),
    ("1220", "Furniture and Fixtures", "Fixed Assets", false, "Asset", "Fixed Asset"),
    ("1230", "Computers", "Fixed Assets", false, "Asset", "Fixed Asset"),
    ("2000", "Liabilities", "", true, "Liability", ""),
    ("2100", "Current Liabilities", "Liabilities", true, "Liability", ""),
    ("2110", "Accounts Payable", "Current Liabilities", true, "Liability", ""),
    ("2111", "Creditors", "Accounts Payable", false, "Liability", "Payable"),
    ("2120", "Stock Liabilities", "Current Liabilities", true, "Liability", ""),
    ("2121", "Stock Received But Not Billed", "Stock Liabilities", false, "Liability", "Stock Received But Not Billed"),
    ("2130", "Duties and Taxes", "Current Liabilities", true, "Liability", ""),
    ("2131", "Output Tax", "Duties and Taxes", false, "Liability", "Tax"),
    ("2132", "TDS Payable", "Duties and Taxes", false, "Liability", "Tax"),
    ("2140", "Customer Advances", "Current Liabilities", false, "Liability", ""),
    ("2150", "Deferred Revenue", "Current Liabilities", false, "Liability", ""),
    ("2160", "Accrued Expenses", "Current Liabilities", false, "Liability", ""),
    ("2200", "Loans", "Liabilities", true, "Liability", ""),
    ("2210", "Secured Loans", "Loans", false, "Liability", ""),
    ("3000", "Equity", "", true, "Equity", ""),
    ("3100", "Capital Stock", "Equity", false, "Equity", "Equity"),
    ("3200", "Retained Earnings", "Equity", false, "Equity", "Equity"),
    ("3300", "Opening Balance Equity", "Equity", false, "Equity", "Temporary"),
    ("4000", "Income", "", true, "Income", ""),
    ("4100", "Direct Income", "Income", true, "Income", ""),
    ("4110", "Sales", "Direct Income", false, "Income", "Income"),
    ("4120", "Service Income", "Direct Income", false, "Income", "Income"),
    ("4200", "Indirect Income", "Income", true, "Income", ""),
    ("4210", "Other Income", "Indirect Income", false, "Income", "Income"),
    ("5000", "Expenses", "", true, "Expense", ""),
    ("5100", "Direct Expenses", "Expenses", true, "Expense", ""),
    ("5110", "Cost of Goods Sold", "Direct Expenses", false, "Expense", "Cost of Goods Sold"),
    ("5120", "Stock Adjustment", "Direct Expenses", false, "Expense", "Stock Adjustment"),
    ("5130", "Manufacturing Overheads Absorbed", "Direct Expenses", false, "Expense", "Expense"),
    ("5200", "Indirect Expenses", "Expenses", true, "Expense", ""),
    ("5210", "Administrative Expenses", "Indirect Expenses", false, "Expense", "Expense"),
    ("5220", "Salaries", "Indirect Expenses", false, "Expense", "Expense"),
    ("5230", "Rent", "Indirect Expenses", false, "Expense", "Expense"),
    ("5240", "Utilities", "Indirect Expenses", false, "Expense", "Expense"),
    ("5250", "Freight and Forwarding", "Indirect Expenses", false, "Expense", "Expense"),
    ("5260", "Exchange Gain or Loss", "Indirect Expenses", false, "Expense", "Exchange Gain Loss"),
    ("5270", "Round Off", "Indirect Expenses", false, "Expense", "Round Off"),
    ("5280", "Bank Charges", "Indirect Expenses", false, "Expense", "Expense"),
];

/// The company's default links and the standard accounts they point at.
const DEFAULTS: &[(&str, &str)] = &[
    ("default_receivable_account", "Debtors"),
    ("default_payable_account", "Creditors"),
    ("default_income_account", "Sales"),
    ("default_expense_account", "Administrative Expenses"),
    ("default_deferred_revenue_account", "Deferred Revenue"),
    ("default_deferred_expense_account", "Prepaid Expenses"),
    ("default_cash_account", "Cash"),
    ("default_bank_account", "Primary Bank"),
    ("default_inventory_account", "Stock In Hand"),
    ("cogs_account", "Cost of Goods Sold"),
    ("srbnb_account", "Stock Received But Not Billed"),
    ("stock_adjustment_account", "Stock Adjustment"),
    ("round_off_account", "Round Off"),
    ("exchange_gain_loss_account", "Exchange Gain or Loss"),
    ("retained_earnings_account", "Retained Earnings"),
    ("opening_balance_account", "Opening Balance Equity"),
    ("overhead_absorbed_account", "Manufacturing Overheads Absorbed"),
];

/// One account of the standard chart, as a new record for `company`.
fn standard_account(company: Uuid, currency: &Value, entry: &(&str, &str, &str, bool, &str, &str), parent: Option<&String>, number: bool) -> Doc {
    let &(account_number, name, _, is_group, root, typ) = entry;
    let mut d = Doc::new();
    d.insert("account_name".into(), json!(name));
    if number {
        d.insert("account_number".into(), json!(account_number));
    }
    d.insert("company".into(), json!(company.to_string()));
    d.insert("is_group".into(), json!(is_group));
    d.insert("root_type".into(), json!(root));
    d.insert("account_currency".into(), currency.clone());
    if !typ.is_empty() {
        d.insert("account_type".into(), json!(typ));
    }
    // Borrowing is financing, which no account type says.
    if name == "Secured Loans" {
        d.insert("cash_flow_section".into(), json!("Financing"));
    }
    if let Some(p) = parent {
        d.insert("parent_account".into(), json!(p));
    }
    d
}

/// Creates the standard chart, cost center, warehouses and default links. Idempotent: a company
/// with accounts already is left to [`complete_chart`].
pub async fn setup_company(hc: &mut HookCtx<'_>, company: Uuid) -> Result<bool> {
    let k = hc.kernel;
    let sctx = sys(hc.ctx);
    let existing = k.find_tx(&sctx, hc.tx, "Account", &[Filter::eq("company", company.to_string())], None, 1).await?;
    if !existing.is_empty() {
        return Ok(false);
    }
    let co = k.get_tx(&sctx, hc.tx, "Company", company, false).await?;
    let currency = co.get("default_currency").cloned().unwrap_or(Value::Null);
    let mut ids: HashMap<&str, String> = HashMap::new();
    for entry in COA {
        let parent = (!entry.2.is_empty()).then(|| ids.get(entry.2)).flatten();
        let created = k.insert_tx(&sctx, hc.tx, "Account", standard_account(company, &currency, entry, parent, true)).await?;
        ids.insert(entry.1, s(&created, "id").unwrap_or_default().to_string());
    }
    let mut cc = Doc::new();
    cc.insert("cost_center_name".into(), json!("Main"));
    cc.insert("company".into(), json!(company.to_string()));
    let cc = k.insert_tx(&sctx, hc.tx, "CostCenter", cc).await?;
    // Warehouses are inventory's: a workspace with accounting alone (an HR connector, say) has none.
    if hc.ctx.schema.entity("Warehouse").is_some() {
        let mut group = Doc::new();
        group.insert("warehouse_name".into(), json!("All Warehouses"));
        group.insert("company".into(), json!(company.to_string()));
        group.insert("is_group".into(), json!(true));
        let group = k.insert_tx(&sctx, hc.tx, "Warehouse", group).await?;
        for (name, account) in [("Stores", "Stock In Hand"), ("Finished Goods", "Stock In Hand"), ("Work In Progress", "Work In Progress")] {
            let mut w = Doc::new();
            w.insert("warehouse_name".into(), json!(name));
            w.insert("company".into(), json!(company.to_string()));
            w.insert("parent_warehouse".into(), group.get("id").cloned().unwrap_or(Value::Null));
            w.insert("account".into(), json!(ids.get(account).cloned()));
            k.insert_tx(&sctx, hc.tx, "Warehouse", w).await?;
        }
    }
    let mut v = Doc::new();
    for &(field, account) in DEFAULTS {
        v.insert(field.into(), json!(ids.get(account).cloned()));
    }
    v.insert("default_cost_center".into(), cc.get("id").cloned().unwrap_or(Value::Null));
    k.set_values_tx(&sctx, hc.tx, "Company", company, v).await?;
    crate::localization::setup_company(hc, company, &ids).await?;
    Ok(true)
}

/// Brings an existing company's chart up to the standard one, so accounts a later release adds
/// (Deferred Revenue and Prepaid Expenses in 0.9.0, the accrual accounts in 0.10.0) reach companies
/// made before it: each standard account the company lacks by name is added under its standard
/// parent, where the company has that parent, without the standard number when another account
/// holds it; then each default link still empty is set to its standard account. Nothing the
/// company already has is changed. Answers the accounts added and how many links were set.
pub async fn complete_chart(hc: &mut HookCtx<'_>, company: Uuid) -> Result<(Vec<&'static str>, usize)> {
    let k = hc.kernel;
    let sctx = sys(hc.ctx);
    let rows = k.find_tx(&sctx, hc.tx, "Account", &[Filter::eq("company", company.to_string())], None, 100_000).await?;
    let mut ids: HashMap<String, String> = rows.iter().filter_map(|r| Some((s(r, "account_name")?.to_string(), s(r, "id")?.to_string()))).collect();
    let numbers: std::collections::HashSet<&str> = rows.iter().filter_map(|r| s(r, "account_number")).collect();
    let co = k.get_tx(&sctx, hc.tx, "Company", company, false).await?;
    let currency = co.get("default_currency").cloned().unwrap_or(Value::Null);
    let mut added = Vec::new();
    for entry in COA {
        if ids.contains_key(entry.1) {
            continue;
        }
        let parent = match entry.2 {
            "" => None,
            p => match ids.get(p) {
                Some(id) => Some(id.clone()),
                None => continue,
            },
        };
        let doc = standard_account(company, &currency, entry, parent.as_ref(), !numbers.contains(entry.0));
        let created = k.insert_tx(&sctx, hc.tx, "Account", doc).await?;
        ids.insert(entry.1.to_string(), s(&created, "id").unwrap_or_default().to_string());
        added.push(entry.1);
    }
    let mut v = Doc::new();
    for &(field, account) in DEFAULTS {
        if co.get(field).is_none_or(Value::is_null)
            && let Some(a) = ids.get(account)
        {
            v.insert(field.into(), json!(a));
        }
    }
    let linked = v.len();
    if linked > 0 {
        k.set_values_tx(&sctx, hc.tx, "Company", company, v).await?;
    }
    Ok((added, linked))
}

pub struct CompanyHook;

#[async_trait]
impl DocHook for CompanyHook {
    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        if old.is_none() {
            let company = id(doc, "id").ok_or_else(|| Error::internal("company without id"))?;
            setup_company(hc, company).await?;
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let company = id(doc, "id").ok_or_else(|| Error::internal("company without id"))?;
        if name == "recognise_deferred" {
            return Ok(Some(crate::deferred::action(hc.kernel, hc.ctx, hc.tx, company, params).await?));
        }
        if name != "setup_chart_of_accounts" {
            return Ok(None);
        }
        if setup_company(hc, company).await? {
            return Ok(Some(json!({"created": true, "message": "Standard chart of accounts created"})));
        }
        let (added, linked) = complete_chart(hc, company).await?;
        let message = match (added.is_empty(), linked) {
            (true, 0) => "This company's chart already has every standard account".to_string(),
            (true, n) => format!("{n} default account{} set", if n == 1 { "" } else { "s" }),
            (false, n) => format!("Added {} from the standard chart; {n} default account{} set", added.join(", "), if n == 1 { "" } else { "s" }),
        };
        Ok(Some(json!({"created": false, "added": added, "defaults_set": linked, "message": message})))
    }
}

fn month_starts(start: NaiveDate, end: NaiveDate) -> Vec<(NaiveDate, NaiveDate)> {
    let mut out = Vec::new();
    let mut cur = start;
    while cur <= end {
        let next = if cur.month() == 12 { NaiveDate::from_ymd_opt(cur.year() + 1, 1, 1) } else { NaiveDate::from_ymd_opt(cur.year(), cur.month() + 1, 1) }
            .unwrap_or(end);
        let period_end = (next - chrono::Duration::days(1)).min(end);
        out.push((cur, period_end));
        if next <= cur {
            break;
        }
        cur = next;
    }
    out
}

pub struct FiscalYearHook;

#[async_trait]
impl DocHook for FiscalYearHook {
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let (start, end) = (date(doc, "start_date")?, date(doc, "end_date")?);
        if (end - start).num_days() > 400 {
            return Err(Error::field("end_date", "a fiscal year cannot be longer than 400 days"));
        }
        Ok(())
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, _old: Option<&Doc>) -> Result<()> {
        let company = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        for (a, b) in month_starts(date(doc, "start_date")?, date(doc, "end_date")?) {
            sqlx::query("insert into accounting_period (tenant_id, company_id, period_start, period_end) values ($1, $2, $3, $4) on conflict do nothing")
                .bind(hc.ctx.tenant)
                .bind(company)
                .bind(a)
                .bind(b)
                .execute(&mut **hc.tx)
                .await
                .map_err(|e| match Error::from(e) {
                    Error::Conflict(_) => Error::invalid("these dates overlap accounting periods of another fiscal year"),
                    other => other,
                })?;
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let status = match name {
            "close_period" => "closed",
            "reopen_period" => "open",
            _ => return Ok(None),
        };
        if !hc.ctx.has_role("Accounts Manager") {
            return Err(Error::Forbidden("only an Accounts Manager can close or reopen periods".into()));
        }
        let company = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let month =
            params.get("period_start").and_then(Value::as_str).ok_or_else(|| Error::field("period_start", "give the first day of the month, YYYY-MM-DD"))?;
        let start = ferrum_kernel::value::parse_date(month).map_err(|m| Error::field("period_start", m))?;
        let n = sqlx::query(
            "update accounting_period set status = $4, closed_by = case when $4 = 'closed' then $5 else null end, \
             closed_at = case when $4 = 'closed' then now() else null end where tenant_id = $1 and company_id = $2 and period_start = $3",
        )
        .bind(hc.ctx.tenant)
        .bind(company)
        .bind(start)
        .bind(status)
        .bind((!hc.ctx.user.is_nil()).then_some(hc.ctx.user))
        .execute(&mut **hc.tx)
        .await?
        .rows_affected();
        if n == 0 {
            return Err(Error::NotFound("accounting period starting on that date".into()));
        }
        let _ = rows(doc, "");
        Ok(Some(json!({"period_start": start, "status": status})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_cover_the_year() {
        let m = month_starts(NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(), NaiveDate::from_ymd_opt(2027, 3, 31).unwrap());
        assert_eq!(m.len(), 12);
        assert_eq!(m[0].1, NaiveDate::from_ymd_opt(2026, 4, 30).unwrap());
        assert_eq!(m[11].1, NaiveDate::from_ymd_opt(2027, 3, 31).unwrap());
        let parents: Vec<&str> = COA.iter().map(|a| a.1).collect();
        for a in COA {
            assert!(a.2.is_empty() || parents.contains(&a.2), "{} has unknown parent {}", a.1, a.2);
        }
    }
}
