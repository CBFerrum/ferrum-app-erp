//! Company setup (standard chart of accounts, cost center, warehouses; a country's own accounts
//! and tax templates from its localization) and fiscal years with monthly accounting periods that
//! can be closed and reopened.

use crate::util::{date, id, rows, s, sys};
use async_trait::async_trait;
use chrono::{Datelike, Duration, Months, NaiveDate};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, Filter, HookCtx, Kernel, Result, Tx};
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
    ("1152", "Withholding Tax Receivable", "Tax Assets", false, "Asset", "Tax"),
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
    ("2132", "Withholding Tax Payable", "Duties and Taxes", false, "Liability", "Tax"),
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

/// One account of the standard chart, as a new record for `company`, standing for itself.
fn coa_record(company: Uuid, currency: &Value, entry: &(&str, &str, &str, bool, &str, &str), parent: Option<&String>, number: bool) -> Doc {
    let &(account_number, name, _, is_group, root, typ) = entry;
    let mut d = Doc::new();
    d.insert("account_name".into(), json!(name));
    if number {
        d.insert("account_number".into(), json!(account_number));
    }
    d.insert("standard_number".into(), json!(account_number));
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

/// Creates the standard chart, cost center, warehouses, default links and the current fiscal year.
/// Idempotent: a company with accounts already is left to [`complete_chart`].
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
    match id(&co, "chart_template") {
        None => {
            for entry in COA {
                let parent = (!entry.2.is_empty()).then(|| ids.get(entry.2)).flatten();
                let created = k.insert_tx(&sctx, hc.tx, "Account", coa_record(company, &currency, entry, parent, true)).await?;
                ids.insert(entry.1, s(&created, "id").unwrap_or_default().to_string());
            }
        }
        Some(template) => {
            let rows = template_rows(hc, template).await?;
            let made = add_template_rows(hc, company, &currency, &rows, &HashMap::new()).await?;
            // The roles, by the standard chart's names, as the defaults and the add-ons ask for them.
            for &(number, name, ..) in COA {
                if let Some(a) = rows.iter().find(|r| s(r, "standard_number") == Some(number)).and_then(|r| s(r, "account_number")).and_then(|n| made.get(n)) {
                    ids.insert(name, a.clone());
                }
            }
        }
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
    // Its current fiscal year, by its own start month, with its months, unless it has years of its own
    // already: nothing posts outside an accounting period.
    if k.find_tx(&sctx, hc.tx, "FiscalYear", &[Filter::eq("company", company.to_string())], None, 1).await?.is_empty() {
        let month = co.get("fy_start_month").and_then(Value::as_u64).unwrap_or(4).clamp(1, 12) as u32;
        let start = ferrum_domain::reports::fy_start(hc.ctx.today(), month);
        make_year(k, hc.ctx, hc.tx, company, start, 12).await?;
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
        let doc = coa_record(company, &currency, entry, parent.as_ref(), !numbers.contains(entry.0));
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

/// A template's rows, refused when it is disabled.
async fn template_rows(hc: &mut HookCtx<'_>, template: Uuid) -> Result<Vec<Doc>> {
    let t = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "ChartTemplate", template, false).await?;
    if crate::util::b(&t, "disabled") {
        return Err(Error::field("chart_template", "this chart of accounts template is disabled"));
    }
    Ok(rows(&t, "accounts"))
}

/// Inserts the template rows whose numbers are not in `have` (number → account id), parents first, each under its
/// template parent where that parent is there. Answers every number's account id, those there before included.
async fn add_template_rows(
    hc: &mut HookCtx<'_>,
    company: Uuid,
    currency: &Value,
    rows: &[Doc],
    have: &HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    let sctx = sys(hc.ctx);
    let mut made: HashMap<String, String> = have.clone();
    let mut left: Vec<&Doc> = rows.iter().filter(|r| s(r, "account_number").is_some_and(|n| !made.contains_key(n))).collect();
    while !left.is_empty() {
        let before = left.len();
        let mut next = Vec::new();
        for r in left {
            let parent = s(r, "parent_number");
            let parent_id = match parent {
                None => None,
                Some(p) => match made.get(p) {
                    Some(id) => Some(id.clone()),
                    None if rows.iter().any(|x| s(x, "account_number") == Some(p)) => {
                        next.push(r);
                        continue;
                    }
                    // Its parent is in neither the template nor the company: it is left out.
                    None => continue,
                },
            };
            let mut d = Doc::new();
            for f in ["account_number", "account_name", "root_type", "account_type", "cash_flow_section", "standard_number"] {
                if let Some(v) = r.get(f).filter(|v| !v.is_null() && v.as_str() != Some("")) {
                    d.insert(f.into(), v.clone());
                }
            }
            d.insert("is_group".into(), json!(crate::util::b(r, "is_group")));
            d.insert("company".into(), json!(company.to_string()));
            d.insert("account_currency".into(), currency.clone());
            if let Some(p) = parent_id {
                d.insert("parent_account".into(), json!(p));
            }
            let created = hc.kernel.insert_tx(&sctx, hc.tx, "Account", d).await?;
            made.insert(s(r, "account_number").unwrap_or_default().to_string(), s(&created, "id").unwrap_or_default().to_string());
        }
        if next.len() == before {
            return Err(Error::field("chart_template", "the template's parents form a loop"));
        }
        left = next;
    }
    Ok(made)
}

/// `setup_chart_of_accounts` on a template company with accounts: the template's rows it lacks (by number), then
/// the default links still empty, from the roles.
async fn complete_from_template(hc: &mut HookCtx<'_>, company: Uuid, template: Uuid) -> Result<(Vec<String>, usize)> {
    let sctx = sys(hc.ctx);
    let k = hc.kernel;
    let rows = template_rows(hc, template).await?;
    let accounts = k.find_tx(&sctx, hc.tx, "Account", &[Filter::eq("company", company.to_string())], None, 100_000).await?;
    let have: HashMap<String, String> = accounts.iter().filter_map(|a| Some((s(a, "account_number")?.to_string(), s(a, "id")?.to_string()))).collect();
    let co = k.get_tx(&sctx, hc.tx, "Company", company, false).await?;
    let currency = co.get("default_currency").cloned().unwrap_or(Value::Null);
    let made = add_template_rows(hc, company, &currency, &rows, &have).await?;
    let added: Vec<String> =
        rows.iter().filter_map(|r| s(r, "account_number")).filter(|n| !have.contains_key(*n) && made.contains_key(*n)).map(str::to_string).collect();
    let mut v = Doc::new();
    for &(field, account) in DEFAULTS {
        if co.get(field).is_some_and(|x| !x.is_null()) {
            continue;
        }
        let Some(number) = COA.iter().find(|e| e.1 == account).map(|e| e.0) else { continue };
        if let Some(a) = standard_account(k, hc.ctx, hc.tx, company, number).await? {
            v.insert(field.into(), json!(a));
        }
    }
    let linked = v.len();
    if linked > 0 {
        k.set_values_tx(&sctx, hc.tx, "Company", company, v).await?;
    }
    Ok((added, linked))
}

/// The company's account that plays the standard chart's account `number`: on the standard chart (no template) the
/// account with that number, as always; on a template's chart the one standing for it. Never the second by the first's
/// rule: a national chart may give that number to something else.
pub async fn standard_account(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, number: &str) -> Result<Option<String>> {
    let sctx = sys(ctx);
    let co = k.get_tx(&sctx, tx, "Company", company, false).await?;
    let field = if id(&co, "chart_template").is_some() { "standard_number" } else { "account_number" };
    let found = k.find_tx(&sctx, tx, "Account", &[Filter::eq("company", company.to_string()), Filter::eq(field, number)], None, 1).await?;
    Ok(found.first().and_then(|a| s(a, "id")).map(str::to_string))
}

/// The company's account standing for `number`, made now under the account standing for `parent_number` when it has
/// none: numbered `number` on the standard chart, unnumbered on a template's (its numbers are the template's); named
/// `name`, or `name (number)` when the company has an account of that name already.
#[allow(clippy::too_many_arguments)]
pub async fn ensure_account(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut Tx,
    company: Uuid,
    number: &str,
    name: &str,
    parent_number: &str,
    root_type: &str,
    account_type: &str,
) -> Result<String> {
    if let Some(found) = standard_account(k, ctx, tx, company, number).await? {
        return Ok(found);
    }
    let parent = standard_account(k, ctx, tx, company, parent_number)
        .await?
        .ok_or_else(|| Error::invalid(format!("the company's chart has no account standing for {parent_number}")))?;
    let sctx = sys(ctx);
    let co = k.get_tx(&sctx, tx, "Company", company, false).await?;
    let taken = !k.find_tx(&sctx, tx, "Account", &[Filter::eq("company", company.to_string()), Filter::eq("account_name", name)], None, 1).await?.is_empty();
    let mut a = Doc::new();
    a.insert("account_name".into(), json!(if taken { format!("{name} ({number})") } else { name.to_string() }));
    if id(&co, "chart_template").is_none() {
        a.insert("account_number".into(), json!(number));
    }
    a.insert("standard_number".into(), json!(number));
    a.insert("company".into(), json!(company.to_string()));
    a.insert("is_group".into(), json!(false));
    a.insert("root_type".into(), json!(root_type));
    if !account_type.is_empty() {
        a.insert("account_type".into(), json!(account_type));
    }
    a.insert("parent_account".into(), json!(parent));
    a.insert("account_currency".into(), co.get("default_currency").cloned().unwrap_or(Value::Null));
    let created = k.insert_tx(&sctx, tx, "Account", a).await?;
    Ok(s(&created, "id").unwrap_or_default().to_string())
}

/// The standard chart's number of the standard account named `name` (as the add-ons name parents).
pub fn coa_number(name: &str) -> Option<&'static str> {
    COA.iter().find(|e| e.1 == name).map(|e| e.0)
}

/// What is wrong with a template's rows, in words: numbers once, parents that are groups of the same root, no
/// party account as a group, each standard number once, and every role ERP and its add-ons book to mapped to a row
/// of the right kind.
pub fn template_problems(rows: &[Doc]) -> Vec<String> {
    let mut out = Vec::new();
    let mut by_number: HashMap<&str, &Doc> = HashMap::new();
    for r in rows {
        let n = s(r, "account_number").unwrap_or_default();
        if by_number.insert(n, r).is_some() {
            out.push(format!("account {n} appears twice"));
        }
    }
    for r in rows {
        let n = s(r, "account_number").unwrap_or_default();
        let group = crate::util::b(r, "is_group");
        if group && matches!(s(r, "account_type"), Some("Receivable" | "Payable")) {
            out.push(format!("account {n}: a receivable or payable account cannot be a group"));
        }
        if let Some(p) = s(r, "parent_number") {
            match by_number.get(p) {
                None => out.push(format!("account {n}: its parent {p} is not in the template")),
                Some(parent) if !crate::util::b(parent, "is_group") => out.push(format!("account {n}: its parent {p} is not a group")),
                Some(parent) if s(parent, "root_type") != s(r, "root_type") => out.push(format!("account {n}: its root type is not its parent {p}'s")),
                _ => {}
            }
        }
    }
    let mut stands: HashMap<&str, &Doc> = HashMap::new();
    for r in rows {
        if let Some(sn) = s(r, "standard_number")
            && stands.insert(sn, r).is_some()
        {
            out.push(format!("two accounts stand for {sn}"));
        }
    }
    let name = |n: &str| COA.iter().find(|e| e.0 == n).map_or("", |e| e.1);
    let leaves = DEFAULTS.iter().filter_map(|(_, a)| COA.iter().find(|e| e.1 == *a).map(|e| e.0)).chain(["1142", "1151", "1152", "2131", "2132"]);
    for n in leaves {
        match stands.get(n) {
            None => out.push(format!("no account stands for {n} ({})", name(n))),
            Some(r) if crate::util::b(r, "is_group") => out.push(format!("the account standing for {n} ({}) is a group", name(n))),
            _ => {}
        }
    }
    for n in ["1150", "2130"] {
        match stands.get(n) {
            None => out.push(format!("no group stands for {n} ({})", name(n))),
            Some(r) if !crate::util::b(r, "is_group") => out.push(format!("the account standing for {n} ({}) is not a group", name(n))),
            _ => {}
        }
    }
    out
}

/// A template is saved only whole (`template_problems`).
pub struct ChartTemplateHook;

#[async_trait]
impl DocHook for ChartTemplateHook {
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let problems = template_problems(&rows(doc, "accounts"));
        if problems.is_empty() {
            return Ok(());
        }
        Err(Error::Validation(problems.into_iter().map(|m| ferrum_kernel::FieldError::new("accounts", m)).collect()))
    }
}

pub struct CompanyHook;

#[async_trait]
impl DocHook for CompanyHook {
    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        match old {
            None => {
                let company = id(doc, "id").ok_or_else(|| Error::internal("company without id"))?;
                setup_company(hc, company).await?;
            }
            Some(old) if id(old, "chart_template") != id(doc, "chart_template") => {
                return Err(Error::field("chart_template", "a company's chart is chosen when it is made"));
            }
            Some(_) => {}
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let company = id(doc, "id").ok_or_else(|| Error::internal("company without id"))?;
        if name == "post_opening_invoices" {
            return Ok(Some(crate::opening::post_all(hc, company).await?));
        }
        if name == "recognise_deferred" {
            return Ok(Some(crate::deferred::action(hc.kernel, hc.ctx, hc.tx, company, params).await?));
        }
        if name != "setup_chart_of_accounts" {
            return Ok(None);
        }
        if setup_company(hc, company).await? {
            return Ok(Some(json!({"created": true, "added": [], "defaults_set": 0, "message": "Chart of accounts created"})));
        }
        let (added, linked) = match id(doc, "chart_template") {
            Some(template) => complete_from_template(hc, company, template).await?,
            None => {
                let (added, linked) = complete_chart(hc, company).await?;
                (added.into_iter().map(str::to_string).collect(), linked)
            }
        };
        let message = match (added.is_empty(), linked) {
            (true, 0) => "This company's chart already has every account of its chart".to_string(),
            (true, n) => format!("{n} default account{} set", if n == 1 { "" } else { "s" }),
            (false, n) => format!("Added {} to the chart; {n} default account{} set", added.join(", "), if n == 1 { "" } else { "s" }),
        };
        Ok(Some(json!({"created": false, "added": added, "defaults_set": linked, "message": message})))
    }
}

/// Makes a company's fiscal year of `months` from `start` as a person does, its months made by the
/// FiscalYear hook, under the name the platform gives the year that starts on `start`.
async fn make_year(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, start: NaiveDate, months: u32) -> Result<()> {
    let end = start.checked_add_months(Months::new(months)).and_then(|d| d.pred_opt()).unwrap_or(start);
    let mut fy = Doc::new();
    fy.insert("year_name".into(), json!(ferrum_kernel::numbering::fiscal_year_label(start, start.month())));
    fy.insert("company".into(), json!(company.to_string()));
    fy.insert("start_date".into(), json!(start.to_string()));
    fy.insert("end_date".into(), json!(end.to_string()));
    k.insert_tx(&sys(ctx), tx, "FiscalYear", fy).await.map(|_| ())
}

/// Upkeep on the workspace's `today`: each company's next fiscal year is made 30 days before it is
/// needed, so the books do not stop at a year's end. It starts the day after the company's latest year
/// and is as many months long, so a year of the company's own shape keeps it. A company with no year is
/// left as it is, and no year further out is ever made: a day in none is still refused.
pub async fn upkeep(k: &Kernel, ctx: &Ctx, today: NaiveDate) -> Result<()> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let due: Vec<(Uuid, NaiveDate, NaiveDate)> = sqlx::query_as(
        "select distinct on (f.company) f.company, f.start_date, f.end_date from e_fiscal_year f where f.tenant_id = $1 \
         and not exists (select 1 from e_fiscal_year h where h.tenant_id = f.tenant_id and h.company = f.company and $2 between h.start_date and h.end_date) \
         order by f.company, f.end_date desc",
    )
    .bind(ctx.tenant)
    .bind(today + Duration::days(30))
    .fetch_all(&mut *tx)
    .await?;
    tx.rollback().await?;
    for (company, start, end) in due {
        let next = end + Duration::days(1);
        let months = (next.year() - start.year()) * 12 + next.month() as i32 - start.month() as i32;
        // Each company in a transaction of its own: one whose year cannot be made (on two replicas at
        // once, the second finds it made) holds up no other.
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        match make_year(k, ctx, &mut tx, company, next, months.max(1) as u32).await {
            Ok(()) => tx.commit().await?,
            Err(e) => {
                tx.rollback().await?;
                tracing::warn!(tenant = %ctx.tenant, %company, "next fiscal year not made: {e}");
            }
        }
    }
    Ok(())
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

    /// The standard chart as a template's rows, each standing for itself.
    fn standard_rows() -> Vec<Doc> {
        COA.iter()
            .map(|&(number, name, parent, is_group, root, typ)| {
                let parent = COA.iter().find(|e| e.1 == parent).map(|e| e.0);
                let v = json!({"account_number": number, "account_name": name, "parent_number": parent, "is_group": is_group, "root_type": root,
                               "account_type": if typ.is_empty() { Value::Null } else { json!(typ) }, "standard_number": number});
                v.as_object().cloned().unwrap()
            })
            .collect()
    }

    #[test]
    fn a_template_must_map_every_role() {
        assert_eq!(template_problems(&standard_rows()), Vec::<String>::new(), "the standard chart is a whole template");
        let without = |n: &str| {
            standard_rows()
                .into_iter()
                .map(|mut r| {
                    if r["standard_number"] == json!(n) {
                        r.insert("standard_number".into(), Value::Null);
                    }
                    r
                })
                .collect::<Vec<_>>()
        };
        assert!(template_problems(&without("2131")).contains(&"no account stands for 2131 (Output Tax)".to_string()));
        assert!(template_problems(&without("2130")).contains(&"no group stands for 2130 (Duties and Taxes)".to_string()));
        let mut twice = standard_rows();
        twice.iter_mut().find(|r| r["account_number"] == json!("2132")).unwrap().insert("standard_number".into(), json!("2131"));
        assert!(template_problems(&twice).iter().any(|m| m == "two accounts stand for 2131"));
        let mut under_leaf = standard_rows();
        under_leaf.iter_mut().find(|r| r["account_number"] == json!("2132")).unwrap().insert("parent_number".into(), json!("2131"));
        assert!(template_problems(&under_leaf).contains(&"account 2132: its parent 2131 is not a group".to_string()));
        let mut grouped = standard_rows();
        grouped.iter_mut().find(|r| r["account_number"] == json!("1111")).unwrap().insert("is_group".into(), json!(true));
        assert!(template_problems(&grouped).iter().any(|m| m.contains("1111: a receivable or payable account cannot be a group")));
    }
}
