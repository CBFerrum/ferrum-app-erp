//! ERP's reports (the books and stock: trial balance to purchase register), computed straight
//! from the ledgers (journal lines, open items, stock moves) inside one read-only,
//! tenant-isolated transaction. The report framework they plug into is ferrum-domain's.

use crate::periods::{self, Compare};
use crate::util::money;
use chrono::NaiveDate;
use ferrum_domain::reports::{Params, ReportCompany, ReportSource, col, fy_start, param_def, period, read_tx, resolve_company, today};
use ferrum_kernel::{Ctx, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

const MAX_ROWS: i64 = 10_000;

struct Def {
    name: &'static str,
    label: &'static str,
    module: &'static str,
    /// Reading this entity is what the report requires.
    entity: &'static str,
    params: &'static [&'static str],
}

const DEFS: &[Def] = &[
    Def {
        name: "trial_balance",
        label: "Trial Balance",
        module: "accounting",
        entity: "JournalEntry",
        params: &["company", "from_date", "to_date", "finance_book"],
    },
    Def {
        name: "consolidated_trial_balance",
        label: "Consolidated Trial Balance",
        module: "accounting",
        entity: "JournalEntry",
        params: &["company", "from_date", "to_date", "finance_book"],
    },
    Def {
        name: "general_ledger",
        label: "General Ledger",
        module: "accounting",
        entity: "JournalEntry",
        params: &["company", "from_date", "to_date", "account", "customer", "supplier", "cost_center", "project", "exclude_closing", "finance_book"],
    },
    Def {
        name: "profit_and_loss",
        label: "Profit and Loss",
        module: "accounting",
        entity: "JournalEntry",
        params: &["company", "from_date", "to_date", "compare", "periods", "cost_center", "project", "finance_book", "consolidated"],
    },
    Def {
        name: "balance_sheet",
        label: "Balance Sheet",
        module: "accounting",
        entity: "JournalEntry",
        params: &["company", "as_of", "compare", "periods", "finance_book", "consolidated"],
    },
    Def {
        name: "cash_flow",
        label: "Cash Flow",
        module: "accounting",
        entity: "JournalEntry",
        params: &["company", "from_date", "to_date", "compare", "periods", "finance_book"],
    },
    Def {
        name: "deferred_revenue_and_expense",
        label: "Deferred Revenue and Expense",
        module: "accounting",
        entity: "JournalEntry",
        params: &["company", "deferred", "as_of"],
    },
    Def { name: "receivables", label: "Accounts Receivable", module: "selling", entity: "SalesInvoice", params: &["company", "as_of", "customer"] },
    Def { name: "payables", label: "Accounts Payable", module: "buying", entity: "PurchaseInvoice", params: &["company", "as_of", "supplier"] },
    Def { name: "stock_balance", label: "Stock Balance", module: "inventory", entity: "StockEntry", params: &["company", "as_of", "warehouse", "item"] },
    Def {
        name: "stock_ledger",
        label: "Stock Ledger",
        module: "inventory",
        entity: "StockEntry",
        params: &["company", "from_date", "to_date", "warehouse", "item"],
    },
    Def {
        name: "sales_register",
        label: "Sales Register",
        module: "selling",
        entity: "SalesInvoice",
        params: &["company", "from_date", "to_date", "customer"],
    },
    Def {
        name: "purchase_register",
        label: "Purchase Register",
        module: "buying",
        entity: "PurchaseInvoice",
        params: &["company", "from_date", "to_date", "supplier"],
    },
];

/// The parameters that are not the framework's standard ones.
fn param(name: &str) -> Value {
    match name {
        "compare" => json!({"name": name, "label": "Compare With", "type": "select", "options": ["Previous period", "Previous year"]}),
        "periods" => json!({"name": name, "label": "How Many To Compare", "type": "select", "options": ["1", "2", "3"], "default": "1"}),
        "cost_center" => json!({"name": name, "label": "Cost Center", "type": "link", "to": "CostCenter"}),
        "project" => json!({"name": name, "label": "Project", "type": "link", "to": "Project"}),
        "exclude_closing" => json!({"name": name, "label": "Leave Out The Year-End Closing", "type": "check"}),
        "deferred" => json!({"name": name, "label": "Deferred", "type": "select", "options": ["Revenue", "Expense"], "default": "Revenue"}),
        "consolidated" => json!({"name": name, "label": "Consolidated", "type": "check",
            "description": "The company and every company under it, in its currency, with their business with each other taken out"}),
        "finance_book" => {
            json!({"name": name, "label": "Finance Book", "type": "link", "to": "FinanceBook", "description": "Empty: the common books. A book: the common entries and that book's own"})
        }
        other => param_def(other),
    }
}

/// ERP's reports. Each sums up every record of its entity, so it needs access to all of them
/// (a company scope is fine: reports run per company and check it).
pub struct ErpReports;

#[async_trait::async_trait]
impl ReportSource for ErpReports {
    fn catalog(&self) -> Vec<Value> {
        DEFS.iter()
            .map(|d| {
                json!({"name": d.name, "label": d.label, "module": d.module, "entity": d.entity, "scoped": false,
                       "params": d.params.iter().map(|p| param(p)).collect::<Vec<_>>()})
            })
            .collect()
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
        let def = DEFS.iter().find(|d| d.name == name).ok_or_else(|| Error::NotFound(format!("report {name}")))?;
        let p = Params { raw };
        let mut tx = read_tx(k, ctx).await?;
        let co = resolve_company(&mut tx, ctx, &p).await?;
        let book = p.uuid("finance_book")?;
        crate::ledger::read_book(&mut tx, book).await?;
        let book_name: Option<String> = match book {
            Some(b) => {
                sqlx::query_scalar("select finance_book_name from e_finance_book where tenant_id = $1 and id = $2")
                    .bind(ctx.tenant)
                    .bind(b)
                    .fetch_optional(&mut *tx)
                    .await?
            }
            None => None,
        };
        let mut out = match name {
            "trial_balance" => trial_balance(&mut tx, ctx, &co, &p).await?,
            "consolidated_trial_balance" => consolidated_trial_balance(k, &mut tx, ctx, &co, &p).await?,
            "general_ledger" => general_ledger(&mut tx, ctx, &co, &p).await?,
            "profit_and_loss" => profit_and_loss(k, &mut tx, ctx, &co, &p).await?,
            "balance_sheet" => balance_sheet(k, &mut tx, ctx, &co, &p).await?,
            "cash_flow" => cash_flow(&mut tx, ctx, &co, &p).await?,
            "deferred_revenue_and_expense" => deferred_lines(&mut tx, ctx, &co, &p).await?,
            "receivables" => ageing(&mut tx, ctx, &co, &p, "customer").await?,
            "payables" => ageing(&mut tx, ctx, &co, &p, "supplier").await?,
            "stock_balance" => stock_balance(&mut tx, ctx, &co, &p).await?,
            "stock_ledger" => stock_ledger(&mut tx, ctx, &co, &p).await?,
            "sales_register" => register(&mut tx, ctx, &co, &p, "customer").await?,
            "purchase_register" => register(&mut tx, ctx, &co, &p, "supplier").await?,
            _ => return Err(Error::NotFound(format!("report {name}"))),
        };
        tx.rollback().await?;
        if let Value::Object(m) = &mut out {
            m.insert("name".into(), json!(def.name));
            m.insert("label".into(), json!(def.label));
            let meta = m.entry("meta").or_insert_with(|| json!({}));
            if let Value::Object(meta) = meta {
                meta.insert("company".into(), json!(co.name));
                meta.insert("currency".into(), json!(co.currency));
                if let Some(b) = &book_name {
                    meta.insert("finance_book".into(), json!(b));
                }
            }
        }
        Ok(out)
    }
}

// ───────────────────────────── accounts ─────────────────────────────

#[derive(sqlx::FromRow, Clone)]
struct Acc {
    id: Uuid,
    parent_account: Option<Uuid>,
    account_name: String,
    account_number: Option<String>,
    is_group: Option<bool>,
    root_type: String,
    account_type: Option<String>,
    cash_flow_section: Option<String>,
}

fn natural(root: &str, debit_minus_credit: Decimal) -> Decimal {
    if matches!(root, "Asset" | "Expense") { debit_minus_credit } else { -debit_minus_credit }
}

fn label(a: &Acc) -> String {
    label_of(a.account_number.as_deref(), &a.account_name)
}

fn label_of(number: Option<&str>, name: &str) -> String {
    match number {
        Some(n) if !n.is_empty() => format!("{n} - {name}"),
        _ => name.to_string(),
    }
}

async fn accounts(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany) -> Result<Vec<Acc>> {
    Ok(sqlx::query_as::<_, Acc>(
        "select id, parent_account, account_name, account_number, is_group, root_type, account_type, cash_flow_section from e_account \
         where tenant_id = $1 and company = $2 order by account_number nulls last, account_name",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .fetch_all(&mut **tx)
    .await?)
}

/// Debit-minus-credit per account over `[from, to]` (from = None: since the beginning).
async fn balances(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, from: Option<NaiveDate>, to: NaiveDate) -> Result<HashMap<Uuid, Decimal>> {
    balances_where(tx, ctx, co, from, to, &[], &Dims::default()).await
}

/// What a statement of income and expense can be narrowed to besides its period: a cost center
/// (with every cost center below it) and a project, the two dimensions a ledger line carries.
#[derive(Default)]
struct Dims {
    cost_centers: Option<Vec<Uuid>>,
    project: Option<Uuid>,
}

async fn dims(tx: &mut Tx, ctx: &Ctx, p: &Params<'_>) -> Result<Dims> {
    let cost_centers = match p.uuid("cost_center")? {
        Some(root) => Some(
            sqlx::query_scalar(
                "with recursive t(id, depth) as (select id, 0 from e_cost_center where tenant_id = $1 and id = $2 \
                 union all select c.id, t.depth + 1 from e_cost_center c join t on c.parent_cost_center = t.id where c.tenant_id = $1 and t.depth < 64) \
                 select id from t",
            )
            .bind(ctx.tenant)
            .bind(root)
            .fetch_all(&mut **tx)
            .await?,
        ),
        None => None,
    };
    Ok(Dims { cost_centers, project: p.uuid("project")? })
}

/// Entries that close a year's income and expense into equity (the Finance module's Period
/// Closing Voucher). An income statement leaves them out, or a closed year would show no profit;
/// balance sheets and trial balances keep them.
pub const CLOSING_SOURCES: &[&str] = &["PeriodClosingVoucher"];

/// Account balances over a period, leaving out entries from the `skip` source entities and lines
/// outside the dimensions.
async fn balances_where(
    tx: &mut Tx,
    ctx: &Ctx,
    co: &ReportCompany,
    from: Option<NaiveDate>,
    to: NaiveDate,
    skip: &[&str],
    dims: &Dims,
) -> Result<HashMap<Uuid, Decimal>> {
    Ok(sqlx::query_as::<_, (Uuid, Decimal)>(
        "select l.account_id, sum(l.debit - l.credit) from journal_line_in_book l where l.tenant_id = $1 and l.company_id = $2 \
         and ($3::date is null or l.posting_date >= $3) and l.posting_date <= $4 \
         and (cardinality($5::text[]) = 0 or not exists (select 1 from journal_entry e where e.tenant_id = l.tenant_id and e.id = l.entry_id and e.source_entity = any($5))) \
         and ($6::uuid[] is null or l.cost_center_id = any($6)) and ($7::uuid is null or l.project_id = $7) \
         group by l.account_id",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(skip.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    .bind(&dims.cost_centers)
    .bind(dims.project)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .collect())
}

/// One line of an indented account tree: the account, and its balance in each period compared,
/// rolled up from its children.
struct Node {
    account: String,
    id: Uuid,
    depth: usize,
    group: bool,
    amounts: Vec<Decimal>,
}

/// Indented account tree for one root type, with balances rolled up to groups, one amount per
/// period of `bals` (each a period's debit-minus-credit per account). An account shows when it had
/// activity in any of the periods.
fn tree(accs: &[Acc], bals: &[HashMap<Uuid, Decimal>], root: &str, precision: u32) -> (Vec<Node>, Vec<Decimal>) {
    let mut children: HashMap<Option<Uuid>, Vec<&Acc>> = HashMap::new();
    for a in accs.iter().filter(|a| a.root_type == root) {
        let parent = a.parent_account.filter(|p| accs.iter().any(|x| x.id == *p && x.root_type == root));
        children.entry(parent).or_default().push(a);
    }
    fn total(a: &Acc, children: &HashMap<Option<Uuid>, Vec<&Acc>>, bal: &HashMap<Uuid, Decimal>, depth: usize) -> Decimal {
        if depth > 64 {
            return Decimal::ZERO;
        }
        let own = natural(&a.root_type, bal.get(&a.id).copied().unwrap_or_default());
        own + children.get(&Some(a.id)).map(|cs| cs.iter().map(|c| total(c, children, bal, depth + 1)).sum::<Decimal>()).unwrap_or_default()
    }
    fn walk(a: &Acc, depth: usize, children: &HashMap<Option<Uuid>, Vec<&Acc>>, bals: &[HashMap<Uuid, Decimal>], precision: u32, out: &mut Vec<Node>) {
        if depth > 64 {
            return;
        }
        let totals: Vec<Decimal> = bals.iter().map(|b| total(a, children, b, depth)).collect();
        let has_activity = bals
            .iter()
            .any(|b| b.contains_key(&a.id) || children.get(&Some(a.id)).is_some_and(|cs| cs.iter().any(|c| total(c, children, b, depth + 1) != Decimal::ZERO)));
        if totals.iter().all(Decimal::is_zero) && !has_activity {
            return;
        }
        out.push(Node {
            account: label(a),
            id: a.id,
            depth,
            group: a.is_group.unwrap_or(false),
            amounts: totals.iter().map(|t| t.round_dp(precision)).collect(),
        });
        for c in children.get(&Some(a.id)).cloned().unwrap_or_default() {
            walk(c, depth + 1, children, bals, precision, out);
        }
    }
    let mut out = Vec::new();
    let mut sums = vec![Decimal::ZERO; bals.len()];
    for r in children.get(&None).cloned().unwrap_or_default() {
        for (sum, bal) in sums.iter_mut().zip(bals) {
            *sum += total(r, &children, bal, 0);
        }
        walk(r, 0, &children, bals, precision, &mut out);
    }
    (out, sums)
}

/// A statement's periods, the one asked for first and then those before it (`compare` and
/// `periods` say how many, up to three).
fn spans(p: &Params<'_>, from: NaiveDate, to: NaiveDate) -> Result<Vec<(NaiveDate, NaiveDate)>> {
    let Some(how) = compare(p)? else { return Ok(vec![(from, to)]) };
    Ok(std::iter::once((from, to)).chain((1..=count(p)).map(|k| periods::before(from, to, how, k))).collect())
}

/// The days of a balance sheet and its comparisons.
fn days(p: &Params<'_>, on: NaiveDate) -> Result<Vec<NaiveDate>> {
    let Some(how) = compare(p)? else { return Ok(vec![on]) };
    Ok(std::iter::once(on).chain((1..=count(p)).map(|k| periods::day_before(on, how, k))).collect())
}

fn compare(p: &Params<'_>) -> Result<Option<Compare>> {
    match p.str("compare") {
        None => Ok(None),
        Some(text) => Compare::parse(text).map(Some).ok_or_else(|| Error::field("compare", "choose Previous period or Previous year")),
    }
}

fn count(p: &Params<'_>) -> u32 {
    p.str("periods").and_then(|v| v.parse::<u32>().ok()).unwrap_or(1).clamp(1, 3)
}

/// The key of the amount of the `i`th period in a row: `amount`, then `amount_1`, `amount_2`...
fn key(i: usize) -> String {
    if i == 0 { "amount".into() } else { format!("amount_{i}") }
}

/// Puts a row's amounts in it, one for each period, and with comparisons how much the first moved
/// from the second and by what percentage of it.
fn amounts(row: &mut Value, a: &[Decimal]) {
    for (i, v) in a.iter().enumerate() {
        row[key(i)] = money(*v);
    }
    if a.len() > 1 {
        let change = a[0] - a[1];
        row["change"] = money(change);
        row["change_pct"] = if a[1].is_zero() { Value::Null } else { money((change / a[1].abs() * Decimal::ONE_HUNDRED).round_dp(2)) };
    }
}

fn node_rows(nodes: &[Node], indent: usize) -> Vec<Value> {
    nodes
        .iter()
        .map(|n| {
            let mut row = json!({"account": n.account, "account_id": n.id, "_indent": n.depth + indent, "_bold": n.group});
            amounts(&mut row, &n.amounts);
            row
        })
        .collect()
}

fn total_row(label: &str, a: &[Decimal]) -> Value {
    let mut row = json!({"account": label, "_bold": true});
    amounts(&mut row, a);
    row
}

/// The columns of a statement of accounts: the account, an amount for each period and, with more
/// than one, how much the first moved.
fn statement_columns(labels: &[String]) -> Vec<Value> {
    let mut cols = vec![col("account", "Account", "data")];
    if labels.len() == 1 {
        cols.push(col("amount", "Amount", "currency"));
        return cols;
    }
    cols.extend(labels.iter().enumerate().map(|(i, l)| col(&key(i), l, "currency")));
    cols.push(col("change", "Change", "currency"));
    cols.push(col("change_pct", "Change %", "percent"));
    cols
}

async fn trial_balance(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let year_start = fy_start(from, co.fy_month);
    let accs = accounts(tx, ctx, co).await?;
    let before = balances(tx, ctx, co, None, from.pred_opt().unwrap_or(from)).await?;
    // Income and expense restart every fiscal year; earlier years show as one equity line.
    let pl_this_year = balances(tx, ctx, co, Some(year_start), from.pred_opt().unwrap_or(from)).await?;
    let movement: HashMap<Uuid, (Decimal, Decimal)> = sqlx::query_as::<_, (Uuid, Decimal, Decimal)>(
        "select account_id, sum(debit), sum(credit) from journal_line_in_book where tenant_id = $1 and company_id = $2 and posting_date between $3 and $4 group by account_id",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|(a, dr, cr)| (a, (dr, cr)))
    .collect();
    let has_before = from > NaiveDate::MIN;
    let mut tb = Tb::default();
    let mut prior_pl = Decimal::ZERO;
    for a in accs.iter().filter(|a| !a.is_group.unwrap_or(false)) {
        let pl = matches!(a.root_type.as_str(), "Income" | "Expense");
        let opening = if pl {
            let all = before.get(&a.id).copied().unwrap_or_default();
            let this = pl_this_year.get(&a.id).copied().unwrap_or_default();
            prior_pl += all - this;
            this
        } else {
            before.get(&a.id).copied().unwrap_or_default()
        };
        let (dr, cr) = movement.get(&a.id).copied().unwrap_or_default();
        if opening.is_zero() && dr.is_zero() && cr.is_zero() {
            continue;
        }
        tb.push(label(a), &a.root_type, opening, dr, cr);
    }
    if has_before && !prior_pl.is_zero() {
        tb.push(EARLIER_YEARS.into(), "Equity", prior_pl, Decimal::ZERO, Decimal::ZERO);
    }
    Ok(tb.report(json!({"from_date": from, "to_date": to})))
}

const EARLIER_YEARS: &str = "Profit/loss of earlier fiscal years (not yet closed)";

/// A trial balance being written: its rows and the totals of its six amount columns.
#[derive(Default)]
struct Tb {
    rows: Vec<Value>,
    totals: [Decimal; 6],
}

impl Tb {
    /// Adds an account's opening (debit minus credit) and its debits and credits in the period,
    /// the opening and the closing they make each shown as a debit or a credit.
    fn push(&mut self, name: String, root: &str, opening: Decimal, dr: Decimal, cr: Decimal) {
        let closing = opening + dr - cr;
        let cells = [opening.max(Decimal::ZERO), (-opening).max(Decimal::ZERO), dr, cr, closing.max(Decimal::ZERO), (-closing).max(Decimal::ZERO)];
        for (t, c) in self.totals.iter_mut().zip(cells) {
            *t += c;
        }
        self.rows.push(json!({
            "account": name, "root_type": root,
            "opening_debit": money(cells[0]), "opening_credit": money(cells[1]),
            "debit": money(cells[2]), "credit": money(cells[3]),
            "closing_debit": money(cells[4]), "closing_credit": money(cells[5]),
        }));
    }

    fn report(self, mut meta: Value) -> Value {
        let t = self.totals;
        meta["balanced"] = json!(t[0] == t[1] && t[2] == t[3] && t[4] == t[5]);
        let names = ["opening_debit", "opening_credit", "debit", "credit", "closing_debit", "closing_credit"];
        let total_obj: serde_json::Map<String, Value> = names.iter().zip(t).map(|(n, t)| (n.to_string(), money(t))).collect();
        json!({
            "columns": [col("account", "Account", "data"), col("root_type", "Type", "data"), col("opening_debit", "Opening (Dr)", "currency"),
                col("opening_credit", "Opening (Cr)", "currency"), col("debit", "Debit", "currency"), col("credit", "Credit", "currency"),
                col("closing_debit", "Closing (Dr)", "currency"), col("closing_credit", "Closing (Cr)", "currency")],
            "rows": self.rows,
            "totals": total_obj,
            "meta": meta,
        })
    }
}

// ───────────────────────────── groups of companies ─────────────────────────────

/// A company and every company under it, the parent first (a company is counted once however the
/// tree is drawn). Adding up a group's books takes being allowed to see each of its companies.
struct Group {
    /// (id, name, currency, currency code).
    members: Vec<(Uuid, String, Uuid, String)>,
    ids: Vec<Uuid>,
}

impl Group {
    async fn of(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany) -> Result<Group> {
        let rows: Vec<(Uuid, String, Option<Uuid>, Option<String>)> = sqlx::query_as(
            "with recursive g(id) as (select $2::uuid union select c.id from e_company c join g on c.parent_company = g.id where c.tenant_id = $1) \
             select c.id, c.company_name, c.default_currency, trim(cur.code) from g join e_company c on c.tenant_id = $1 and c.id = g.id \
             left join e_currency cur on cur.tenant_id = c.tenant_id and cur.id = c.default_currency order by c.id <> $2, c.company_name",
        )
        .bind(ctx.tenant)
        .bind(co.id)
        .fetch_all(&mut **tx)
        .await?;
        if !ctx.is_admin
            && !ctx.is_system
            && let Some(allowed) = ctx.scopes.get("Company")
            && let Some((_, name, ..)) = rows.iter().find(|m| !allowed.contains(&m.0))
        {
            return Err(Error::Forbidden(format!("you do not have access to {name}, a company of this group")));
        }
        let members = rows
            .into_iter()
            .map(|(id, name, currency, code)| match currency {
                Some(c) => Ok((id, name, c, code.unwrap_or_default())),
                None => Err(Error::invalid(format!("{name} has no default currency"))),
            })
            .collect::<Result<Vec<_>>>()?;
        let ids = members.iter().map(|m| m.0).collect();
        Ok(Group { members, ids })
    }

    /// Each company's rate into the parent's currency: its latest on or before `on`.
    async fn rates(&self, k: &Kernel, tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, on: NaiveDate, field: &str) -> Result<HashMap<Uuid, Decimal>> {
        let mut rates = HashMap::new();
        for (id, name, currency, code) in &self.members {
            let rate = match crate::util::exchange_rate(k, ctx, tx, *currency, self.members[0].2, on).await {
                Err(Error::Validation(_)) => {
                    return Err(Error::field(
                        field,
                        format!("{name}: no exchange rate from {code} to {} on or before {on}; add one under Exchange Rate", co.currency),
                    ));
                }
                other => other?,
            };
            rates.insert(*id, rate);
        }
        Ok(rates)
    }

    fn position(&self, company: Uuid) -> usize {
        self.members.iter().position(|m| m.0 == company).unwrap_or(usize::MAX)
    }

    fn names(&self) -> String {
        self.members.iter().map(|m| m.1.as_str()).collect::<Vec<_>>().join(", ")
    }

    /// The group, when the report is asked for one (`consolidated`).
    async fn asked(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Option<Group>> {
        if !p.str("consolidated").is_some_and(|v| v == "true" || v == "1") {
            return Ok(None);
        }
        if p.str("cost_center").is_some() || p.str("project").is_some() {
            return Err(Error::field("consolidated", "a cost center or a project is one company's; leave them empty to add up the group"));
        }
        Ok(Some(Group::of(tx, ctx, co).await?))
    }
}

// The SQL every group figure shares: the lines of the group's companies with their account and
// party ($1 the tenant, $2 the group's companies, $3 its parent).
macro_rules! group_lines {
    () => {
        "journal_line_in_book l join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
         left join e_customer c on c.tenant_id = l.tenant_id and c.id = l.customer_id \
         left join e_supplier s on s.tenant_id = l.tenant_id and s.id = l.supplier_id"
    };
}

/// The group company a line's customer or supplier represents.
macro_rules! party_company {
    () => {
        "case when coalesce(c.represents_company, s.represents_company) = any($2) then coalesce(c.represents_company, s.represents_company) end"
    };
}

/// Whether a line on an account marked Eliminate in Consolidation is business within the group:
/// its entry is with no customer or supplier outside it, and it is not the parent's own equity.
macro_rules! cancels {
    () => {
        "case when a.eliminate_in_consolidation and not (l.company_id = $3 and a.root_type = 'Equity') then not exists ( \
           select 1 from journal_line o left join e_customer oc on oc.tenant_id = o.tenant_id and oc.id = o.customer_id \
           left join e_supplier os on os.tenant_id = o.tenant_id and os.id = o.supplier_id \
           where o.tenant_id = l.tenant_id and o.posting_date = l.posting_date and o.entry_id = l.entry_id \
           and num_nonnulls(o.customer_id, o.supplier_id) = 1 and not coalesce(coalesce(oc.represents_company, os.represents_company) = any($2), false)) \
         else false end"
    };
}

/// The accounts of a group's companies: (id, company, name, number, root type, parent account), in chart order.
type GroupAcc = (Uuid, Uuid, String, Option<String>, String, Option<Uuid>);

async fn group_accounts(tx: &mut Tx, ctx: &Ctx, g: &Group) -> Result<Vec<GroupAcc>> {
    Ok(sqlx::query_as(
        "select id, company, account_name, account_number, root_type, parent_account from e_account where tenant_id = $1 and company = any($2) \
         order by account_number nulls last, account_name",
    )
    .bind(ctx.tenant)
    .bind(&g.ids)
    .fetch_all(&mut **tx)
    .await?)
}

/// The parent's account that the `i`th of `accs` is: the parent's own are themselves, another
/// company's the parent's with the same number, else the same name.
fn same_account(accs: &[GroupAcc], top: Uuid) -> impl Fn(usize) -> Option<usize> + '_ {
    let parents = || accs.iter().enumerate().filter(move |(_, a)| a.1 == top);
    let by_number: HashMap<&str, usize> = parents().filter_map(|(i, a)| a.3.as_deref().filter(|n| !n.is_empty()).map(|n| (n, i))).collect();
    let by_name: HashMap<&str, usize> = parents().map(|(i, a)| (a.2.as_str(), i)).collect();
    move |i| {
        let a = &accs[i];
        if a.1 == top {
            return Some(i);
        }
        a.3.as_deref().filter(|n| !n.is_empty()).and_then(|n| by_number.get(n)).or_else(|| by_name.get(a.2.as_str())).copied()
    }
}

/// Debit minus credit per account of the parent's chart over `[from, to]`, the group added up: each
/// company translated at its rate, business between them eliminated, entries from the `skip`
/// sources left out, and each account shown on the parent's that it is ([`same_account`]) or else on
/// the nearest one above it that is, at the least the parent's top account of its type.
async fn group_balances(
    tx: &mut Tx,
    ctx: &Ctx,
    co: &ReportCompany,
    g: &Group,
    rates: &HashMap<Uuid, Decimal>,
    from: Option<NaiveDate>,
    to: NaiveDate,
    skip: &[&str],
) -> Result<HashMap<Uuid, Decimal>> {
    let accs = group_accounts(tx, ctx, g).await?;
    let index: HashMap<Uuid, usize> = accs.iter().enumerate().map(|(i, a)| (a.0, i)).collect();
    let same = same_account(&accs, co.id);
    let top = |root: &str| accs.iter().find(|a| a.1 == co.id && a.4 == root && a.5.is_none()).map(|a| a.0);
    let shown_on = |i: usize| -> Result<Uuid> {
        let mut j = i;
        for _ in 0..64 {
            if let Some(t) = same(j) {
                return Ok(accs[t].0);
            }
            match accs[j].5.and_then(|p| index.get(&p)) {
                Some(&up) => j = up,
                None => break,
            }
        }
        let a = &accs[i];
        top(&a.4).ok_or_else(|| Error::invalid(format!("{} has no {} account to show {} under", co.name, a.4, label_of(a.3.as_deref(), &a.2))))
    };
    let lines: Vec<(Uuid, Uuid, Decimal)> = sqlx::query_as(concat!(
        "select l.company_id, l.account_id, sum(l.debit - l.credit) from ",
        group_lines!(),
        " where l.tenant_id = $1 and l.company_id = any($2) and ($4::date is null or l.posting_date >= $4) and l.posting_date <= $5 \
          and (",
        party_company!(),
        ") is null and not ",
        cancels!(),
        " and (cardinality($6::text[]) = 0 or not exists (select 1 from journal_entry e where e.tenant_id = l.tenant_id and e.id = l.entry_id and e.source_entity = any($6))) \
          group by 1, 2"
    ))
    .bind(ctx.tenant)
    .bind(&g.ids)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(skip.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    .fetch_all(&mut **tx)
    .await?;
    let mut out: HashMap<Uuid, Decimal> = HashMap::new();
    for (company, account, net) in lines {
        let (Some(rate), Some(&i)) = (rates.get(&company), index.get(&account)) else { continue };
        *out.entry(shown_on(i)?).or_default() += net * rate;
    }
    Ok(out.into_iter().map(|(a, v)| (a, v.round_dp(co.precision))).collect())
}

/// The trial balance of a group: the company asked for and every company under it, each one
/// translated into the parent's currency at its latest rate on or before the end date and shown on
/// the parent's chart (by account number, else by name; an account the parent does not have shows
/// on its own, with its company's name). Business between the group's companies cancels out:
/// lines whose customer or supplier represents one of the group's companies, and lines on accounts
/// marked Eliminate in Consolidation unless their entry is with someone outside the group (a
/// customer or supplier that is not one of its companies). The parent's own equity stays: it is
/// the group's. An entry eliminated only in part (an intercompany sale booked to an account that is
/// not eliminated, say) leaves the intercompany difference; what two companies say they owe each
/// other is set side by side in `meta.intercompany_balances`.
// ponytail: one closing rate per company for every account (as ERPNext does); IAS 21 rates of the
// day for income and expense, historical rates for equity and a translation reserve come when a
// group needs them.
async fn consolidated_trial_balance(k: &Kernel, tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let year_start = fy_start(from, co.fy_month);
    let g = Group::of(tx, ctx, co).await?;
    let rates = g.rates(k, tx, ctx, co, to, "to_date").await?;
    let accs = group_accounts(tx, ctx, &g).await?;
    let index: HashMap<Uuid, usize> = accs.iter().enumerate().map(|(i, a)| (a.0, i)).collect();
    let same = same_account(&accs, co.id);
    // (company, account, the group company the line's party represents, whether it is on an
    // eliminated account in business within the group, the balance before the period, the part of
    // it from this fiscal year, the period's debits and credits, and every debit and credit up to
    // the end).
    let lines: Vec<(Uuid, Uuid, Option<Uuid>, bool, Decimal, Decimal, Decimal, Decimal, Decimal, Decimal)> = sqlx::query_as(concat!(
        "select l.company_id, l.account_id, ",
        party_company!(),
        ", ",
        cancels!(),
        ", coalesce(sum(l.debit - l.credit) filter (where l.posting_date < $4), 0), \
           coalesce(sum(l.debit - l.credit) filter (where l.posting_date >= $5 and l.posting_date < $4), 0), \
           coalesce(sum(l.debit) filter (where l.posting_date >= $4), 0), coalesce(sum(l.credit) filter (where l.posting_date >= $4), 0), \
           sum(l.debit), sum(l.credit) from ",
        group_lines!(),
        " where l.tenant_id = $1 and l.company_id = any($2) and l.posting_date <= $6 group by 1, 2, 3, 4"
    ))
    .bind(ctx.tenant)
    .bind(&g.ids)
    .bind(co.id)
    .bind(from)
    .bind(year_start)
    .bind(to)
    .fetch_all(&mut **tx)
    .await?;
    let mut sums: HashMap<usize, [Decimal; 3]> = HashMap::new();
    let mut eliminated: HashMap<Uuid, (Decimal, Decimal)> = HashMap::new();
    // What a group company's books say another owes it (a negative amount: what it owes the other).
    let mut owed: HashMap<(Uuid, Uuid), Decimal> = HashMap::new();
    let mut prior_pl = Decimal::ZERO;
    for (company, account, with, cancels, before, this_year, dr, cr, all_dr, all_cr) in lines {
        let (Some(&rate), Some(&i)) = (rates.get(&company), index.get(&account)) else { continue };
        if let Some(with) = with {
            *owed.entry((company, with)).or_default() += (all_dr - all_cr) * rate;
        }
        if cancels || with.is_some() {
            let e = eliminated.entry(company).or_default();
            e.0 += all_dr * rate;
            e.1 += all_cr * rate;
            continue;
        }
        // Income and expense restart every fiscal year, as in a company's trial balance.
        let opening = if matches!(accs[i].4.as_str(), "Income" | "Expense") {
            prior_pl += (before - this_year) * rate;
            this_year
        } else {
            before
        };
        let s = sums.entry(same(i).unwrap_or(i)).or_default();
        s[0] += opening * rate;
        s[1] += dr * rate;
        s[2] += cr * rate;
    }
    let mut shown: Vec<usize> = sums.keys().copied().collect();
    shown.sort_by_key(|&i| (g.position(accs[i].1), i));
    let r = |v: Decimal| v.round_dp(co.precision);
    let mut tb = Tb::default();
    for i in shown {
        let [opening, dr, cr] = sums[&i].map(r);
        if opening.is_zero() && dr.is_zero() && cr.is_zero() {
            continue;
        }
        let (_, company, name, number, root, _) = &accs[i];
        let account = label_of(number.as_deref(), name);
        let account = if *company == co.id { account } else { format!("{account} ({})", g.members[g.position(*company)].1) };
        tb.push(account, root, opening, dr, cr);
    }
    if !r(prior_pl).is_zero() {
        tb.push(EARLIER_YEARS.into(), "Equity", r(prior_pl), Decimal::ZERO, Decimal::ZERO);
    }
    // What is left once the group's business with itself is taken out balances only if whole
    // entries were; the rest (and rounding the translated amounts) shows here.
    let t = tb.totals;
    let (opening, movement) = (t[0] - t[1], t[2] - t[3]);
    if !opening.is_zero() || !movement.is_zero() {
        tb.push(INTERCOMPANY_DIFFERENCE.into(), "Equity", -opening, (-movement).max(Decimal::ZERO), movement.max(Decimal::ZERO));
    }
    // Each pair once: what the first says the second owes it, and what the second says it owes.
    let mut balances = Vec::new();
    let mut disagree = Decimal::ZERO;
    for (x, (xid, xname, ..)) in g.members.iter().enumerate() {
        for (yid, yname, ..) in &g.members[x + 1..] {
            let says = r(owed.get(&(*xid, *yid)).copied().unwrap_or_default());
            let other = r(-owed.get(&(*yid, *xid)).copied().unwrap_or_default());
            if says.is_zero() && other.is_zero() {
                continue;
            }
            disagree += (says - other).abs();
            balances.push(json!({"company": xname, "counterparty": yname, "company_says": money(says), "counterparty_says": money(other), "difference": money(says - other)}));
        }
    }
    let companies: Vec<Value> = g
        .members
        .iter()
        .map(|(id, name, _, currency)| {
            let (dr, cr) = eliminated.get(id).copied().unwrap_or_default();
            json!({"company": name, "currency": currency, "rate": money(rates[id]), "eliminated_debit": money(r(dr)), "eliminated_credit": money(r(cr))})
        })
        .collect();
    Ok(tb.report(json!({
        "from_date": from, "to_date": to, "rates_on": to, "companies": companies,
        "intercompany_difference": money(-(opening + movement)),
        "intercompany_balances": balances, "intercompany_balances_disagree_by": money(disagree),
    })))
}

const INTERCOMPANY_DIFFERENCE: &str = "Intercompany difference (entries eliminated in part)";

async fn general_ledger(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    // A group account is every account below it: a figure of a statement opens the entries behind it.
    let accounts: Option<Vec<Uuid>> = match p.uuid("account")? {
        Some(root) => Some(
            sqlx::query_scalar(
                "with recursive t(id, depth) as (select id, 0 from e_account where tenant_id = $1 and company = $2 and id = $3 \
                 union all select a.id, t.depth + 1 from e_account a join t on a.parent_account = t.id where a.tenant_id = $1 and t.depth < 64) select id from t",
            )
            .bind(ctx.tenant)
            .bind(co.id)
            .bind(root)
            .fetch_all(&mut **tx)
            .await?,
        ),
        None => None,
    };
    let dims = dims(tx, ctx, p).await?;
    // The journal keeps the id it was posted under: a merged party is every id it has had.
    let customer = match p.uuid("customer")? {
        Some(id) => Some(ferrum_kernel::merge::merged_ids(tx, ctx.tenant, "Customer", id).await?),
        None => None,
    };
    let supplier = match p.uuid("supplier")? {
        Some(id) => Some(ferrum_kernel::merge::merged_ids(tx, ctx.tenant, "Supplier", id).await?),
        None => None,
    };
    let skip: Vec<String> =
        if p.str("exclude_closing").is_some_and(|v| v == "true" || v == "1") { CLOSING_SOURCES.iter().map(|s| s.to_string()).collect() } else { vec![] };
    let opening: Decimal = sqlx::query_scalar(
        "select coalesce(sum(l.debit - l.credit), 0) from journal_line_in_book l where l.tenant_id = $1 and l.company_id = $2 and l.posting_date < $3 \
         and ($4::uuid[] is null or l.account_id = any($4)) and ($5::uuid[] is null or l.customer_id = any($5)) and ($6::uuid[] is null or l.supplier_id = any($6)) \
         and ($7::uuid[] is null or l.cost_center_id = any($7)) and ($8::uuid is null or l.project_id = $8) \
         and (cardinality($9::text[]) = 0 or not exists (select 1 from journal_entry e where e.tenant_id = l.tenant_id and e.id = l.entry_id and e.source_entity = any($9)))",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(&accounts)
    .bind(&customer)
    .bind(&supplier)
    .bind(&dims.cost_centers)
    .bind(dims.project)
    .bind(&skip)
    .fetch_one(&mut **tx)
    .await?;
    #[allow(clippy::type_complexity)]
    let lines: Vec<(NaiveDate, String, String, Option<String>, Uuid, Option<String>, Decimal, Decimal, Option<String>)> = sqlx::query_as(
        "select l.posting_date, coalesce(a.account_number || ' - ', '') || a.account_name, e.source_entity, e.source_number, e.source_id, \
         coalesce(c.customer_name, s.supplier_name), l.debit, l.credit, l.remarks \
         from journal_line_in_book l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
         join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id \
         left join e_customer c on c.tenant_id = l.tenant_id and c.id = l.customer_id \
         left join e_supplier s on s.tenant_id = l.tenant_id and s.id = l.supplier_id \
         where l.tenant_id = $1 and l.company_id = $2 and l.posting_date between $3 and $4 \
         and ($5::uuid[] is null or l.account_id = any($5)) and ($6::uuid[] is null or l.customer_id = any($6)) and ($7::uuid[] is null or l.supplier_id = any($7)) \
         and ($9::uuid[] is null or l.cost_center_id = any($9)) and ($10::uuid is null or l.project_id = $10) \
         and (cardinality($11::text[]) = 0 or e.source_entity <> all($11)) \
         order by l.posting_date, e.created_at, l.entry_id, l.line_no limit $8",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(&accounts)
    .bind(&customer)
    .bind(&supplier)
    .bind(MAX_ROWS + 1)
    .bind(&dims.cost_centers)
    .bind(dims.project)
    .bind(&skip)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = lines.len() as i64 > MAX_ROWS;
    let mut balance = opening;
    let mut rows = vec![
        json!({"posting_date": from, "account": "Opening", "debit": money(opening.max(Decimal::ZERO)), "credit": money((-opening).max(Decimal::ZERO)), "balance": money(opening), "_bold": true}),
    ];
    let (mut tdr, mut tcr) = (Decimal::ZERO, Decimal::ZERO);
    for (day, acc, entity, number, sid, party, dr, cr, remarks) in lines.into_iter().take(MAX_ROWS as usize) {
        balance += dr - cr;
        tdr += dr;
        tcr += cr;
        rows.push(json!({"posting_date": day, "account": acc, "voucher_type": entity, "voucher_no": number, "voucher_id": sid, "party": party,
            "debit": money(dr), "credit": money(cr), "balance": money(balance), "remarks": remarks}));
    }
    rows.push(json!({"account": "Closing", "debit": money(tdr), "credit": money(tcr), "balance": money(balance), "_bold": true}));
    Ok(json!({
        "columns": [col("posting_date", "Date", "date"), col("account", "Account", "data"), col("voucher_type", "Voucher Type", "data"),
            col("voucher_no", "Voucher No", "data"), col("party", "Party", "data"), col("debit", "Debit", "currency"), col("credit", "Credit", "currency"),
            col("balance", "Balance (Dr - Cr)", "currency"), col("remarks", "Remarks", "data")],
        "rows": rows,
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

/// What each period of a statement came to, for the interface to head its columns with.
fn periods_meta(spans: &[(NaiveDate, NaiveDate)], labels: &[String], figures: &[(&str, &[Decimal])]) -> Vec<Value> {
    spans
        .iter()
        .enumerate()
        .map(|(i, (from, to))| {
            let mut m = json!({"key": key(i), "label": labels[i], "from_date": from, "to_date": to});
            for (name, values) in figures {
                m[*name] = money(values[i]);
            }
            m
        })
        .collect()
}

async fn profit_and_loss(k: &Kernel, tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let spans = spans(p, from, to)?;
    let group = Group::asked(tx, ctx, co, p).await?;
    let dims = dims(tx, ctx, p).await?;
    let accs = accounts(tx, ctx, co).await?;
    let mut bals = Vec::new();
    for (f, t) in &spans {
        bals.push(match &group {
            Some(g) => {
                let rates = g.rates(k, tx, ctx, co, *t, "to_date").await?;
                group_balances(tx, ctx, co, g, &rates, Some(*f), *t, CLOSING_SOURCES).await?
            }
            None => balances_where(tx, ctx, co, Some(*f), *t, CLOSING_SOURCES, &dims).await?,
        });
    }
    let (inc_nodes, income) = tree(&accs, &bals, "Income", co.precision);
    let (exp_nodes, expense) = tree(&accs, &bals, "Expense", co.precision);
    let net: Vec<Decimal> = income.iter().zip(&expense).map(|(i, e)| *i - *e).collect();
    let mut rows = vec![json!({"account": "Income", "_bold": true})];
    rows.extend(node_rows(&inc_nodes, 1));
    rows.push(total_row("Total Income", &income));
    rows.push(json!({"account": "Expenses", "_bold": true}));
    rows.extend(node_rows(&exp_nodes, 1));
    rows.push(total_row("Total Expenses", &expense));
    rows.push(total_row(if net[0] >= Decimal::ZERO { "Net Profit" } else { "Net Loss" }, &net));
    let labels: Vec<String> = spans.iter().map(|(f, t)| periods::label(*f, *t)).collect();
    let mut meta = json!({"from_date": from, "to_date": to, "periods": periods_meta(&spans, &labels, &[("income", &income), ("expense", &expense), ("net_profit", &net)])});
    if let Some(g) = &group {
        meta["companies"] = json!(g.names());
    }
    Ok(json!({
        "columns": statement_columns(&labels),
        "rows": rows,
        "totals": {"income": money(income[0]), "expense": money(expense[0]), "net_profit": money(net[0])},
        "meta": meta,
    }))
}

async fn balance_sheet(k: &Kernel, tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let as_of = p.date("as_of")?.unwrap_or_else(today);
    let on = days(p, as_of)?;
    let group = Group::asked(tx, ctx, co, p).await?;
    let accs = accounts(tx, ctx, co).await?;
    let mut bals = Vec::new();
    for day in &on {
        bals.push(match &group {
            Some(g) => {
                let rates = g.rates(k, tx, ctx, co, *day, "as_of").await?;
                group_balances(tx, ctx, co, g, &rates, None, *day, &[]).await?
            }
            None => balances(tx, ctx, co, None, *day).await?,
        });
    }
    let (asset_nodes, assets) = tree(&accs, &bals, "Asset", co.precision);
    let (liab_nodes, liabilities) = tree(&accs, &bals, "Liability", co.precision);
    let (eq_nodes, equity) = tree(&accs, &bals, "Equity", co.precision);
    let (_, income) = tree(&accs, &bals, "Income", co.precision);
    let (_, expense) = tree(&accs, &bals, "Expense", co.precision);
    let unclosed: Vec<Decimal> = income.iter().zip(&expense).map(|(i, e)| *i - *e).collect();
    let mut all_equity: Vec<Decimal> = equity.iter().zip(&unclosed).map(|(e, u)| *e + *u).collect();
    // A group's books are short by what was eliminated only in part (see the consolidated trial balance).
    let residue: Vec<Decimal> = match group {
        Some(_) => assets.iter().zip(&liabilities).zip(&all_equity).map(|((a, l), e)| *a - *l - *e).collect(),
        None => vec![],
    };
    let residue = residue.iter().any(|r| !r.is_zero()).then_some(residue);
    for (e, r) in all_equity.iter_mut().zip(residue.iter().flatten()) {
        *e += *r;
    }
    let total: Vec<Decimal> = liabilities.iter().zip(&all_equity).map(|(l, e)| *l + *e).collect();
    let mut rows = vec![json!({"account": "Assets", "_bold": true})];
    rows.extend(node_rows(&asset_nodes, 1));
    rows.push(total_row("Total Assets", &assets));
    rows.push(json!({"account": "Liabilities", "_bold": true}));
    rows.extend(node_rows(&liab_nodes, 1));
    rows.push(total_row("Total Liabilities", &liabilities));
    rows.push(json!({"account": "Equity", "_bold": true}));
    rows.extend(node_rows(&eq_nodes, 1));
    let mut open = json!({"account": "Profit/loss not yet closed to equity", "_indent": 1});
    amounts(&mut open, &unclosed);
    rows.push(open);
    if let Some(residue) = &residue {
        let mut row = json!({"account": INTERCOMPANY_DIFFERENCE, "_indent": 1});
        amounts(&mut row, residue);
        rows.push(row);
    }
    rows.push(total_row("Total Equity", &all_equity));
    rows.push(total_row("Total Liabilities and Equity", &total));
    let labels: Vec<String> = on.iter().map(|d| d.format("%d %b %Y").to_string()).collect();
    let spans: Vec<(NaiveDate, NaiveDate)> = on.iter().map(|d| (*d, *d)).collect();
    let mut meta = json!({"as_of": as_of, "balanced": assets[0] == total[0], "difference": money(assets[0] - total[0]),
                          "periods": periods_meta(&spans, &labels, &[("assets", &assets), ("liabilities", &liabilities), ("equity", &all_equity)])});
    if let Some(g) = &group {
        meta["companies"] = json!(g.names());
    }
    Ok(json!({
        "columns": statement_columns(&labels),
        "rows": rows,
        "totals": {"assets": money(assets[0]), "liabilities": money(liabilities[0]), "equity": money(all_equity[0])},
        "meta": meta,
    }))
}

/// Where an account's change shows in the cash flow: the account's own choice, else by what it is
/// (fixed assets are investing, equity is financing, everything else operating).
fn section(a: &Acc) -> &'static str {
    match a.cash_flow_section.as_deref() {
        Some("Investing") => "investing",
        Some("Financing") => "financing",
        Some("Operating") => "operating",
        _ if a.account_type.as_deref() == Some("Fixed Asset") => "investing",
        _ if a.root_type == "Equity" => "financing",
        _ => "operating",
    }
}

/// The cash flow, by the indirect method: the profit of the period, adjusted for what moved in the
/// other accounts of the balance sheet, which the account's type or its own setting sorts into
/// operating, investing and financing activities. It must end where the bank and cash accounts did;
/// `meta.difference` says by how much it does not (it should always be nothing).
async fn cash_flow(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let spans = spans(p, from, to)?;
    let accs = accounts(tx, ctx, co).await?;
    let cash = |a: &Acc| a.root_type == "Asset" && matches!(a.account_type.as_deref(), Some("Bank" | "Cash"));
    struct Figures {
        profit: Decimal,
        /// The cash each balance sheet account's movement brought in (or took out).
        impact: HashMap<Uuid, Decimal>,
        /// The part of the cash accounts' movement that was only revaluation.
        exchange: Decimal,
        opening: Decimal,
        closing: Decimal,
    }
    let mut per: Vec<Figures> = Vec::new();
    for (f, t) in &spans {
        let moves = balances_where(tx, ctx, co, Some(*f), *t, CLOSING_SOURCES, &Dims::default()).await?;
        let moved = |a: &Acc| moves.get(&a.id).copied().unwrap_or_default();
        let profit = -accs.iter().filter(|a| matches!(a.root_type.as_str(), "Income" | "Expense")).map(moved).sum::<Decimal>();
        let impact = accs
            .iter()
            .filter(|a| !cash(a) && matches!(a.root_type.as_str(), "Asset" | "Liability" | "Equity") && !moved(a).is_zero())
            .map(|a| (a.id, -moved(a)))
            .collect();
        let cash_at = |bal: HashMap<Uuid, Decimal>| accs.iter().filter(|a| cash(a)).map(|a| bal.get(&a.id).copied().unwrap_or_default()).sum::<Decimal>();
        let opening = cash_at(balances(tx, ctx, co, None, f.pred_opt().unwrap_or(*f)).await?);
        let closing = cash_at(balances(tx, ctx, co, None, *t).await?);
        let exchange: Decimal = sqlx::query_scalar(
            "select coalesce(sum(l.debit - l.credit), 0) from journal_line_in_book l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
             join e_account a on a.tenant_id = l.tenant_id and a.id = l.account_id where l.tenant_id = $1 and l.company_id = $2 \
             and l.posting_date between $3 and $4 and e.source_entity = 'ExchangeRevaluation' and a.root_type = 'Asset' and a.account_type in ('Bank', 'Cash')",
        )
        .bind(ctx.tenant)
        .bind(co.id)
        .bind(f)
        .bind(t)
        .fetch_one(&mut **tx)
        .await?;
        per.push(Figures { profit, impact, exchange, opening, closing });
    }
    let n = per.len();
    let column = |f: &dyn Fn(&Figures) -> Decimal| -> Vec<Decimal> { per.iter().map(f).collect() };
    let sum_of = |parts: &[Vec<Decimal>]| -> Vec<Decimal> { (0..n).map(|i| parts.iter().map(|c| c[i]).sum()).collect() };
    let section_rows = |which: &str| -> (Vec<Value>, Vec<Decimal>) {
        let mut rows = Vec::new();
        let mut total = vec![Decimal::ZERO; n];
        for a in accs.iter().filter(|a| section(a) == which) {
            let a_amounts: Vec<Decimal> = per.iter().map(|f| f.impact.get(&a.id).copied().unwrap_or_default().round_dp(co.precision)).collect();
            if a_amounts.iter().all(Decimal::is_zero) {
                continue;
            }
            for (t, v) in total.iter_mut().zip(&a_amounts) {
                *t += *v;
            }
            let mut row = json!({"account": label(a), "account_id": a.id, "section": which, "_indent": 1});
            amounts(&mut row, &a_amounts);
            rows.push(row);
        }
        (rows, total)
    };
    let profit = column(&|f| f.profit);
    let unrealised = column(&|f| -f.exchange);
    let exchange = column(&|f| f.exchange);
    let (operating_rows, operating_lines) = section_rows("operating");
    let (investing_rows, investing) = section_rows("investing");
    let (financing_rows, financing) = section_rows("financing");
    let operating = sum_of(&[profit.clone(), operating_lines, unrealised.clone()]);
    let net = sum_of(&[operating.clone(), investing.clone(), financing.clone(), exchange.clone()]);
    let opening = column(&|f| f.opening);
    let closing = column(&|f| f.closing);

    let mut rows = vec![json!({"account": "Cash flow from operating activities", "_bold": true})];
    let mut start = json!({"account": "Profit (loss) for the period", "section": "profit", "_indent": 1});
    amounts(&mut start, &profit);
    rows.push(start);
    rows.extend(operating_rows);
    if unrealised.iter().any(|v| !v.is_zero()) {
        let mut row = json!({"account": "Exchange gain (loss) on cash and bank, not yet realised", "section": "exchange", "_indent": 1});
        amounts(&mut row, &unrealised);
        rows.push(row);
    }
    rows.push(total_row("Net cash from operating activities", &operating));
    rows.push(json!({"account": "Cash flow from investing activities", "_bold": true}));
    rows.extend(investing_rows);
    rows.push(total_row("Net cash from investing activities", &investing));
    rows.push(json!({"account": "Cash flow from financing activities", "_bold": true}));
    rows.extend(financing_rows);
    rows.push(total_row("Net cash from financing activities", &financing));
    if exchange.iter().any(|v| !v.is_zero()) {
        rows.push(total_row("Effect of exchange rates on cash and bank", &exchange));
    }
    rows.push(total_row("Net increase (decrease) in cash", &net));
    let mut at_start = json!({"account": "Cash and bank at the start of the period"});
    amounts(&mut at_start, &opening);
    rows.push(at_start);
    rows.push(total_row("Cash and bank at the end of the period", &closing));
    let difference: Vec<Decimal> = (0..n).map(|i| closing[i] - opening[i] - net[i]).collect();
    let labels: Vec<String> = spans.iter().map(|(f, t)| periods::label(*f, *t)).collect();
    Ok(json!({
        "columns": statement_columns(&labels),
        "rows": rows,
        "totals": {"operating": money(operating[0]), "investing": money(investing[0]), "financing": money(financing[0]), "net_change": money(net[0]),
                   "opening": money(opening[0]), "closing": money(closing[0])},
        "meta": {"from_date": from, "to_date": to, "reconciled": difference[0].is_zero(), "difference": money(difference[0]),
                 "periods": periods_meta(&spans, &labels, &[("operating", &operating), ("investing", &investing), ("financing", &financing), ("net_change", &net),
                                                           ("opening", &opening), ("closing", &closing), ("difference", &difference)])},
    }))
}

// ─────────────────────────── receivables / payables ───────────────────────────

async fn ageing(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>, kind: &str) -> Result<Value> {
    let as_of = p.date("as_of")?.unwrap_or_else(today);
    let party = p.uuid(kind)?;
    #[allow(clippy::type_complexity)]
    let items: Vec<(Uuid, String, Option<String>, NaiveDate, Option<NaiveDate>, Option<String>, String, Decimal, Decimal, Decimal)> = sqlx::query_as(
        "select o.id, o.entity, o.number, o.posting_date, o.due_date, coalesce(c.customer_name, s.supplier_name), o.currency, o.amount, \
         o.amount - coalesce((select sum(x.amount) from allocation x where x.tenant_id = o.tenant_id and x.open_item_id = o.id and x.posting_date <= $3), 0), \
         o.exchange_rate \
         from open_item o left join e_customer c on c.tenant_id = o.tenant_id and c.id = o.customer_id \
         left join e_supplier s on s.tenant_id = o.tenant_id and s.id = o.supplier_id \
         where o.tenant_id = $1 and o.company_id = $2 and o.party_kind = $4 and o.posting_date <= $3 and o.status <> 'cancelled' \
         and ($5::uuid is null or o.customer_id = $5 or o.supplier_id = $5) \
         order by coalesce(o.due_date, o.posting_date), o.posting_date limit $6",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(as_of)
    .bind(kind)
    .bind(party)
    .bind(MAX_ROWS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = items.len() as i64 > MAX_ROWS;
    let buckets = ["not_due", "d1_30", "d31_60", "d61_90", "d90_plus"];
    let mut totals: BTreeMap<&str, Decimal> = buckets.iter().map(|b| (*b, Decimal::ZERO)).collect();
    let mut total_outstanding = Decimal::ZERO;
    let mut rows = Vec::new();
    for (oid, entity, number, posted, due, name, currency, amount, outstanding, rate) in items.into_iter().take(MAX_ROWS as usize) {
        if outstanding.is_zero() {
            continue;
        }
        let base = crate::util::round(outstanding * rate, co.precision);
        let days = (as_of - due.unwrap_or(posted)).num_days();
        let bucket = match days {
            d if d <= 0 => "not_due",
            1..=30 => "d1_30",
            31..=60 => "d31_60",
            61..=90 => "d61_90",
            _ => "d90_plus",
        };
        *totals.entry(bucket).or_default() += base;
        total_outstanding += base;
        let mut row = json!({"party": name, "voucher_type": entity, "voucher_no": number, "voucher_id": oid, "posting_date": posted, "due_date": due,
            "currency": currency, "invoice_amount": money(amount), "outstanding": money(outstanding), "outstanding_base": money(base), "days_overdue": days.max(0)});
        for b in buckets {
            row[b] = money(if b == bucket { base } else { Decimal::ZERO });
        }
        rows.push(row);
    }
    let mut total_obj = serde_json::Map::new();
    total_obj.insert("outstanding_base".into(), money(total_outstanding));
    for (b, v) in totals {
        total_obj.insert(b.into(), money(v));
    }
    Ok(json!({
        "columns": [col("party", if kind == "customer" { "Customer" } else { "Supplier" }, "data"), col("voucher_no", "Voucher", "data"),
            col("posting_date", "Date", "date"), col("due_date", "Due Date", "date"), col("currency", "Currency", "data"),
            col("invoice_amount", "Invoice Amount", "currency"), col("outstanding", "Outstanding", "currency"),
            col("outstanding_base", "Outstanding (Company Currency)", "currency"), col("days_overdue", "Days Overdue", "int"),
            col("not_due", "Not Due", "currency"), col("d1_30", "1-30", "currency"), col("d31_60", "31-60", "currency"),
            col("d61_90", "61-90", "currency"), col("d90_plus", "90+", "currency")],
        "rows": rows,
        "totals": total_obj,
        "meta": {"as_of": as_of, "truncated": truncated},
    }))
}

// ───────────────────────────── stock ─────────────────────────────

async fn stock_balance(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (wh, item) = (p.uuid("warehouse")?, p.uuid("item")?);
    let as_of = p.date("as_of")?;
    let sql = if as_of.is_some() {
        // Historical: sum the append-only moves up to the date.
        "select i.item_code, i.item_name, w.warehouse_name, sum(m.qty), sum(m.value) from stock_move m \
         join e_item i on i.tenant_id = m.tenant_id and i.id = m.item_id join e_warehouse w on w.tenant_id = m.tenant_id and w.id = m.warehouse_id \
         where m.tenant_id = $1 and m.company_id = $2 and ($3::uuid is null or m.warehouse_id = $3) and ($4::uuid is null or m.item_id = $4) \
         and m.posting_date <= $5::date group by i.item_code, i.item_name, w.warehouse_name having sum(m.qty) <> 0 or sum(m.value) <> 0 \
         order by i.item_code, w.warehouse_name limit $6"
    } else {
        "select i.item_code, i.item_name, w.warehouse_name, b.qty, b.value from stock_balance b \
         join e_item i on i.tenant_id = b.tenant_id and i.id = b.item_id join e_warehouse w on w.tenant_id = b.tenant_id and w.id = b.warehouse_id \
         where b.tenant_id = $1 and b.company_id = $2 and ($3::uuid is null or b.warehouse_id = $3) and ($4::uuid is null or b.item_id = $4) \
         and $5::date is null and (b.qty <> 0 or b.value <> 0) order by i.item_code, w.warehouse_name limit $6"
    };
    let data: Vec<(String, String, String, Decimal, Decimal)> =
        sqlx::query_as(sql).bind(ctx.tenant).bind(co.id).bind(wh).bind(item).bind(as_of).bind(MAX_ROWS + 1).fetch_all(&mut **tx).await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let (mut tq, mut tv) = (Decimal::ZERO, Decimal::ZERO);
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|(code, name, whn, qty, value)| {
            tq += qty;
            tv += value;
            let rate = if qty.is_zero() { Decimal::ZERO } else { (value / qty).round_dp(co.precision + 2) };
            json!({"item_code": code, "item_name": name, "warehouse": whn, "qty": money(qty), "valuation_rate": money(rate), "value": money(value)})
        })
        .collect();
    Ok(json!({
        "columns": [col("item_code", "Item", "data"), col("item_name", "Item Name", "data"), col("warehouse", "Warehouse", "data"),
            col("qty", "Quantity", "decimal"), col("valuation_rate", "Valuation Rate", "currency"), col("value", "Value", "currency")],
        "rows": rows,
        "totals": {"qty": money(tq), "value": money(tv)},
        "meta": {"as_of": as_of, "truncated": truncated},
    }))
}

async fn stock_ledger(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let (wh, item) = (p.uuid("warehouse")?, p.uuid("item")?);
    #[allow(clippy::type_complexity)]
    let data: Vec<(NaiveDate, String, String, Decimal, Decimal, Decimal, Decimal, Decimal, String, Uuid, Option<String>, Option<String>, bool)> = sqlx::query_as(
        "select m.posting_date, i.item_code, w.warehouse_name, m.qty, m.valuation_rate, m.value, m.qty_after, m.value_after, m.source_entity, m.source_id, \
         m.batch_no, m.serial_no, m.reversal_of is not null \
         from stock_move m join e_item i on i.tenant_id = m.tenant_id and i.id = m.item_id join e_warehouse w on w.tenant_id = m.tenant_id and w.id = m.warehouse_id \
         where m.tenant_id = $1 and m.company_id = $2 and m.posting_date between $3 and $4 and ($5::uuid is null or m.warehouse_id = $5) \
         and ($6::uuid is null or m.item_id = $6) order by m.posting_ts, m.seq limit $7",
    )
    .bind(ctx.tenant)
    .bind(co.id)
    .bind(from)
    .bind(to)
    .bind(wh)
    .bind(item)
    .bind(MAX_ROWS + 1)
    .fetch_all(&mut **tx)
    .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|(day, code, whn, qty, rate, value, qa, va, entity, sid, batch, serial, reversal)| {
            json!({"posting_date": day, "item_code": code, "warehouse": whn, "qty": money(qty), "valuation_rate": money(rate.round_dp(9)), "value": money(value),
                "qty_after": money(qa), "value_after": money(va), "voucher_type": entity, "voucher_id": sid, "batch_no": batch, "serial_no": serial, "is_reversal": reversal})
        })
        .collect();
    Ok(json!({
        "columns": [col("posting_date", "Date", "date"), col("item_code", "Item", "data"), col("warehouse", "Warehouse", "data"), col("qty", "Qty", "decimal"),
            col("valuation_rate", "Rate", "currency"), col("value", "Value", "currency"), col("qty_after", "Balance Qty", "decimal"),
            col("value_after", "Balance Value", "currency"), col("voucher_type", "Voucher Type", "data"), col("batch_no", "Batch", "data"),
            col("serial_no", "Serial No", "data"), col("is_reversal", "Reversal", "check")],
        "rows": rows,
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

// ───────────────────────────── registers ─────────────────────────────

async fn register(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>, kind: &str) -> Result<Value> {
    let (from, to) = period(p, co)?;
    let party = p.uuid(kind)?;
    let (table, party_name, extra) =
        if kind == "customer" { ("e_sales_invoice", "customer_name", "po_no") } else { ("e_purchase_invoice", "supplier_name", "bill_no") };
    #[allow(clippy::type_complexity)]
    let data: Vec<(Uuid, Option<String>, NaiveDate, Option<String>, Option<String>, Option<Decimal>, Option<Decimal>, Option<Decimal>, Option<Decimal>, Option<Decimal>, Option<String>, Option<bool>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select i.id, i.number, i.posting_date, i.{party_name}, i.{extra}, i.net_total, i.total_taxes, i.grand_total, i.base_grand_total, i.outstanding_amount, \
             i.status, i.is_return from {table} i where i.tenant_id = $1 and i.company = $2 and i.docstatus = 1 and i.posting_date between $3 and $4 \
             and ($5::uuid is null or i.{kind} = $5) order by i.posting_date, i.number limit $6"
        )))
        .bind(ctx.tenant)
        .bind(co.id)
        .bind(from)
        .bind(to)
        .bind(party)
        .bind(MAX_ROWS + 1)
        .fetch_all(&mut **tx)
        .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let mut total_base = Decimal::ZERO;
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|(iid, number, day, name, reference, net, taxes, grand, base, outstanding, status, ret)| {
            total_base += base.unwrap_or_default();
            json!({"voucher_id": iid, "voucher_no": number, "posting_date": day, "party": name, "reference": reference,
                "net_total": money(net.unwrap_or_default()), "total_taxes": money(taxes.unwrap_or_default()), "grand_total": money(grand.unwrap_or_default()),
                "base_grand_total": money(base.unwrap_or_default()), "outstanding_amount": money(outstanding.unwrap_or_default()), "status": status,
                "is_return": ret.unwrap_or(false)})
        })
        .collect();
    let count = rows.len();
    Ok(json!({
        "columns": [col("voucher_no", "Invoice", "data"), col("posting_date", "Date", "date"), col("party", if kind == "customer" { "Customer" } else { "Supplier" }, "data"),
            col("reference", if kind == "customer" { "Customer PO" } else { "Supplier Invoice No" }, "data"), col("net_total", "Net Total", "currency"),
            col("total_taxes", "Taxes", "currency"), col("grand_total", "Grand Total", "currency"), col("base_grand_total", "Grand Total (Company Currency)", "currency"),
            col("outstanding_amount", "Outstanding", "currency"), col("status", "Status", "data")],
        "rows": rows,
        "totals": {"base_grand_total": money(total_base), "count": count},
        "meta": {"from_date": from, "to_date": to, "truncated": truncated},
    }))
}

// ───────────────────────────── deferred revenue and expense ─────────────────────────────

/// Every deferred line of the company's submitted invoices: what it defers over which days, what
/// was recognised and through when, what still waits on its deferred account, and what
/// recognising through `as_of` would move. Once recognition has run, what still waits is what the
/// deferred accounts hold.
async fn deferred_lines(tx: &mut Tx, ctx: &Ctx, co: &ReportCompany, p: &Params<'_>) -> Result<Value> {
    let as_of = p.date("as_of")?.unwrap_or_else(today);
    let expense = p.raw.get("deferred").map(String::as_str) == Some("Expense");
    let (invoice, party, flag) =
        if expense { ("PurchaseInvoice", "supplier_name", "enable_deferred_expense") } else { ("SalesInvoice", "customer_name", "enable_deferred_revenue") };
    let ent = ctx.schema.entity(invoice).ok_or_else(|| Error::NotFound(invoice.into()))?;
    let lines = ent.children.get("items").ok_or_else(|| Error::internal("an invoice without lines"))?;
    #[allow(clippy::type_complexity)]
    let data: Vec<(Uuid, Option<String>, Option<String>, Option<String>, NaiveDate, NaiveDate, Option<Decimal>, Option<Decimal>, Option<Decimal>, Option<NaiveDate>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select i.id, i.number, i.{party}, l.item_name, l.service_start_date, l.service_end_date, l.base_net_amount, l.net_amount, l.deferred_booked,              l.deferred_booked_till from {invoices} i join {lines} l on l.tenant_id = i.tenant_id and l.parent_id = i.id              where i.tenant_id = $1 and i.company = $2 and i.docstatus = 1 and coalesce(i.is_return, false) = false and coalesce(l.{flag}, false)              and l.service_start_date is not null and l.service_end_date is not null order by l.service_start_date, i.number limit $3",
            invoices = ferrum_kernel::ddl::q(&ent.table),
            lines = ferrum_kernel::ddl::q(&lines.table),
        )))
        .bind(ctx.tenant)
        .bind(co.id)
        .bind(MAX_ROWS + 1)
        .fetch_all(&mut **tx)
        .await?;
    let truncated = data.len() as i64 > MAX_ROWS;
    let (mut waiting, mut due) = (Decimal::ZERO, Decimal::ZERO);
    let rows: Vec<Value> = data
        .into_iter()
        .take(MAX_ROWS as usize)
        .map(|(iid, number, name, item, start, end, base, net, booked, till)| {
            let amount = base.or(net).unwrap_or_default();
            let booked = booked.unwrap_or_default();
            let next = (crate::deferred::earned(amount, start, end, as_of, co.precision) - booked).max(Decimal::ZERO);
            waiting += amount - booked;
            due += next;
            json!({"voucher_id": iid, "voucher_no": number, "party": name, "item": item, "service_start_date": start, "service_end_date": end,
                "amount": money(amount), "recognised": money(booked), "recognised_till": till, "deferred": money(amount - booked), "due": money(next)})
        })
        .collect();
    let count = rows.len();
    Ok(json!({
        "columns": [col("voucher_no", "Invoice", "data"), col("party", if expense { "Supplier" } else { "Customer" }, "data"), col("item", "Item", "data"),
            col("service_start_date", "Service From", "date"), col("service_end_date", "Service To", "date"), col("amount", "Amount", "currency"),
            col("recognised", "Recognised", "currency"), col("recognised_till", "Recognised Through", "date"), col("deferred", "Still Deferred", "currency"),
            col("due", "To Recognise Through The Day", "currency")],
        "rows": rows,
        "totals": {"deferred": money(waiting), "due": money(due), "count": count},
        "meta": {"as_of": as_of, "deferred": if expense { "Expense" } else { "Revenue" }, "truncated": truncated},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_lists_every_report_with_params() {
        let c = ErpReports.catalog();
        assert_eq!(c.len(), DEFS.len());
        assert!(c[0]["params"].as_array().unwrap().iter().any(|p| p["name"] == "company"));
        assert!(c.iter().all(|e| e["scoped"] == json!(false) && e["entity"].is_string()), "they sum up every record of their entity");
    }
}
