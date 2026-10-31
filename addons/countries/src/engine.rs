//! A country's VAT or GST as data, for the countries whose tax works the common way (Ferrum
//! docs/plan/localization.md, L6 and L7): each item has a category (standard, reduced, zero-rated,
//! exempt...) whose dated rate falls on one of the country's tax rows (one row in most countries;
//! Malaysia's sales tax and service tax, Japan's 10% and 8%, are two). On ERP's localization seam:
//!
//! * Sales at home take the item's rate; goods leaving the country and services to customers
//!   abroad are zero-rated exports.
//! * Purchases at home recover the tax on the row's input account (rows whose tax is not
//!   recoverable, like Malaysia's, carry none); imported goods carry none (the tax is paid at
//!   customs); services bought from abroad are reverse-charged by the company where the country
//!   says so (all its companies, or those whose setting says so).
//! * An item's own rate for a row's account (its item tax rates) wins over the country's.
//! * A sales invoice that charges the tax needs the company's registration number.
//! * A new company gets the country's accounts.
//!
//! Returns: [`Figures`] sums a period's submitted invoices by category from their tax breakdowns,
//! and each country's [`Return`] lays them out in its form's boxes.

use crate::util::{b, find, get, id, rows, s, set, sys};
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_app_erp::localization::{Localization, ship_to};
use ferrum_app_erp::pricing::Side;
use ferrum_domain::reports::{Params, ReportSource, col, param_def, period, read_tx, resolve_company};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, Filter, HookCtx, Kernel, Result, Tx};
use ferrum_meta::EffEntity;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;
use uuid::Uuid;

/// The standard chart's output and input tax, and the account tax the company reverse-charges
/// itself is owed on.
pub const OUTPUT_TAX: &str = "2131";
pub const INPUT_TAX: &str = "1151";
pub const REVERSE_CHARGE: &str = "2151";

/// Categories every country has, at no rate.
pub const ZERO_RATED: &str = "Zero Rated";
pub const EXEMPT: &str = "Exempt";
pub const OUT_OF_SCOPE: &str = "Out of Scope";
pub const EXPORT: &str = "Export";
pub const REVERSE_CHARGED: &str = "Reverse Charge";
/// The reason a line carries on a row its category is not on (Malaysia's goods on the service
/// tax row); returns leave these out.
pub const NOT_SUBJECT: &str = "Not subject to";

/// One of a country's tax rows.
#[derive(Debug)]
pub struct Row {
    /// Its description on documents ("GST", "Service Tax").
    pub label: &'static str,
    /// The account sales post it to.
    pub sales: &'static str,
    /// The account purchases recover it on; `None`: not recoverable, so purchases carry none.
    pub purchases: Option<&'static str>,
    /// The account debited when the company reverse-charges it itself, where not `purchases`
    /// (an expense, when it is not recoverable).
    pub self_assessed: Option<&'static str>,
}

/// A category's rate from a day on, on a row.
#[derive(Debug)]
pub struct Rate {
    pub category: &'static str,
    pub row: usize,
    pub from: &'static str,
    pub rate: &'static str,
}

/// Who reverse-charges services bought from abroad.
#[derive(Debug)]
pub enum ReverseCharge {
    Never,
    Always,
    /// The companies whose check field says so (Singapore's partly exempt businesses).
    When(&'static str),
}

#[derive(Debug)]
pub struct Country {
    /// The add-on's package, and the country (ISO 3166 code) whose companies it serves.
    pub package: &'static str,
    pub country: &'static str,
    pub rows: &'static [Row],
    pub rates: &'static [Rate],
    /// An item's category field, and the category of items that name none.
    pub category_field: &'static str,
    pub default_category: &'static str,
    /// An item's "Goods or Services" field (empty: stock items are goods).
    pub kind_field: &'static str,
    /// The registration number field on companies, customers and suppliers (a sales invoice that
    /// charges the tax needs the company's), its normal form or `None` when it is not one, and
    /// what a valid one looks like.
    pub number_field: &'static str,
    pub number: fn(&str) -> Option<String>,
    pub number_hint: &'static str,
    /// What exports and exempt supplies say on the invoice.
    pub export_reason: &'static str,
    pub exempt_reason: &'static str,
    pub reverse_charge: ReverseCharge,
    /// A simplified method some companies take (China's small-scale taxpayers): the company's
    /// check field, and the category their taxable sales take; their purchases recover nothing.
    pub simplified: Option<(&'static str, &'static str)>,
    /// Accounts a new company of the country gets: (number, name, parent group, root type).
    pub accounts: &'static [(&'static str, &'static str, &'static str, &'static str)],
}

/// How a line is taxed on one row.
#[derive(Debug, Clone, PartialEq)]
pub struct Treatment {
    pub rate: Decimal,
    pub category: String,
    pub reason: Option<String>,
}

impl Treatment {
    fn zero(category: &str, reason: Option<String>) -> Self {
        Treatment { rate: Decimal::ZERO, category: category.to_string(), reason }
    }
}

/// Where a document's supplies go, as the rules read it.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub side: Side,
    pub on: String,
    /// Goods leave the country (sales) or come from abroad (purchases).
    pub goods_abroad: bool,
    /// The customer (or supplier) of services is abroad.
    pub services_abroad: bool,
    /// The company reverse-charges services bought from abroad.
    pub reverse: bool,
    /// The company is on the country's simplified method.
    pub simplified: bool,
}

impl Country {
    /// A category's rate on a day: (row, rate).
    pub fn rate(&self, category: &str, on: &str) -> Option<(usize, Decimal)> {
        let mut found = None;
        for r in self.rates.iter().filter(|r| r.category.eq_ignore_ascii_case(category) && r.from <= on) {
            if found.is_none_or(|(_, from, _)| r.from >= from) {
                found = Some((r.row, r.from, Decimal::from_str(r.rate).unwrap_or_default()));
            }
        }
        found.map(|(row, _, rate)| (row, rate))
    }

    /// The categories an item's field offers: the rated ones, then the built-in.
    pub fn categories(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        for r in self.rates {
            // The simplified method's category is the company's, not an item's.
            if !out.contains(&r.category) && self.simplified.is_none_or(|(_, simple)| simple != r.category) {
                out.push(r.category);
            }
        }
        out.extend([ZERO_RATED, EXEMPT, OUT_OF_SCOPE]);
        out
    }

    pub fn goods(&self, item: &Doc) -> bool {
        match s(item, self.kind_field) {
            Some("Goods") => true,
            Some("Services") => false,
            _ => b(item, "is_stock_item"),
        }
    }

    /// How a line of an item is taxed on each of the country's rows.
    pub fn treat(&self, plan: &Plan, item: &Doc) -> Vec<Treatment> {
        let goods = self.goods(item);
        let abroad = if goods { plan.goods_abroad } else { plan.services_abroad };
        let category = s(item, self.category_field).unwrap_or(self.default_category);
        let not_subject = |label: &str| Treatment::zero(OUT_OF_SCOPE, Some(format!("{NOT_SUBJECT} {label}")));
        let on_row = |row: usize, t: Treatment| -> Vec<Treatment> {
            self.rows.iter().enumerate().map(|(i, r)| if i == row { t.clone() } else { not_subject(r.label) }).collect()
        };
        let row_of = |category: &str| self.rate(category, &plan.on).map(|(row, _)| row).unwrap_or(0);
        match (plan.side, abroad) {
            (Side::Selling, true) => return on_row(row_of(category), Treatment::zero(EXPORT, Some(self.export_reason.into()))),
            (Side::Buying, true) if goods => {
                return on_row(row_of(category), Treatment::zero(OUT_OF_SCOPE, Some("Imported goods: the tax is paid at customs".into())));
            }
            (Side::Buying, true) if !plan.reverse => return on_row(row_of(category), Treatment::zero(OUT_OF_SCOPE, Some("Services from abroad".into()))),
            _ => {}
        }
        if plan.simplified && !matches!(category, ZERO_RATED | EXEMPT | OUT_OF_SCOPE) {
            if let (Side::Selling, Some((_, simple))) = (plan.side, self.simplified)
                && let Some((row, rate)) = self.rate(simple, &plan.on)
            {
                return on_row(row, Treatment { rate, category: simple.to_string(), reason: None });
            }
            if plan.side == Side::Buying {
                return on_row(0, Treatment::zero(OUT_OF_SCOPE, Some("Simplified method: no input tax credit".into())));
            }
        }
        match category {
            ZERO_RATED => on_row(0, Treatment::zero(ZERO_RATED, None)),
            EXEMPT => on_row(0, Treatment::zero(EXEMPT, Some(self.exempt_reason.into()))),
            OUT_OF_SCOPE => on_row(0, Treatment::zero(OUT_OF_SCOPE, None)),
            c => match self.rate(c, &plan.on) {
                Some((row, rate)) => {
                    let reverse = plan.side == Side::Buying && abroad;
                    on_row(row, Treatment { rate, category: if reverse { REVERSE_CHARGED.into() } else { c.to_string() }, reason: None })
                }
                None => on_row(0, Treatment::zero(OUT_OF_SCOPE, Some(format!("No rate for {c} on {}", plan.on)))),
            },
        }
    }

    /// The account a row's tax posts to on a side (`None`: purchases carry none of it).
    pub fn account(&self, row: usize, side: Side, reverse: bool) -> Option<&'static str> {
        let r = self.rows.get(row)?;
        match side {
            Side::Selling => Some(r.sales),
            Side::Buying if reverse => r.self_assessed.or(r.purchases),
            Side::Buying => r.purchases,
        }
    }
}

async fn country_of(k: &Kernel, ctx: &Ctx, tx: &mut Tx, a: Option<Uuid>) -> Result<Option<String>> {
    let Some(a) = a else { return Ok(None) };
    Ok(find(k, ctx, tx, "Address", vec![Filter::eq("id", a.to_string())], None, 1).await?.first().and_then(|a| s(a, "country_code")).map(str::to_string))
}

/// The plan for a document of a company of the country, with the company.
pub async fn plan(c: &Country, k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc, side: Side) -> Result<Option<(Plan, Doc)>> {
    let Some(cid) = id(doc, "company") else { return Ok(None) };
    let sctx = sys(ctx);
    let company = get(k, &sctx, tx, "Company", cid).await?;
    if s(&company, "country_code") != Some(c.country) {
        return Ok(None);
    }
    let on = s(doc, "posting_date").or(s(doc, "transaction_date")).map(str::to_string).unwrap_or_else(|| ctx.today().format("%Y-%m-%d").to_string());
    let away = |country: Option<String>| country.is_some_and(|x| x != c.country);
    let (goods_abroad, services_abroad) = match side {
        Side::Selling => {
            let goods = ship_to(k, &sctx, tx, doc).await?.and_then(|a| s(&a, "country_code").map(str::to_string));
            let billing = country_of(k, &sctx, tx, id(doc, "customer_address")).await?.or(goods.clone());
            (away(goods), away(billing))
        }
        Side::Buying => {
            let from = country_of(k, &sctx, tx, id(doc, "supplier_address")).await?;
            (away(from.clone()), away(from))
        }
    };
    let reverse = match c.reverse_charge {
        ReverseCharge::Never => false,
        ReverseCharge::Always => true,
        ReverseCharge::When(field) => b(&company, field),
    };
    let simplified = c.simplified.is_some_and(|(field, _)| b(&company, field));
    Ok(Some((Plan { side, on, goods_abroad, services_abroad, reverse, simplified }, company)))
}

pub async fn account(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, number: &str) -> Result<String> {
    let found = find(k, &sys(ctx), tx, "Account", vec![Filter::eq("company", company.to_string()), Filter::eq("account_number", number)], None, 1).await?;
    found
        .first()
        .and_then(|a| s(a, "id"))
        .map(str::to_string)
        .ok_or_else(|| Error::invalid(format!("the company has no account {number} for its taxes: create its chart of accounts first")))
}

async fn items_of(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc) -> Result<Vec<Doc>> {
    let mut cache: HashMap<Uuid, Doc> = HashMap::new();
    let mut out = Vec::new();
    for r in rows(doc, "items") {
        match id(&r, "item") {
            Some(i) => {
                if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(i) {
                    e.insert(get(k, ctx, tx, "Item", i).await?);
                }
                out.push(cache[&i].clone());
            }
            None => out.push(Doc::new()),
        }
    }
    Ok(out)
}

/// A country's rules on ERP's localization seam.
pub struct National(pub &'static Country);

/// The rows a document of a side needs, each with its account and whether it is reverse-charged:
/// (row, account number, reverse charge).
fn needed(c: &Country, side: Side, treated: &[Vec<Treatment>]) -> Vec<(usize, &'static str, bool)> {
    let mut out = Vec::new();
    for (row, _) in c.rows.iter().enumerate() {
        let used: Vec<&Treatment> =
            treated.iter().filter_map(|t| t.get(row)).filter(|t| !t.reason.as_deref().is_some_and(|r| r.starts_with(NOT_SUBJECT))).collect();
        // Every document carries the first row (its breakdown records exports and exemptions);
        // the others only when a line falls on them.
        if row > 0 && used.is_empty() {
            continue;
        }
        let reverse = side == Side::Buying && used.iter().any(|t| t.category == REVERSE_CHARGED);
        if let Some(acc) = c.account(row, side, reverse) {
            out.push((row, acc, reverse));
        }
    }
    out
}

#[async_trait]
impl Localization for National {
    fn package(&self) -> &'static str {
        self.0.package
    }

    async fn determine(&self, k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()> {
        let c = self.0;
        if doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 || id(doc, "tax_template").is_some() || ent.field("taxes").is_none() {
            return Ok(());
        }
        let Some((plan, company)) = plan(c, k, ctx, tx, doc, side).await? else { return Ok(()) };
        let items = items_of(k, &sys(ctx), tx, doc).await?;
        if items.is_empty() {
            return Ok(());
        }
        let cid = id(&company, "id").ok_or_else(|| Error::internal("company without id"))?;
        let treated: Vec<Vec<Treatment>> = items.iter().map(|i| c.treat(&plan, i)).collect();
        let mut ours = Vec::new();
        for (row, number, reverse) in needed(c, side, &treated) {
            let acc = account(k, ctx, tx, cid, number).await?;
            let rate = treated.iter().filter_map(|t| t.get(row)).map(|t| t.rate).max().unwrap_or_default();
            let owed = if reverse { Some(account(k, ctx, tx, cid, REVERSE_CHARGE).await?) } else { None };
            ours.push((acc, c.rows[row].label, rate, reverse, owed));
        }
        // The country's accounts, so rows it no longer needs go.
        let mut all = Vec::new();
        for r in c.rows {
            for number in [Some(r.sales), r.purchases, r.self_assessed].into_iter().flatten() {
                if let Ok(a) = account(k, ctx, tx, cid, number).await
                    && !all.contains(&a)
                {
                    all.push(a);
                }
            }
        }
        let taxes = rows(doc, "taxes");
        let current: Vec<(String, bool)> =
            taxes.iter().filter_map(|t| s(t, "account").filter(|a| all.iter().any(|x| x == a)).map(|a| (a.to_string(), b(t, "reverse_charge")))).collect();
        let wanted: Vec<(String, bool)> = ours.iter().map(|(a, _, _, r, _)| (a.clone(), *r)).collect();
        if current == wanted {
            return Ok(());
        }
        let mut out: Vec<Value> = ours
            .into_iter()
            .map(|(acc, label, rate, reverse, owed)| {
                json!({"id": Uuid::now_v7().to_string(), "charge_type": "On Net Total", "account": acc, "description": label, "rate": rate.to_string(),
                       "reverse_charge": reverse, "reverse_charge_account": owed})
            })
            .collect();
        out.extend(taxes.into_iter().filter(|t| s(t, "account").is_none_or(|a| !all.iter().any(|x| x == a))).map(Value::Object));
        set(doc, "taxes", Value::Array(out));
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
        let c = self.0;
        let Some((plan, company)) = plan(c, k, ctx, tx, doc, side).await? else { return Ok(()) };
        let Some(cid) = id(&company, "id") else { return Ok(()) };
        let mut accounts: Vec<Option<String>> = Vec::new();
        for row in 0..c.rows.len() {
            let mut found = None;
            for reverse in [false, true] {
                if let Some(number) = c.account(row, side, reverse)
                    && let Ok(a) = account(k, ctx, tx, cid, number).await
                    && taxes.iter().any(|t| s(t, "account") == Some(a.as_str()))
                {
                    found = Some(a);
                }
            }
            accounts.push(found);
        }
        for (i, (line, item)) in lines.iter().zip(items).enumerate() {
            if id(line, "item").is_none() {
                continue;
            }
            for (row, t) in c.treat(&plan, item).into_iter().enumerate() {
                let Some(Some(acc)) = accounts.get(row) else { continue };
                // The item's own rate for the account wins: the country's category goes with it.
                if let Some(own) = rates[i].iter_mut().find(|r| s(r, "tax_account") == Some(acc.as_str())) {
                    if s(own, "category").is_none() {
                        own.insert("category".into(), json!(t.category));
                    }
                    continue;
                }
                let Value::Object(entry) = json!({"tax_account": acc, "rate": t.rate.to_string(), "category": t.category, "exemption_reason": t.reason}) else {
                    continue;
                };
                rates[i].push(entry);
            }
        }
        Ok(())
    }

    async fn check(&self, hc: &mut HookCtx<'_>, doc: &Doc, side: Side) -> Result<()> {
        if side != Side::Selling || hc.entity.def.name != "SalesInvoice" {
            return Ok(());
        }
        let c = self.0;
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let company = get(hc.kernel, &sys(hc.ctx), hc.tx, "Company", cid).await?;
        if s(&company, "country_code") == Some(c.country)
            && s(&company, c.number_field).is_none()
            && rows(doc, "taxes").iter().any(|t| crate::util::d(t, "tax_amount") != Decimal::ZERO)
        {
            return Err(Error::invalid(format!(
                "an invoice that charges {} carries the company's registration number: set it on the company",
                c.rows[0].label
            )));
        }
        Ok(())
    }

    async fn setup_company(&self, hc: &mut HookCtx<'_>, company: Uuid, accounts: &HashMap<&str, String>) -> Result<()> {
        let c = self.0;
        let sctx = sys(hc.ctx);
        let co = hc.kernel.get_tx(&sctx, hc.tx, "Company", company, false).await?;
        if s(&co, "country_code") != Some(c.country) {
            return Ok(());
        }
        let reverse = [(REVERSE_CHARGE, "Tax Owed (reverse charge)", "Duties and Taxes", "Liability")];
        let extra: &[_] = if matches!(c.reverse_charge, ReverseCharge::Never) { &[] } else { &reverse };
        for (number, name, parent, root) in c.accounts.iter().chain(extra) {
            let Some(parent) = accounts.get(parent) else { continue };
            if !find(hc.kernel, &sctx, hc.tx, "Account", vec![Filter::eq("company", company.to_string()), Filter::eq("account_number", *number)], None, 1)
                .await?
                .is_empty()
            {
                continue;
            }
            let kind = if *root == "Expense" { "Expense" } else { "Tax" };
            let Value::Object(a) = json!({"account_name": name, "account_number": number, "company": company.to_string(), "is_group": false, "root_type": root,
                                          "account_type": kind, "parent_account": parent, "account_currency": co.get("default_currency")})
            else {
                continue;
            };
            hc.kernel.insert_tx(&sctx, hc.tx, "Account", a).await?;
        }
        Ok(())
    }
}

/// Puts registration numbers in their normal form on companies, customers and suppliers, and
/// refuses ones that are not.
pub struct NumberHook(pub &'static Country);

#[async_trait]
impl DocHook for NumberHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let c = self.0;
        if hc.entity.field(c.number_field).is_none() {
            return Ok(());
        }
        let Some(raw) = s(doc, c.number_field).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string) else { return Ok(()) };
        // A party abroad keeps its own country's number as written.
        if hc.entity.def.name != "Company" && s(doc, "country_code").is_some_and(|cc| cc != c.country) {
            return Ok(());
        }
        let normal = (c.number)(&raw).ok_or_else(|| Error::Validation(vec![FieldError::new(c.number_field, format!("is not valid: {}", c.number_hint))]))?;
        set(doc, c.number_field, normal);
        Ok(())
    }
}

/// The digits of a number written with spaces, dots, dashes or slashes.
pub fn digits(raw: &str) -> String {
    raw.chars().filter(|c| !matches!(c, ' ' | '.' | '-' | '/')).collect()
}

// ───────────────────────────── returns ─────────────────────────────

/// A period's submitted invoices, summed from their tax breakdowns on the country's accounts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Figures {
    /// Sales by category: (net, tax).
    pub sales: BTreeMap<String, (Decimal, Decimal)>,
    /// Purchases by category: (net, tax); the company's reverse charges under "Reverse Charge".
    pub purchases: BTreeMap<String, (Decimal, Decimal)>,
    /// The invoices' totals with tax.
    pub sales_gross: Decimal,
    pub purchases_gross: Decimal,
}

impl Figures {
    pub fn sales_net(&self, categories: &[&str]) -> Decimal {
        categories.iter().filter_map(|c| self.sales.get(*c)).map(|v| v.0).sum()
    }

    pub fn sales_tax(&self, categories: &[&str]) -> Decimal {
        categories.iter().filter_map(|c| self.sales.get(*c)).map(|v| v.1).sum()
    }

    pub fn purchases_net(&self, categories: &[&str]) -> Decimal {
        categories.iter().filter_map(|c| self.purchases.get(*c)).map(|v| v.0).sum()
    }

    pub fn purchases_tax(&self, categories: &[&str]) -> Decimal {
        categories.iter().filter_map(|c| self.purchases.get(*c)).map(|v| v.1).sum()
    }

    /// All sales (or purchases) net of tax, and all their tax.
    pub fn all_sales(&self) -> (Decimal, Decimal) {
        self.sales.values().fold((Decimal::ZERO, Decimal::ZERO), |a, v| (a.0 + v.0, a.1 + v.1))
    }

    pub fn all_purchases(&self) -> (Decimal, Decimal) {
        self.purchases.values().fold((Decimal::ZERO, Decimal::ZERO), |a, v| (a.0 + v.0, a.1 + v.1))
    }
}

async fn ids(tx: &mut Tx, ctx: &Ctx, company: Uuid, numbers: &[&str]) -> Result<Vec<Uuid>> {
    let numbers: Vec<String> = numbers.iter().map(|n| n.to_string()).collect();
    Ok(sqlx::query_scalar("select id from e_account where tenant_id = $1 and company = $2 and account_number = any($3)")
        .bind(ctx.tenant)
        .bind(company)
        .bind(&numbers)
        .fetch_all(&mut **tx)
        .await?)
}

async fn side_figures(
    tx: &mut Tx,
    ctx: &Ctx,
    company: Uuid,
    entity: &str,
    accounts: &[Uuid],
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(BTreeMap<String, (Decimal, Decimal)>, Decimal)> {
    let parent = ferrum_meta::table_for(entity);
    let breakdown = ferrum_kernel::ddl::q(&ferrum_meta::child_table(&parent, "tax_breakdown"));
    let parent = ferrum_kernel::ddl::q(&parent);
    let rows: Vec<(Option<String>, bool, Option<Decimal>, Option<Decimal>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select b.category, coalesce(b.reverse_charge, false), sum(b.base_taxable_amount), sum(b.base_tax_amount) \
         from {parent} i join {breakdown} b on b.tenant_id = i.tenant_id and b.parent_id = i.id \
         where i.tenant_id = $1 and i.company = $2 and i.docstatus = 1 and i.posting_date between $3 and $4 and b.account = any($5) \
         and coalesce(b.exemption_reason, '') not like '{NOT_SUBJECT}%' group by 1, 2"
    )))
    .bind(ctx.tenant)
    .bind(company)
    .bind(from)
    .bind(to)
    .bind(accounts)
    .fetch_all(&mut **tx)
    .await?;
    let mut out: BTreeMap<String, (Decimal, Decimal)> = BTreeMap::new();
    for (category, reverse, net, tax) in rows {
        let key = if reverse { REVERSE_CHARGED.to_string() } else { category.unwrap_or_default() };
        let e = out.entry(key).or_default();
        e.0 += net.unwrap_or_default();
        e.1 += tax.unwrap_or_default();
    }
    let gross: Option<Decimal> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select sum(base_grand_total) from {parent} where tenant_id = $1 and company = $2 and docstatus = 1 and posting_date between $3 and $4"
    )))
    .bind(ctx.tenant)
    .bind(company)
    .bind(from)
    .bind(to)
    .fetch_one(&mut **tx)
    .await?;
    Ok((out, gross.unwrap_or_default()))
}

/// A period's figures for a company of the country.
pub async fn figures(c: &Country, tx: &mut Tx, ctx: &Ctx, company: Uuid, from: NaiveDate, to: NaiveDate) -> Result<Figures> {
    let sales_accounts: Vec<&str> = c.rows.iter().map(|r| r.sales).collect();
    let purchase_accounts: Vec<&str> = c.rows.iter().flat_map(|r| [r.purchases, r.self_assessed]).flatten().collect();
    let sales_ids = ids(tx, ctx, company, &sales_accounts).await?;
    let purchase_ids = ids(tx, ctx, company, &purchase_accounts).await?;
    let (sales, sales_gross) = side_figures(tx, ctx, company, "SalesInvoice", &sales_ids, from, to).await?;
    let (purchases, purchases_gross) = side_figures(tx, ctx, company, "PurchaseInvoice", &purchase_ids, from, to).await?;
    Ok(Figures { sales, purchases, sales_gross, purchases_gross })
}

/// A box of a return: (box, label, amount).
pub type Line = (&'static str, &'static str, Decimal);

/// A country's return, worked out from a period's figures.
pub struct Return {
    pub name: &'static str,
    pub label: &'static str,
    pub country: &'static Country,
    /// The boxes from the figures and the company.
    pub boxes: fn(&Figures, &Doc) -> Vec<Line>,
}

#[async_trait]
impl ReportSource for Return {
    fn catalog(&self) -> Vec<Value> {
        let params: Vec<Value> = ["company", "from_date", "to_date"].iter().map(|p| param_def(p)).collect();
        vec![json!({"name": self.name, "label": self.label, "module": "accounting", "entity": "SalesInvoice", "scoped": false, "params": params})]
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
        if name != self.name {
            return Err(Error::NotFound(format!("report {name}")));
        }
        let p = Params { raw };
        let mut tx = read_tx(k, ctx).await?;
        let co = resolve_company(&mut tx, ctx, &p).await?;
        let (from, to) = period(&p, &co)?;
        let f = figures(self.country, &mut tx, ctx, co.id, from, to).await?;
        let company = k.get_tx(ctx, &mut tx, "Company", co.id, false).await?;
        tx.rollback().await?;
        let rows: Vec<Value> = (self.boxes)(&f, &company).into_iter().map(|(b, l, v)| json!({"box": b, "label": l, "amount": crate::util::money(v)})).collect();
        Ok(json!({"name": name, "label": self.label, "rows": rows, "meta": {"from_date": from, "to_date": to},
                  "columns": [col("box", "Box", "data"), col("label", "", "data"), col("amount", "Amount", "currency")]}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST: Country = Country {
        package: "xx",
        country: "XX",
        rows: &[
            Row { label: "Sales Tax", sales: "2131", purchases: None, self_assessed: None },
            Row { label: "Service Tax", sales: "2133", purchases: None, self_assessed: Some("5291") },
        ],
        rates: &[
            Rate { category: "Goods 10%", row: 0, from: "2018-09-01", rate: "10" },
            Rate { category: "Services", row: 1, from: "2018-09-01", rate: "6" },
            Rate { category: "Services", row: 1, from: "2024-03-01", rate: "8" },
        ],
        category_field: "xx_category",
        default_category: "Goods 10%",
        kind_field: "xx_kind",
        number_field: "xx_number",
        number: |v| Some(v.to_string()),
        number_hint: "anything",
        export_reason: "Export",
        exempt_reason: "Exempt",
        reverse_charge: ReverseCharge::Always,
        simplified: Some(("xx_small", "Goods 10%")),
        accounts: &[],
    };

    fn n(v: &str) -> Decimal {
        Decimal::from_str(v).unwrap()
    }

    fn item(v: Value) -> Doc {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn categories_rows_and_dates() {
        assert_eq!(TEST.rate("services", "2024-02-29"), Some((1, n("6"))));
        assert_eq!(TEST.rate("Services", "2026-10-31"), Some((1, n("8"))));
        assert_eq!(TEST.rate("Services", "2010-01-01"), None);
        assert_eq!(TEST.categories(), vec!["Services", ZERO_RATED, EXEMPT, OUT_OF_SCOPE], "the simplified method's category is not an item's");
        let home = Plan { side: Side::Selling, on: "2026-10-31".into(), goods_abroad: false, services_abroad: false, reverse: true, simplified: false };
        let goods = TEST.treat(&home, &item(json!({"xx_kind": "Goods"})));
        assert_eq!((goods[0].rate, goods[1].reason.as_deref()), (n("10"), Some("Not subject to Service Tax")));
        let service = TEST.treat(&home, &item(json!({"xx_kind": "Services", "xx_category": "Services"})));
        assert_eq!((service[0].reason.as_deref(), service[1].rate), (Some("Not subject to Sales Tax"), n("8")));
        let export = TEST.treat(&Plan { goods_abroad: true, ..home.clone() }, &item(json!({"is_stock_item": true})));
        assert_eq!(export[0].category, EXPORT);
        let simple = TEST.treat(&Plan { simplified: true, ..home.clone() }, &item(json!({"xx_kind": "Services", "xx_category": "Services"})));
        assert_eq!((simple[0].category.as_str(), simple[0].rate), ("Goods 10%", n("10")), "the simplified method's category for every taxable sale");
        let exempt = TEST.treat(&home, &item(json!({"xx_category": "Exempt"})));
        assert_eq!((exempt[0].category.as_str(), exempt[0].reason.as_deref()), (EXEMPT, Some("Exempt")));
    }

    #[test]
    fn purchases_from_abroad() {
        let import = Plan { side: Side::Buying, on: "2026-10-31".into(), goods_abroad: true, services_abroad: true, reverse: true, simplified: false };
        let goods = TEST.treat(&import, &item(json!({"xx_kind": "Goods"})));
        assert_eq!((goods[0].category.as_str(), goods[0].rate), (OUT_OF_SCOPE, Decimal::ZERO));
        let services = TEST.treat(&import, &item(json!({"xx_kind": "Services", "xx_category": "Services"})));
        assert_eq!((services[1].category.as_str(), services[1].rate), (REVERSE_CHARGED, n("8")));
        let rows = needed(&TEST, Side::Buying, &[services]);
        assert_eq!(rows, vec![(1, "5291", true)], "not recoverable: reverse-charged to an expense, and no input row");
        let not_reverse = TEST.treat(&Plan { reverse: false, ..import }, &item(json!({"xx_kind": "Services", "xx_category": "Services"})));
        assert_eq!(not_reverse[1].reason.as_deref(), Some("Services from abroad"));
        let figures =
            Figures { sales: BTreeMap::from([("Goods 10%".to_string(), (n("100"), n("10"))), (EXPORT.to_string(), (n("50"), n("0")))]), ..Default::default() };
        assert_eq!(
            (figures.sales_net(&["Goods 10%", EXPORT]), figures.sales_tax(&["Goods 10%"]), figures.all_sales()),
            (n("150"), n("10"), (n("150"), n("10")))
        );
    }
}
