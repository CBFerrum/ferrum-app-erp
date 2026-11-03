//! Finance reports: budget variance, bank reconciliation statement, cash flow forecast,
//! receivables by installment due date, dunning summary and subscription metrics. Each runs in one
//! read-only, tenant-isolated transaction and reads only what the caller may read.

use crate::budget::{actual, ordered, requested};
use crate::common::{fiscal_year, fiscal_year_of};
use crate::rules::{self, Interval, MONTHS};
use crate::subscription::{self, Sub};
use crate::terms::{open_installments, schedules};
use async_trait::async_trait;
use chrono::{Datelike, Duration, NaiveDate};
use ferrum_domain::reports::{Params, ReportCompany, ReportSource, col, param_def, period, read_tx, resolve_company, today};
use ferrum_domain::util::{exchange_rate, money, round};
use ferrum_kernel::{Action, Ctx, Error, Kernel, Result, Tx, row_scope};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

pub struct FinanceReports;

struct Def {
    name: &'static str,
    label: &'static str,
    entity: &'static str,
    params: &'static [&'static str],
}

const DEFS: &[Def] = &[
    Def {
        name: "budget_variance",
        label: "Budget Variance",
        entity: "Budget",
        params: &["company", "fiscal_year", "budget_against", "cost_center", "project"],
    },
    Def { name: "finance_overview", label: "Finance Overview", entity: "JournalEntry", params: &["company", "as_of"] },
    Def { name: "bank_reconciliation_statement", label: "Bank Reconciliation Statement", entity: "BankTransaction", params: &["bank_account", "as_of"] },
    Def { name: "cash_flow_forecast", label: "Cash Flow Forecast", entity: "JournalEntry", params: &["company", "from_date", "to_date", "group_by"] },
    Def {
        name: "receivables_by_due_date",
        label: "Receivables by Installment Due Date",
        entity: "SalesInvoice",
        params: &["company", "as_of", "party_type", "customer", "supplier"],
    },
    Def { name: "dunning_summary", label: "Dunning Summary", entity: "Dunning", params: &["company", "from_date", "to_date", "customer", "status"] },
    Def { name: "subscription_metrics", label: "Subscription MRR, ARR and Churn", entity: "Subscription", params: &["company", "from_date", "to_date"] },
];

fn param(name: &str) -> Value {
    match name {
        "fiscal_year" => json!({"name": name, "label": "Fiscal Year", "type": "link", "to": "FiscalYear"}),
        "budget_against" => json!({"name": name, "label": "Budget Against", "type": "select", "options": ["Cost Center", "Project"]}),
        "cost_center" => json!({"name": name, "label": "Cost Center", "type": "link", "to": "CostCenter"}),
        "project" => json!({"name": name, "label": "Project", "type": "link", "to": "Project"}),
        "bank_account" => json!({"name": name, "label": "Bank Account", "type": "link", "to": "BankAccount", "required": true}),
        "group_by" => json!({"name": name, "label": "Group By", "type": "select", "options": ["Week", "Month"], "default": "Month"}),
        "party_type" => json!({"name": name, "label": "Party Type", "type": "select", "options": ["Customer", "Supplier"], "default": "Customer"}),
        "status" => json!({"name": name, "label": "Status", "type": "select", "options": ["Draft", "Unresolved", "Resolved", "Cancelled"]}),
        "to_date" => json!({"name": name, "label": "To Date", "type": "date"}),
        other => param_def(other),
    }
}

#[async_trait]
impl ReportSource for FinanceReports {
    fn catalog(&self) -> Vec<Value> {
        DEFS.iter()
            .map(|d| {
                let mut entry = json!({"name": d.name, "label": d.label, "module": "finance", "entity": d.entity, "params": d.params.iter().map(|p| param(p)).collect::<Vec<_>>()});
                // The overview counts documents of several kinds across the company.
                if d.name == "finance_overview" {
                    entry["scoped"] = json!(false);
                }
                entry
            })
            .collect()
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
        let p = Params { raw };
        let mut tx = read_tx(k, ctx).await?;
        if name == "bank_reconciliation_statement" {
            let out = bank_statement(ctx, &mut tx, &p).await?;
            tx.rollback().await?;
            return Ok(out);
        }
        let co = resolve_company(&mut tx, ctx, &p).await?;
        let mut out = match name {
            "budget_variance" => budget_variance(ctx, &mut tx, &co, &p).await?,
            "cash_flow_forecast" => cash_flow(k, ctx, &mut tx, &co, &p).await?,
            "finance_overview" => overview(ctx, &mut tx, &co, &p).await?,
            "receivables_by_due_date" => by_due_date(ctx, &mut tx, &co, &p).await?,
            "dunning_summary" => dunning_summary(ctx, &mut tx, &co, &p).await?,
            "subscription_metrics" => subscription_metrics(k, ctx, &mut tx, &co, &p).await?,
            _ => return Err(Error::NotFound(format!("report {name}"))),
        };
        tx.rollback().await?;
        if let Value::Object(m) = &mut out {
            let meta = m.entry("meta").or_insert_with(|| json!({}));
            meta["company"] = json!(co.name);
            meta["currency"] = json!(co.currency);
        }
        Ok(out)
    }
}

fn pct(part: Decimal, whole: Decimal) -> Decimal {
    if whole.is_zero() { Decimal::ZERO } else { (part / whole * Decimal::ONE_HUNDRED).round_dp(2) }
}

// ───────────────────────────── budget variance ─────────────────────────────

#[derive(sqlx::FromRow)]
struct BudgetLine {
    number: Option<String>,
    budget_against: String,
    cost_center: Option<Uuid>,
    project: Option<Uuid>,
    against_name: Option<String>,
    monthly_distribution: Option<Uuid>,
    applicable_on_purchase_order: bool,
    applicable_on_material_request: bool,
    account: Uuid,
    account_name: String,
    root_type: String,
    budget_amount: Decimal,
}

/// Budget, actual and committed per budget line, phased by month (month budgets are differences
/// of the accumulated budget, so they add up to the annual amount exactly).
async fn budget_variance(ctx: &Ctx, tx: &mut Tx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let fy = match p.uuid("fiscal_year")? {
        Some(f) => fiscal_year(tx, ctx.tenant, f).await?.filter(|(c, _)| *c == co.id).map(|(_, f)| f),
        None => fiscal_year_of(tx, ctx.tenant, co.id, today()).await?,
    }
    .ok_or_else(|| Error::field("fiscal_year", "choose a fiscal year of this company"))?;
    let lines: Vec<BudgetLine> = sqlx::query_as(
        "select b.number, b.budget_against, b.cost_center, b.project, coalesce(cc.cost_center_name, pr.project_name) as against_name, \
         b.monthly_distribution, b.applicable_on_purchase_order, b.applicable_on_material_request, a.account, coalesce(acc.account_number || ' - ', '') || acc.account_name as account_name, \
         acc.root_type, a.budget_amount from e_budget b join e_budget__accounts a on a.tenant_id = b.tenant_id and a.parent_id = b.id \
         join e_account acc on acc.tenant_id = a.tenant_id and acc.id = a.account \
         left join e_cost_center cc on cc.tenant_id = b.tenant_id and cc.id = b.cost_center left join e_project pr on pr.tenant_id = b.tenant_id and pr.id = b.project \
         where b.tenant_id = $1 and b.company = $2 and b.fiscal_year = $3 and b.docstatus = 1 and ($4::text is null or b.budget_against = $4) \
         and ($5::uuid is null or b.cost_center = $5) and ($6::uuid is null or b.project = $6) order by b.number, a.idx",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(fy.id)
    .bind(p.str("budget_against"))
    .bind(p.uuid("cost_center")?)
    .bind(p.uuid("project")?)
    .fetch_all(&mut **tx)
    .await?;
    let mut months = Vec::new();
    let mut m = rules::month_start(fy.start);
    while m <= fy.end && months.len() < 24 {
        months.push(m);
        m = rules::add_months(m, 1);
    }
    let mut rows = Vec::new();
    let mut totals = [Decimal::ZERO; 4];
    for l in lines {
        let scope = crate::budget::scope_of(tx, ctx.tenant, &l.budget_against, l.cost_center, l.project).await?;
        // Expense consumes a budget; income fulfils it: both read in their natural direction.
        let sign = if l.root_type == "Income" { -Decimal::ONE } else { Decimal::ONE };
        let mut month_rows = Vec::new();
        let (mut sum_actual, mut sum_committed) = (Decimal::ZERO, Decimal::ZERO);
        let mut previous = Decimal::ZERO;
        for start in &months {
            let end = rules::month_end(*start).min(fy.end);
            let from = (*start).max(fy.start);
            let share = crate::budget::monthly_share(tx, ctx.tenant, &fy, l.monthly_distribution, end).await?;
            let cumulative = round(l.budget_amount * share, co.precision);
            let budget = cumulative - previous;
            previous = cumulative;
            let act = sign * actual(tx, ctx.tenant, co.id, l.account, &scope, from, end).await?;
            let mut committed = Decimal::ZERO;
            if l.applicable_on_purchase_order && sign.is_sign_positive() {
                committed += ordered(tx, ctx.tenant, co.id, l.account, &scope, from, end, co.precision).await?;
            }
            if l.applicable_on_material_request && sign.is_sign_positive() {
                committed += requested(tx, ctx, co.id, l.account, &scope, from, end, co.precision).await?;
            }
            sum_actual += act;
            sum_committed += committed;
            month_rows.push(json!({"period": format!("{} {}", MONTHS[start.month0() as usize], start.year()), "budget": money(budget), "actual": money(act),
                "committed": money(committed), "variance": money(budget - act - committed), "variance_pct": money(pct(budget - act - committed, budget)), "_indent": 1}));
        }
        let variance = l.budget_amount - sum_actual - sum_committed;
        rows.push(
            json!({"period": format!("{} · {} {} ({})", l.account_name, l.budget_against, l.against_name.unwrap_or_default(), l.number.unwrap_or_default()),
            "budget": money(l.budget_amount), "actual": money(sum_actual), "committed": money(sum_committed), "variance": money(variance),
            "variance_pct": money(pct(variance, l.budget_amount)), "_bold": true}),
        );
        rows.extend(month_rows);
        for (t, v) in totals.iter_mut().zip([l.budget_amount, sum_actual, sum_committed, variance]) {
            *t += v;
        }
    }
    Ok(json!({
        "columns": [col("period", "Budget / Month", "data"), col("budget", "Budget", "currency"), col("actual", "Actual", "currency"),
            col("committed", "Committed (orders and requests)", "currency"), col("variance", "Variance (left)", "currency"), col("variance_pct", "Variance %", "percent")],
        "rows": rows,
        "totals": {"budget": money(totals[0]), "actual": money(totals[1]), "committed": money(totals[2]), "variance": money(totals[3])},
        "meta": {"fiscal_year": fy.name, "from_date": fy.start, "to_date": fy.end},
    }))
}

// ───────────────────────────── bank reconciliation statement ─────────────────────────────

/// Book balance against bank balance as of a date, with the vouchers not cleared yet and the bank
/// lines not in the books explaining the difference.
pub(crate) async fn bank_statement(ctx: &Ctx, tx: &mut Tx, p: &Params<'_>) -> Result<Value> {
    let ba = p.uuid("bank_account")?.ok_or_else(|| Error::field("bank_account", "is required"))?;
    let as_of = p.date("as_of")?.unwrap_or_else(today);
    let row: Option<(Uuid, Uuid, Option<Uuid>, String, Option<Decimal>, Option<NaiveDate>)> =
        sqlx::query_as("select company, account, currency, account_name, opening_balance, opening_date from e_bank_account where tenant_id = $1 and id = $2")
            .bind(ctx.tenant)
            .bind(ba)
            .fetch_optional(&mut **tx)
            .await?;
    let (company, gl, cur, name, opening, opening_date) = row.ok_or_else(|| Error::NotFound("Bank Account".into()))?;
    let raw = HashMap::from([("company".to_string(), company.to_string())]);
    let co = resolve_company(tx, ctx, &Params { raw: &raw }).await?;
    let code: String = match cur {
        Some(c) => sqlx::query_scalar("select code from e_currency where tenant_id = $1 and id = $2").bind(ctx.tenant).bind(c).fetch_one(&mut **tx).await?,
        None => co.currency.clone(),
    };
    let precision: i32 = sqlx::query_scalar("select coalesce(precision, 2)::int4 from e_currency where tenant_id = $1 and code = $2")
        .bind(ctx.tenant)
        .bind(&code)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(2);
    let base = code == co.currency;
    let book: Decimal = sqlx::query_scalar(
        "select coalesce(sum(case when $5 then debit - credit else debit_txn - credit_txn end), 0) from journal_line_in_book \
         where tenant_id = $1 and company_id = $2 and account_id = $3 and posting_date <= $4 and ($5 or trim(currency) = $6)",
    )
    .bind(ctx.tenant)
    .bind(company)
    .bind(gl)
    .bind(as_of)
    .bind(base)
    .bind(&code)
    .fetch_one(&mut **tx)
    .await?;
    // Vouchers on the account and what statements dated up to `as_of` cleared of them.
    let vouchers: Vec<(String, Uuid, Option<String>, NaiveDate, Decimal, Option<String>, Decimal)> = sqlx::query_as(
        "with v as ( \
           select 'PaymentEntry' as kind, p.id, p.number, p.posting_date, \
                  (case when p.payment_type = 'Pay' then -1 else 1 end) * (case when c.code = $6 then p.paid_amount else p.base_paid_amount end) as amount, \
                  p.party_name as party \
           from e_payment_entry p left join e_currency c on c.tenant_id = p.tenant_id and c.id = p.currency \
           where p.tenant_id = $1 and p.company = $2 and p.docstatus = 1 and p.bank_account = $3 and p.posting_date <= $4 and (c.code = $6 or $5) \
           union all \
           select 'JournalEntry', j.id, j.number, j.posting_date, sum(coalesce(a.debit, 0) - coalesce(a.credit, 0)), j.remark \
           from e_journal_entry j join e_journal_entry__accounts a on a.tenant_id = j.tenant_id and a.parent_id = j.id \
           where $5 and j.tenant_id = $1 and j.company = $2 and j.docstatus = 1 and a.account = $3 and j.posting_date <= $4 \
           group by j.id, j.number, j.posting_date, j.remark) \
         select v.kind, v.id, v.number, v.posting_date, v.amount, v.party, coalesce((select sum(x.allocated_amount) from e_bank_transaction__allocations x \
           join e_bank_transaction t on t.tenant_id = x.tenant_id and t.id = x.parent_id \
           where x.tenant_id = $1 and t.docstatus = 1 and t.date <= $4 and x.payment_entry = v.id::text), 0) \
         from v where v.amount <> 0 order by v.posting_date, v.number",
    )
    .bind(ctx.tenant)
    .bind(company)
    .bind(gl)
    .bind(as_of)
    .bind(base)
    .bind(&code)
    .fetch_all(&mut **tx)
    .await?;
    let mut rows = vec![json!({"description": "Balance as per books", "amount": money(book), "_bold": true})];
    let (mut deposits, mut payments) = (Decimal::ZERO, Decimal::ZERO);
    let mut deposit_rows = Vec::new();
    let mut payment_rows = Vec::new();
    for (kind, vid, number, date, amount, party, cleared) in vouchers {
        let open = amount.abs() - cleared;
        if open <= Decimal::ZERO {
            continue;
        }
        let r = json!({"date": date, "voucher_type": kind, "voucher_id": vid, "voucher_no": number, "description": party, "amount": money(open), "_indent": 1});
        if amount > Decimal::ZERO {
            deposits += open;
            deposit_rows.push(r);
        } else {
            payments += open;
            payment_rows.push(r);
        }
    }
    rows.push(json!({"description": "Less: deposits not yet cleared by the bank", "amount": money(-deposits), "_bold": true}));
    rows.extend(deposit_rows);
    rows.push(json!({"description": "Add: payments not yet cleared by the bank", "amount": money(payments), "_bold": true}));
    rows.extend(payment_rows);
    // Bank lines with nothing (or not everything) recorded in the books yet.
    let lines: Vec<(Uuid, Option<String>, NaiveDate, Option<String>, Decimal, Decimal)> = sqlx::query_as(
        "select t.id, t.number, t.date, coalesce(t.reference_number, '') || ' ' || coalesce(t.description, ''), \
         coalesce(t.deposit, 0) - coalesce(t.withdrawal, 0), coalesce((select sum(x.allocated_amount) from e_bank_transaction__allocations x \
         where x.tenant_id = t.tenant_id and x.parent_id = t.id), 0) \
         from e_bank_transaction t where t.tenant_id = $1 and t.bank_account = $2 and t.docstatus = 1 and t.date <= $3 order by t.date, t.number",
    )
    .bind(ctx.tenant)
    .bind(ba)
    .bind(as_of)
    .fetch_all(&mut **tx)
    .await?;
    let mut unrecorded = Decimal::ZERO;
    let mut statement_moves = Decimal::ZERO;
    let mut unrecorded_rows = Vec::new();
    for (tid, number, date, text, signed, allocated) in lines {
        if opening_date.is_none_or(|o| date > o) {
            statement_moves += signed;
        }
        let open = signed.abs() - allocated;
        if open > Decimal::ZERO {
            let amt = if signed > Decimal::ZERO { open } else { -open };
            unrecorded += amt;
            unrecorded_rows.push(json!({"date": date, "voucher_type": "BankTransaction", "voucher_id": tid, "voucher_no": number,
                "description": text.as_deref().unwrap_or_default().trim(), "amount": money(amt), "_indent": 1}));
        }
    }
    rows.push(json!({"description": "Add: bank lines not recorded in the books", "amount": money(unrecorded), "_bold": true}));
    rows.extend(unrecorded_rows);
    let expected = book - deposits + payments + unrecorded;
    rows.push(json!({"description": "Expected balance per bank", "amount": money(expected), "_bold": true}));
    let statement = if opening_date.is_some_and(|o| o > as_of) { Decimal::ZERO } else { opening.unwrap_or_default() } + statement_moves;
    rows.push(json!({"description": "Balance per imported bank statement", "amount": money(statement), "_bold": true}));
    rows.push(json!({"description": "Difference", "amount": money(statement - expected), "_bold": true}));
    Ok(json!({
        "columns": [col("date", "Date", "date"), col("voucher_type", "Type", "data"), col("voucher_no", "Voucher", "data"), col("description", "Description", "data"),
            col("amount", "Amount", "currency")],
        "rows": rows,
        "totals": {"book_balance": money(book), "uncleared_deposits": money(deposits), "uncleared_payments": money(payments), "unrecorded": money(unrecorded),
            "expected_bank_balance": money(expected), "statement_balance": money(statement), "difference": money(statement - expected)},
        "meta": {"bank_account": name, "as_of": as_of, "currency": code, "precision": precision, "company": co.name, "reconciled": statement == expected},
    }))
}

// ───────────────────────────── cash flow forecast ─────────────────────────────

/// The bucket a date falls in: weeks from `from`, or calendar months. Overdue amounts fall in the first.
fn bucket(day: NaiveDate, from: NaiveDate, weekly: bool) -> NaiveDate {
    let day = day.max(from);
    if weekly { from + Duration::days((day - from).num_days() / 7 * 7) } else { rules::month_start(day).max(from) }
}

async fn cash_flow(k: &Kernel, ctx: &Ctx, tx: &mut Tx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let from = p.date("from_date")?.unwrap_or_else(today);
    let to = p.date("to_date")?.unwrap_or(from + Duration::days(90));
    if to < from {
        return Err(Error::field("to_date", "must not be before the from date"));
    }
    if (to - from).num_days() > 3 * 366 {
        return Err(Error::field("to_date", "forecast at most three years ahead"));
    }
    let weekly = p.str("group_by") == Some("Week");
    let opening: Decimal = sqlx::query_scalar(
        "select coalesce(sum(l.debit - l.credit), 0) from journal_line_in_book l join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
         where l.tenant_id = $1 and l.company_id = $2 and l.posting_date < $3 and a.account_type in ('Bank', 'Cash')",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .fetch_one(&mut **tx)
    .await?;
    // Per bucket: receivables, payables, sales orders, purchase orders, subscriptions.
    let mut flows: BTreeMap<NaiveDate, [Decimal; 5]> = BTreeMap::new();
    let mut overdue = [Decimal::ZERO; 2];
    let add = |flows: &mut BTreeMap<NaiveDate, [Decimal; 5]>, day: NaiveDate, kind: usize, amount: Decimal| {
        if day <= to && !amount.is_zero() {
            flows.entry(bucket(day, from, weekly)).or_default()[kind] += amount;
        }
    };
    // Open receivables and payables by installment due date.
    let items: Vec<(Uuid, String, String, NaiveDate, Option<NaiveDate>, Decimal, Decimal, Decimal)> = sqlx::query_as(
        "select id, entity, party_kind, posting_date, due_date, amount, outstanding, exchange_rate from open_item where tenant_id = $1 and company_id = $2 \
         and status = 'open' and outstanding > 0",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .fetch_all(&mut **tx)
    .await?;
    let ids = |e: &str| items.iter().filter(|i| i.1 == e).map(|i| i.0).collect::<Vec<_>>();
    let mut plans = schedules(tx, ctx.tenant, "e_sales_invoice", &ids("SalesInvoice")).await?;
    plans.extend(schedules(tx, ctx.tenant, "e_purchase_invoice", &ids("PurchaseInvoice")).await?);
    for (iid, _, kind, posted, due, amount, outstanding, rate) in &items {
        let customer = kind == "customer";
        for (inst, open) in open_installments(plans.get(iid), *amount, *outstanding, due.unwrap_or(*posted)) {
            let base = round(open * rate, co.precision);
            if base.is_zero() {
                continue;
            }
            if inst.due < from {
                overdue[if customer { 0 } else { 1 }] += base;
            }
            add(&mut flows, inst.due, if customer { 0 } else { 1 }, if customer { base } else { -base });
        }
    }
    // Unbilled parts of open orders with a payment schedule.
    for (table, kind, sign) in [("e_sales_order", 2usize, Decimal::ONE), ("e_purchase_order", 3usize, -Decimal::ONE)] {
        let sql = format!(
            "select s.due_date, s.payment_amount * coalesce(o.conversion_rate, 1) * (100 - least(coalesce(o.per_billed, 0), 100)) / 100 \
             from {table} o join {table}__fin_payment_schedule s on s.tenant_id = o.tenant_id and s.parent_id = o.id \
             where o.tenant_id = $1 and o.company = $2 and o.docstatus = 1 and o.status not in ('Closed', 'Completed')"
        );
        let rows: Vec<(NaiveDate, Decimal)> = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(ctx.tenant).bind(co.id).fetch_all(&mut **tx).await?;
        for (due, amount) in rows {
            add(&mut flows, due, kind, sign * round(amount, co.precision));
        }
    }
    // Subscription invoices still to come, net of tax, due per the customer's terms.
    let company_currency = currency_id(tx, ctx, &co.currency).await?;
    for (s, amount_txn, invoice_date) in upcoming_subscriptions(k, ctx, tx, co, from, to).await? {
        let rate = exchange_rate(k, ctx, tx, s.currency, company_currency, from).await?;
        let base = round(amount_txn * rate, co.precision);
        add(&mut flows, invoice_date + Duration::days(s.days_until_due), 4, base);
    }
    let mut rows = Vec::new();
    let mut balance = opening;
    let mut totals = [Decimal::ZERO; 5];
    for (start, f) in flows {
        let net: Decimal = f.iter().copied().sum();
        balance += net;
        for (t, v) in totals.iter_mut().zip(f) {
            *t += v;
        }
        let label = if weekly { format!("Week of {start}") } else { format!("{} {}", MONTHS[start.month0() as usize], start.year()) };
        rows.push(json!({"period": label, "from_date": start, "receivables": money(f[0]), "payables": money(f[1]), "sales_orders": money(f[2]),
            "purchase_orders": money(f[3]), "subscriptions": money(f[4]), "net": money(net), "balance": money(balance)}));
    }
    Ok(json!({
        "columns": [col("period", "Period", "data"), col("receivables", "Receivables", "currency"), col("sales_orders", "Sales Orders", "currency"),
            col("subscriptions", "Subscriptions", "currency"), col("payables", "Payables", "currency"), col("purchase_orders", "Purchase Orders", "currency"),
            col("net", "Net Flow", "currency"), col("balance", "Projected Cash", "currency")],
        "rows": rows,
        "totals": {"opening_balance": money(opening), "receivables": money(totals[0]), "payables": money(totals[1]), "sales_orders": money(totals[2]),
            "purchase_orders": money(totals[3]), "subscriptions": money(totals[4]), "closing_balance": money(balance)},
        "meta": {"from_date": from, "to_date": to, "overdue_receivables": money(overdue[0]), "overdue_payables": money(overdue[1]),
            "note": "Overdue amounts are expected in the first period; subscription amounts exclude tax."},
    }))
}

async fn currency_id(tx: &mut Tx, ctx: &Ctx, code: &str) -> Result<Uuid> {
    sqlx::query_scalar("select id from e_currency where tenant_id = $1 and code = $2")
        .bind(ctx.tenant)
        .bind(code)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| Error::NotFound(format!("currency {code}")))
}

/// Invoices subscriptions will make with dates in `[from, to]`: (subscription, amount, invoice date).
async fn upcoming_subscriptions(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut Tx,
    co: &ReportCompany,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<Vec<(Sub, Decimal, NaiveDate)>> {
    let ids: Vec<Uuid> = sqlx::query_scalar("select id from e_subscription where tenant_id = $1 and company = $2 and status not in ('Cancelled', 'Completed')")
        .bind(ctx.tenant)
        .bind(co.id)
        .fetch_all(&mut **tx)
        .await?;
    let sysctx = ferrum_domain::util::sys(ctx);
    let mut out = Vec::new();
    for sid in ids {
        let doc = k.get_tx(&sysctx, tx, "Subscription", sid, false).await?;
        let s = subscription::load(k, ctx, tx, &doc).await?;
        let mut start = subscription::next_start(tx, ctx.tenant, &s).await?;
        for _ in 0..1000 {
            if s.stop().is_some_and(|x| start > x) {
                break;
            }
            let period = s.period_from(start);
            let invoice_date = period.start - Duration::days(s.days_before);
            if invoice_date > to {
                break;
            }
            let (end, factor) = subscription::billed(&s, &period);
            if invoice_date >= from {
                let amount: Decimal = s.plans.iter().map(|p| p.qty * p.price * factor).sum();
                out.push((s.clone(), amount, invoice_date));
            }
            start = end + Duration::days(1);
        }
    }
    Ok(out)
}

// ───────────────────────────── receivables by due date ─────────────────────────────

async fn by_due_date(ctx: &Ctx, tx: &mut Tx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let as_of = p.date("as_of")?.unwrap_or_else(today);
    let supplier_side = p.str("party_type") == Some("Supplier");
    if supplier_side {
        ctx.require("PurchaseInvoice", Action::Read)?;
    }
    let (kind, entity, table, party) = if supplier_side {
        ("supplier", "PurchaseInvoice", "e_purchase_invoice", p.uuid("supplier")?)
    } else {
        ("customer", "SalesInvoice", "e_sales_invoice", p.uuid("customer")?)
    };
    let items: Vec<(Uuid, String, Option<String>, NaiveDate, Option<NaiveDate>, Option<String>, String, Decimal, Decimal, Decimal)> = sqlx::query_as(
        "select o.id, o.entity, o.number, o.posting_date, o.due_date, coalesce(c.customer_name, s.supplier_name), trim(o.currency), o.amount, \
         o.amount - coalesce((select sum(x.amount) from allocation x where x.tenant_id = o.tenant_id and x.open_item_id = o.id and x.posting_date <= $3), 0), \
         o.exchange_rate from open_item o left join e_customer c on c.tenant_id = o.tenant_id and c.id = o.customer_id \
         left join e_supplier s on s.tenant_id = o.tenant_id and s.id = o.supplier_id \
         where o.tenant_id = $1 and o.company_id = $2 and o.party_kind = $4 and o.posting_date <= $3 and o.status <> 'cancelled' and o.amount > 0 \
         and ($5::uuid is null or o.customer_id = $5 or o.supplier_id = $5) order by coalesce(o.due_date, o.posting_date), o.posting_date limit 20000",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(as_of)
    .bind(kind)
    .bind(party)
    .fetch_all(&mut **tx)
    .await?;
    let ids: Vec<Uuid> = items.iter().filter(|i| i.1 == entity).map(|i| i.0).collect();
    let plans = schedules(tx, ctx.tenant, table, &ids).await?;
    let buckets = ["not_due", "d1_30", "d31_60", "d61_90", "d90_plus"];
    let mut totals: BTreeMap<&str, Decimal> = buckets.iter().map(|b| (*b, Decimal::ZERO)).collect();
    let mut total = Decimal::ZERO;
    let mut rows = Vec::new();
    for (iid, ent, number, posted, due, name, currency, amount, outstanding, rate) in items {
        if outstanding <= Decimal::ZERO {
            continue;
        }
        for (inst, open) in open_installments(plans.get(&iid), amount, outstanding, due.unwrap_or(posted)) {
            if open <= Decimal::ZERO {
                continue;
            }
            let base = round(open * rate, co.precision);
            let days = (as_of - inst.due).num_days();
            let b = match days {
                x if x <= 0 => "not_due",
                1..=30 => "d1_30",
                31..=60 => "d31_60",
                61..=90 => "d61_90",
                _ => "d90_plus",
            };
            *totals.entry(b).or_default() += base;
            total += base;
            let mut r = json!({"party": name, "voucher_type": ent, "voucher_id": iid, "voucher_no": number, "installment": inst.description,
                "due_date": inst.due, "currency": currency, "installment_amount": money(inst.amount), "outstanding": money(open),
                "outstanding_base": money(base), "days_overdue": days.max(0)});
            for x in buckets {
                r[x] = money(if x == b { base } else { Decimal::ZERO });
            }
            rows.push(r);
        }
    }
    rows.sort_by(|a, b| a["due_date"].as_str().cmp(&b["due_date"].as_str()));
    let mut t = serde_json::Map::new();
    t.insert("outstanding_base".into(), money(total));
    for (b, v) in totals {
        t.insert(b.into(), money(v));
    }
    Ok(json!({
        "columns": [col("party", if supplier_side { "Supplier" } else { "Customer" }, "data"), col("voucher_no", "Voucher", "data"),
            col("installment", "Installment", "data"), col("due_date", "Due Date", "date"), col("currency", "Currency", "data"),
            col("installment_amount", "Installment", "currency"), col("outstanding", "Outstanding", "currency"),
            col("outstanding_base", "Outstanding (Company Currency)", "currency"), col("days_overdue", "Days Overdue", "int"), col("not_due", "Not Due", "currency"),
            col("d1_30", "1-30", "currency"), col("d31_60", "31-60", "currency"), col("d61_90", "61-90", "currency"), col("d90_plus", "90+", "currency")],
        "rows": rows,
        "totals": t,
        "meta": {"as_of": as_of},
    }))
}

// ───────────────────────────── dunning summary ─────────────────────────────

async fn dunning_summary(ctx: &Ctx, tx: &mut Tx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let data: Vec<(
        Uuid,
        Option<String>,
        Option<String>,
        NaiveDate,
        Option<i64>,
        Option<Decimal>,
        Option<Decimal>,
        Option<Decimal>,
        Option<Decimal>,
        Option<Decimal>,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "select id, number, customer_name, posting_date, dunning_level, total_outstanding, interest_amount, dunning_fee, dunning_amount, outstanding_amount, \
             status, letter_sent_at from e_dunning where tenant_id = $1 and company = $2 and posting_date between $3 and $4 and docstatus < 2 \
             and ($5::uuid is null or customer = $5) and ($6::text is null or status = $6) order by posting_date, number limit 10000",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(p.uuid("customer")?)
    .bind(p.str("status"))
    .fetch_all(&mut **tx)
    .await?;
    let mut t = [Decimal::ZERO; 5];
    let rows: Vec<Value> = data
        .into_iter()
        .map(|(did, number, customer, date, level, outstanding, interest, fee, charges, open, status, sent)| {
            for (x, v) in t.iter_mut().zip([outstanding, interest, fee, charges, open]) {
                *x += v.unwrap_or_default();
            }
            json!({"dunning_id": did, "dunning": number, "customer": customer, "posting_date": date, "level": level,
                "total_outstanding": money(outstanding.unwrap_or_default()), "interest": money(interest.unwrap_or_default()), "fee": money(fee.unwrap_or_default()),
                "charges": money(charges.unwrap_or_default()), "charges_outstanding": money(open.unwrap_or_default()), "status": status, "letter_sent_at": sent})
        })
        .collect();
    let count = rows.len();
    Ok(json!({
        "columns": [col("dunning", "Dunning", "data"), col("customer", "Customer", "data"), col("posting_date", "Date", "date"), col("level", "Level", "int"),
            col("total_outstanding", "Overdue", "currency"), col("interest", "Interest", "currency"), col("fee", "Fee", "currency"),
            col("charges", "Fee and Interest", "currency"), col("charges_outstanding", "Unpaid Charges", "currency"), col("status", "Status", "data"),
            col("letter_sent_at", "Letter Sent", "datetime")],
        "rows": rows,
        "totals": {"count": count, "total_outstanding": money(t[0]), "interest": money(t[1]), "fee": money(t[2]), "charges": money(t[3]), "charges_outstanding": money(t[4])},
        "meta": {"from_date": from, "to_date": to},
    }))
}

// ───────────────────────────── subscription metrics ─────────────────────────────

/// Monthly recurring revenue per subscription at the start and end of the range (current plans,
/// in company currency at the end date's rate), new and churned subscriptions, churn rates.
async fn subscription_metrics(k: &Kernel, ctx: &Ctx, tx: &mut Tx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let ids: Vec<Uuid> = sqlx::query_scalar("select id from e_subscription where tenant_id = $1 and company = $2 order by number")
        .bind(ctx.tenant)
        .bind(co.id)
        .fetch_all(&mut **tx)
        .await?;
    let company_currency = currency_id(tx, ctx, &co.currency).await?;
    let sysctx = ferrum_domain::util::sys(ctx);
    let active = |s: &Sub, on: NaiveDate| s.billing_start <= on && s.stop().is_none_or(|x| x >= on);
    let mut rows = Vec::new();
    let (mut mrr_start, mut mrr_end, mut new_mrr, mut churned_mrr) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
    let (mut subs_start, mut subs_churned, mut subs_new) = (0i64, 0i64, 0i64);
    for sid in ids {
        let doc = k.get_tx(&sysctx, tx, "Subscription", sid, false).await?;
        let s = subscription::load(k, ctx, tx, &doc).await?;
        let rate = exchange_rate(k, ctx, tx, s.currency, company_currency, to).await?;
        let mrr: Decimal = s.plans.iter().map(|pl| Interval::monthly(pl.interval, pl.price * pl.qty, pl.count)).sum::<Decimal>() * rate;
        let mrr = round(mrr, co.precision);
        let (at_start, at_end) = (active(&s, from), active(&s, to));
        let churned = at_start && !at_end;
        let is_new = !at_start && at_end;
        if !(at_start || at_end || (s.billing_start <= to && s.stop().is_none_or(|x| x >= from))) {
            continue;
        }
        if at_start {
            mrr_start += mrr;
            subs_start += 1;
        }
        if at_end {
            mrr_end += mrr;
        }
        if churned {
            churned_mrr += mrr;
            subs_churned += 1;
        }
        if is_new {
            new_mrr += mrr;
            subs_new += 1;
        }
        rows.push(json!({"subscription": s.number, "subscription_id": s.id, "customer": doc.get("customer_name"), "status": doc.get("status"),
            "billing_start": s.billing_start, "stop": s.stop(), "mrr": money(mrr), "arr": money(mrr * Decimal::from(12)),
            "movement": if churned { "Churned" } else if is_new { "New" } else if at_end { "Retained" } else { "Started and ended" }}));
    }
    Ok(json!({
        "columns": [col("subscription", "Subscription", "data"), col("customer", "Customer", "data"), col("status", "Status", "data"),
            col("billing_start", "Billing Start", "date"), col("stop", "Ends", "date"), col("movement", "Movement", "data"), col("mrr", "MRR", "currency"),
            col("arr", "ARR", "currency")],
        "rows": rows,
        "totals": {"mrr_start": money(mrr_start), "mrr_end": money(mrr_end), "arr_end": money(mrr_end * Decimal::from(12)), "new_mrr": money(new_mrr),
            "churned_mrr": money(churned_mrr), "subscriptions_start": subs_start, "subscriptions_new": subs_new, "subscriptions_churned": subs_churned,
            "customer_churn_pct": money(pct(Decimal::from(subs_churned), Decimal::from(subs_start))), "revenue_churn_pct": money(pct(churned_mrr, mrr_start))},
        "meta": {"from_date": from, "to_date": to, "note": "MRR uses each subscription's current plans."},
    }))
}

// ───────────────────────────── what needs attention ─────────────────────────────

/// Whether the caller may read every record of an entity of this company (a company scope is fine):
/// what is counted for them is not narrowed to their own.
fn all_of(ctx: &Ctx, entity: &str) -> bool {
    let Some(ent) = ctx.schema.entity(entity) else { return false };
    row_scope(ctx, ent, Action::Read).is_ok_and(|s| s.grants.is_empty() && s.conds.iter().all(|c| c.field.as_deref() == Some("company")))
}

/// A finance lead's morning check in one answer, one row each: what is overdue and what falls due
/// this week (receivables and payables, as the ageing reports count them), bank statement lines not
/// reconciled, drafts waiting to be submitted, months that have ended and are not closed, years
/// that have ended and are not closed, and the cash there is. A row names what it counts (`key`),
/// how many, how much when that means something, the oldest date, and the parts (`detail`) a screen
/// links to.
async fn overview(ctx: &Ctx, tx: &mut Tx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let as_of = p.date("as_of")?.unwrap_or_else(today);
    let week = as_of + Duration::days(7);
    let mut rows: Vec<Value> = Vec::new();
    for (kind, overdue_key, week_key, overdue_label, week_label) in [
        ("customer", "overdue_receivables", "receivables_due_week", "Overdue receivables", "Receivables due this week"),
        ("supplier", "overdue_payables", "payables_due_week", "Overdue payables", "Payables due this week"),
    ] {
        for (key, label, from, to) in [(overdue_key, overdue_label, None, as_of - Duration::days(1)), (week_key, week_label, Some(as_of), week)] {
            let (n, amount, oldest): (i64, Decimal, Option<NaiveDate>) = sqlx::query_as(
                "select count(*), coalesce(sum(o.base_outstanding), 0), min(coalesce(o.due_date, o.posting_date)) from open_item o \
                 where o.tenant_id = $1 and o.company_id = $2 and o.party_kind = $3 and o.status = 'open' and o.outstanding > 0 \
                 and coalesce(o.due_date, o.posting_date) <= $5 and ($4::date is null or coalesce(o.due_date, o.posting_date) >= $4)",
            )
            .bind(ctx.tenant)
            .bind(co.id)
            .bind(kind)
            .bind(from)
            .bind(to)
            .fetch_one(&mut **tx)
            .await?;
            rows.push(json!({"key": key, "item": label, "count": n, "amount": money(amount), "oldest": oldest}));
        }
    }
    if all_of(ctx, "BankTransaction") {
        let lines: Vec<(Uuid, String, i64, Option<NaiveDate>)> = sqlx::query_as(
            "select b.id, b.account_name, count(t.id), min(t.date) from e_bank_account b \
             left join e_bank_transaction t on t.tenant_id = b.tenant_id and t.bank_account = b.id and t.docstatus = 1 and t.unallocated_amount > 0 and t.date <= $3 \
             where b.tenant_id = $1 and b.company = $2 and not coalesce(b.disabled, false) group by b.id, b.account_name order by b.account_name",
        )
        .bind(ctx.tenant)
        .bind(co.id)
        .bind(as_of)
        .fetch_all(&mut **tx)
        .await?;
        let total: i64 = lines.iter().map(|l| l.2).sum();
        rows.push(json!({"key": "unreconciled_lines", "item": "Bank lines to reconcile", "count": total, "oldest": lines.iter().filter_map(|l| l.3).min(),
            "detail": lines.iter().map(|(id, name, n, oldest)| json!({"id": id, "name": name, "count": n, "oldest": oldest})).collect::<Vec<_>>()}));
    }
    let mut drafts = Vec::new();
    for entity in ["SalesInvoice", "PurchaseInvoice", "PaymentEntry", "JournalEntry", "PaymentAllocation", "ExchangeRevaluation"] {
        let Some(ent) = ctx.schema.entity(entity).filter(|_| all_of(ctx, entity)).cloned() else { continue };
        let (Some(c), Some(d)) = (ferrum_kernel::store::field_expr(&ent, "company", "a"), ferrum_kernel::store::field_expr(&ent, "posting_date", "a")) else {
            continue;
        };
        let sql = format!("select count(*), min({d}) from {} a where a.tenant_id = $1 and {c} = $2 and a.docstatus = 0", ferrum_kernel::ddl::q(&ent.table));
        let (n, oldest): (i64, Option<NaiveDate>) = sqlx::query_as(sqlx::AssertSqlSafe(sql)).bind(ctx.tenant).bind(co.id).fetch_one(&mut **tx).await?;
        if n > 0 {
            drafts.push(json!({"entity": entity, "label": if n == 1 { ent.def.label.clone() } else { ent.def.plural_label() }, "count": n, "oldest": oldest}));
        }
    }
    let waiting: i64 = drafts.iter().filter_map(|d| d["count"].as_i64()).sum();
    rows.push(json!({"key": "drafts", "item": "Drafts waiting to be submitted", "count": waiting,
        "oldest": drafts.iter().filter_map(|d| d["oldest"].as_str()).min(), "detail": drafts}));
    // Months that have ended and are still open, and the years that have ended with nothing closing them.
    let months: Vec<(Uuid, NaiveDate, NaiveDate)> = sqlx::query_as(
        "select f.id, p.period_start, p.period_end from accounting_period p join e_fiscal_year f on f.tenant_id = p.tenant_id and f.company = p.company_id \
         and p.period_start between f.start_date and f.end_date where p.tenant_id = $1 and p.company_id = $2 and p.status = 'open' and p.period_end < $3 \
         order by p.period_start",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(as_of)
    .fetch_all(&mut **tx)
    .await?;
    rows.push(json!({"key": "open_months", "item": "Months ended and not closed", "count": months.len(), "oldest": months.first().map(|m| m.1),
        "detail": months.iter().rev().take(12).map(|(fy, start, end)| json!({"fiscal_year": fy, "start": start, "end": end})).collect::<Vec<_>>()}));
    let years: Vec<(Uuid, String, NaiveDate)> = sqlx::query_as(
        "select f.id, f.year_name, f.end_date from e_fiscal_year f where f.tenant_id = $1 and f.company = $2 and f.end_date < $3 \
         and exists (select 1 from journal_entry j where j.tenant_id = f.tenant_id and j.company_id = f.company and j.posting_date between f.start_date and f.end_date) \
         and not exists (select 1 from e_period_closing_voucher v where v.tenant_id = f.tenant_id and v.fiscal_year = f.id and v.docstatus = 1) order by f.end_date",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(as_of)
    .fetch_all(&mut **tx)
    .await?;
    rows.push(json!({"key": "unclosed_years", "item": "Fiscal years ended and not closed", "count": years.len(), "oldest": years.first().map(|y| y.2),
        "detail": years.iter().map(|(id, name, end)| json!({"id": id, "name": name, "end": end})).collect::<Vec<_>>()}));
    let cash: Vec<(Uuid, String, Decimal)> = sqlx::query_as(
        "select a.id, coalesce(a.account_number || ' - ', '') || a.account_name, coalesce(sum(l.debit - l.credit), 0) from e_account a \
         left join journal_line_in_book l on l.tenant_id = a.tenant_id and l.account_id = a.id and l.posting_date <= $3 \
         where a.tenant_id = $1 and a.company = $2 and a.account_type in ('Bank', 'Cash') and not coalesce(a.is_group, false) group by a.id, 2 order by 2",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(as_of)
    .fetch_all(&mut **tx)
    .await?;
    rows.push(json!({"key": "cash", "item": "Cash and bank", "amount": money(cash.iter().map(|c| c.2).sum()),
        "detail": cash.iter().map(|(id, name, balance)| json!({"id": id, "name": name, "balance": money(*balance)})).collect::<Vec<_>>()}));
    Ok(json!({
        "columns": [col("item", "What", "data"), col("count", "How many", "int"), col("amount", "Amount", "currency"), col("oldest", "Oldest", "date")],
        "rows": rows,
        "meta": {"as_of": as_of},
    }))
}
