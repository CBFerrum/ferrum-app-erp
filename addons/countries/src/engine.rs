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
//! Returns: a country's return is a definition its package seeds, worked out by ERP's tax returns.
//! [`Figures`] and [`Return`] remain for the returns a definition cannot say yet: Australia's (the
//! invoices' totals), the United Arab Emirates' (boxes by the company's emirate) and Mexico's (tax
//! owed when paid).

use crate::util::{b, find, get, id, rows, s, set, sys};
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_app_erp::localization::{Localization, TaxId, field_tax_ids, ship_to};
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
    pub number: Number,
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

/// How a registration number is checked: a function (the coded countries) or a description (a data file's).
#[derive(Debug)]
pub enum Number {
    Code(fn(&str) -> Option<String>),
    Rule(&'static NumberRule),
}

impl Number {
    /// The number's normal form, or `None` when it is not one.
    pub fn normal(&self, raw: &str) -> Option<String> {
        match self {
            Number::Code(f) => f(raw),
            Number::Rule(r) => r.normal(raw),
        }
    }
}

/// A registration number described as data: characters stripped, suffixes dropped, an optional prefix, the shapes the
/// rest may take (`9` a digit, `A` a letter, `X` either, any other character itself) and weighted-sum checks.
#[derive(Debug, Default)]
pub struct NumberRule {
    pub strip: String,
    pub drop: Vec<String>,
    pub prefix: Option<String>,
    pub shapes: Vec<String>,
    pub checks: Vec<Check>,
}

/// A check character: the weighted sum of the other characters (left to right or right to left), each product's
/// digits summed when `fold`, modulo `modulus`, as the remainder or its complement, mapped (`"10": "K"`) or as a digit.
#[derive(Debug)]
pub struct Check {
    pub weights: Vec<u32>,
    pub from_right: bool,
    pub modulus: u32,
    pub complement: bool,
    pub fold: bool,
    pub map: Vec<(String, String)>,
    /// The check character's position; negative counts from the end.
    pub at: i32,
}

impl NumberRule {
    pub fn normal(&self, raw: &str) -> Option<String> {
        let mut t: String = raw.chars().filter(|c| !self.strip.contains(*c)).collect::<String>().to_uppercase();
        for d in &self.drop {
            if let Some(cut) = t.strip_suffix(d.as_str()) {
                t = cut.to_string();
            }
        }
        let body = match &self.prefix {
            Some(p) => t.strip_prefix(p.as_str()).unwrap_or(&t).to_string(),
            None => t.clone(),
        };
        let chars: Vec<char> = body.chars().collect();
        let fits = |mask: &str| {
            mask.chars().count() == chars.len()
                && mask.chars().zip(&chars).all(|(m, c)| match m {
                    '9' => c.is_ascii_digit(),
                    'A' => c.is_ascii_alphabetic(),
                    'X' => c.is_ascii_alphanumeric(),
                    other => other == *c,
                })
        };
        if !self.shapes.iter().any(|m| fits(m)) {
            return None;
        }
        for c in &self.checks {
            let k = if c.at >= 0 { c.at as usize } else { chars.len().checked_sub(c.at.unsigned_abs() as usize)? };
            let mut others: Vec<char> = chars.iter().enumerate().filter(|(i, _)| *i != k).map(|(_, ch)| *ch).collect();
            if c.from_right {
                others.reverse();
            }
            if others.len() > c.weights.len() {
                return None;
            }
            let mut sum = 0u32;
            for (ch, w) in others.iter().zip(&c.weights) {
                let p = ch.to_digit(10)? * w;
                sum += if c.fold { p.to_string().chars().filter_map(|d| d.to_digit(10)).sum() } else { p };
            }
            let r = sum % c.modulus;
            let v = if c.complement { c.modulus - r } else { r };
            let expected = match c.map.iter().find(|(from, _)| *from == v.to_string()) {
                Some((_, to)) => to.clone(),
                None if v < 10 => v.to_string(),
                None => return None,
            };
            if expected != chars.get(k)?.to_string() {
                return None;
            }
        }
        Some(match &self.prefix {
            Some(p) => format!("{p}{body}"),
            None => body,
        })
    }
}

/// A string kept for the life of the process: a data country's text is read once (`LazyLock`), so the strings leaked
/// are bounded by the files compiled in.
fn keep(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}

impl Country {
    /// A country described as a data file (`data/<code>.json`, docs/countries.md), checked as it is read: an unknown key,
    /// a rate on a row that does not exist, a date or rate that is not one, or a default category without a rate is
    /// refused naming the key.
    pub fn from_json(package: &'static str, text: &str) -> std::result::Result<&'static Country, String> {
        use serde_json::Value;
        let v: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
        let o = v.as_object().ok_or("not an object")?;
        const KEYS: &[&str] = &[
            "country",
            "rows",
            "rates",
            "category_field",
            "default_category",
            "kind_field",
            "number_field",
            "number",
            "number_hint",
            "export_reason",
            "exempt_reason",
            "reverse_charge",
            "simplified",
            "accounts",
            "sources",
        ];
        if let Some(k) = o.keys().find(|k| !KEYS.contains(&k.as_str())) {
            return Err(format!("unknown key {k}"));
        }
        let text_of = |o: &serde_json::Map<String, Value>, k: &str| -> std::result::Result<&'static str, String> {
            o.get(k).and_then(Value::as_str).map(keep).ok_or_else(|| format!("{k} must be text"))
        };
        let opt_text = |o: &serde_json::Map<String, Value>, k: &str| -> Option<&'static str> { o.get(k).and_then(Value::as_str).map(keep) };
        let list =
            |k: &str| -> std::result::Result<Vec<Value>, String> { o.get(k).and_then(Value::as_array).cloned().ok_or_else(|| format!("{k} must be a list")) };
        let mut rows = Vec::new();
        for (i, r) in list("rows")?.iter().enumerate() {
            let r = r.as_object().ok_or_else(|| format!("rows[{i}] must be an object"))?;
            rows.push(Row {
                label: text_of(r, "label")?,
                sales: text_of(r, "sales")?,
                purchases: opt_text(r, "purchases"),
                self_assessed: opt_text(r, "self_assessed"),
            });
        }
        let mut rates = Vec::new();
        for (i, r) in list("rates")?.iter().enumerate() {
            let r = r.as_object().ok_or_else(|| format!("rates[{i}] must be an object"))?;
            let row = r.get("row").and_then(Value::as_u64).ok_or_else(|| format!("rates[{i}].row must be a number"))? as usize;
            if row >= rows.len() {
                return Err(format!("rates[{i}].row: there is no row {row}"));
            }
            let from = text_of(r, "from")?;
            chrono::NaiveDate::parse_from_str(from, "%Y-%m-%d").map_err(|_| format!("rates[{i}].from is not a date: {from}"))?;
            let rate = text_of(r, "rate")?;
            rate.parse::<Decimal>().map_err(|_| format!("rates[{i}].rate is not a decimal: {rate}"))?;
            rates.push(Rate { category: text_of(r, "category")?, row, from, rate });
        }
        let n = o.get("number").and_then(Value::as_object).ok_or("number must be an object")?;
        let mut checks = Vec::new();
        for (i, c) in n.get("checks").and_then(Value::as_array).cloned().unwrap_or_default().iter().enumerate() {
            let c = c.as_object().ok_or_else(|| format!("number.checks[{i}] must be an object"))?;
            let num = |k: &str| c.get(k).and_then(Value::as_i64).ok_or_else(|| format!("number.checks[{i}].{k} must be a number"));
            checks.push(Check {
                weights: c
                    .get("weights")
                    .and_then(Value::as_array)
                    .ok_or_else(|| format!("number.checks[{i}].weights must be a list"))?
                    .iter()
                    .filter_map(Value::as_u64)
                    .map(|w| w as u32)
                    .collect(),
                from_right: c.get("from").and_then(Value::as_str) == Some("right"),
                modulus: num("modulus")? as u32,
                complement: c.get("result").and_then(Value::as_str) == Some("complement"),
                fold: c.get("fold").and_then(Value::as_bool).unwrap_or(false),
                map: c
                    .get("map")
                    .and_then(Value::as_object)
                    .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string())).collect())
                    .unwrap_or_default(),
                at: num("at")? as i32,
            });
        }
        let strings = |k: &str| -> Vec<String> {
            n.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
        };
        let rule = NumberRule {
            strip: n.get("strip").and_then(Value::as_str).unwrap_or_default().to_string(),
            drop: strings("drop"),
            prefix: n.get("prefix").and_then(Value::as_str).map(str::to_string),
            shapes: strings("shapes"),
            checks,
        };
        if rule.shapes.is_empty() {
            return Err("number.shapes must name at least one shape".into());
        }
        let reverse_charge = match o.get("reverse_charge") {
            Some(Value::String(w)) if w == "never" => ReverseCharge::Never,
            Some(Value::String(w)) if w == "always" => ReverseCharge::Always,
            Some(Value::Object(m)) => ReverseCharge::When(m.get("when").and_then(Value::as_str).map(keep).ok_or("reverse_charge.when must be text")?),
            _ => return Err("reverse_charge must be \"never\", \"always\" or {\"when\": field}".into()),
        };
        let simplified = match o.get("simplified") {
            None | Some(Value::Null) => None,
            Some(Value::Object(m)) => Some((text_of(m, "field")?, text_of(m, "category")?)),
            _ => return Err("simplified must be null or {field, category}".into()),
        };
        let mut accounts = Vec::new();
        for (i, a) in list("accounts")?.iter().enumerate() {
            let a = a.as_object().ok_or_else(|| format!("accounts[{i}] must be an object"))?;
            accounts.push((text_of(a, "number")?, text_of(a, "name")?, text_of(a, "parent")?, text_of(a, "root")?));
        }
        let country = Country {
            package,
            country: text_of(o, "country")?,
            rows: Box::leak(rows.into_boxed_slice()),
            rates: Box::leak(rates.into_boxed_slice()),
            category_field: text_of(o, "category_field")?,
            default_category: text_of(o, "default_category")?,
            kind_field: text_of(o, "kind_field")?,
            number_field: text_of(o, "number_field")?,
            number: Number::Rule(Box::leak(Box::new(rule))),
            number_hint: text_of(o, "number_hint")?,
            export_reason: text_of(o, "export_reason")?,
            exempt_reason: text_of(o, "exempt_reason")?,
            reverse_charge,
            simplified,
            accounts: Box::leak(accounts.into_boxed_slice()),
        };
        if country.default_category != OUT_OF_SCOPE && country.rates.iter().all(|r| r.category != country.default_category) {
            return Err(format!("default_category {} has no rate", country.default_category));
        }
        Ok(Box::leak(Box::new(country)))
    }
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
    /// Rates kept as data (Tax Rate records) on the day, by category in lower case: they win
    /// over the built-in ones.
    pub rates: HashMap<String, Decimal>,
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

    /// A category's rate for a plan: a Tax Rate record's when there is one, else the built-in.
    pub fn rate_for(&self, plan: &Plan, category: &str) -> Option<(usize, Decimal)> {
        let built = self.rate(category, &plan.on);
        match plan.rates.get(&category.to_lowercase()) {
            Some(rate) => Some((built.map_or(0, |(row, _)| row), *rate)),
            None => built,
        }
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
                && let Some((row, rate)) = self.rate_for(plan, simple)
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
            c => match self.rate_for(plan, c) {
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
    let rates = ferrum_app_erp::localization::rates(k, ctx, tx, c.country, None, &on).await?;
    Ok(Some((Plan { side, on, goods_abroad, services_abroad, reverse, simplified, rates }, company)))
}

/// The company's account standing for the standard account `number` (on a template's chart, the one mapped to it).
pub async fn account(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, number: &str) -> Result<String> {
    ferrum_app_erp::setup::standard_account(k, ctx, tx, company, number)
        .await?
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

    /// The country's registration number of a company, customer or supplier; with one, a party is
    /// a registered business.
    fn tax_ids(&self, ctx: &Ctx, entity: &str, record: &Doc) -> Vec<TaxId> {
        field_tax_ids(ctx, entity, record, &[self.0.number_field])
    }

    fn registered(&self, _ctx: &Ctx, _entity: &str, record: &Doc) -> Option<bool> {
        s(record, self.0.number_field).map(|_| true)
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
            // Only under a parent the chart has (the standard chart's name of it).
            let Some(parent) = accounts.get(parent).and(ferrum_app_erp::setup::coa_number(parent)) else { continue };
            let kind = if *root == "Expense" { "Expense" } else { "Tax" };
            ferrum_app_erp::setup::ensure_account(hc.kernel, &sctx, hc.tx, company, number, name, parent, root, kind).await?;
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
        let normal =
            c.number.normal(&raw).ok_or_else(|| Error::Validation(vec![FieldError::new(c.number_field, format!("is not valid: {}", c.number_hint))]))?;
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
        number: Number::Code(|v: &str| Some(v.to_string())),
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
        let home = Plan {
            side: Side::Selling,
            on: "2026-10-31".into(),
            goods_abroad: false,
            services_abroad: false,
            reverse: true,
            simplified: false,
            rates: Default::default(),
        };
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
        let import = Plan {
            side: Side::Buying,
            on: "2026-10-31".into(),
            goods_abroad: true,
            services_abroad: true,
            reverse: true,
            simplified: false,
            rates: Default::default(),
        };
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

#[cfg(test)]
mod described {
    use super::*;

    fn check(weights: &[u32], from_right: bool, modulus: u32, fold: bool, map: &[(&str, &str)]) -> Check {
        Check {
            weights: weights.to_vec(),
            from_right,
            modulus,
            complement: true,
            fold,
            map: map.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(),
            at: -1,
        }
    }

    #[test]
    fn a_described_number_is_checked_by_shape_and_weighted_sums() {
        // Weights 5,4,3,2,7,6,5,4,3,2 from the left, modulus 11, complement, 11 -> 0. 20-12345678-?: 2·5 + 0·4 + 1·3 + 2·2 +
        // 3·7 + 4·6 + 5·5 + 6·4 + 7·3 + 8·2 = 148; 148 mod 11 = 5; 11 - 5 = 6.
        let eleven = NumberRule {
            strip: " .-".into(),
            shapes: vec!["99999999999".into()],
            checks: vec![check(&[5, 4, 3, 2, 7, 6, 5, 4, 3, 2], false, 11, false, &[("11", "0")])],
            ..Default::default()
        };
        assert_eq!(eleven.normal("20-12345678-6").as_deref(), Some("20123456786"));
        // 20-00000001-?: 2·5 + 1·2 = 12; 12 mod 11 = 1; 11 - 1 = 10, not a digit and not mapped: no such number.
        assert_eq!(eleven.normal("20-00000001-0"), None);
        // 30-00000000-?: 3·5 = 15; 15 mod 11 = 4; 11 - 4 = 7.
        assert_eq!(eleven.normal("30-00000000-7").as_deref(), Some("30000000007"));
        assert_eq!(eleven.normal("20-12345678-5"), None, "a wrong check digit");
        assert_eq!(eleven.normal("20-1234567-6"), None, "a wrong shape");

        // Weights 2..7 from the right, modulus 11, 11 -> 0, 10 -> K; seven or eight digits and a check.
        let k = NumberRule {
            strip: ".-".into(),
            shapes: vec!["99999999X".into(), "9999999X".into()],
            checks: vec![check(&[2, 3, 4, 5, 6, 7, 2, 3], true, 11, false, &[("11", "0"), ("10", "K")])],
            ..Default::default()
        };
        // 12.345.678-?: from the right 8·2 + 7·3 + 6·4 + 5·5 + 4·6 + 3·7 + 2·2 + 1·3 = 138; 138 mod 11 = 6; 11 - 6 = 5.
        assert_eq!(k.normal("12.345.678-5").as_deref(), Some("123456785"));
        // 1.000.006-?: 6·2 + 1·2 (the eighth weight is 3 only for an eighth digit: here 1 is the seventh, weight 2) = 14; 14 mod 11 = 3; 8.
        assert_eq!(k.normal("1.000.006-8").as_deref(), Some("10000068"));
        // 10.000.004-?: 4·2 + 1·3 = 11; 11 mod 11 = 0; 11 - 0 = 11 -> 0.
        assert_eq!(k.normal("10.000.004-0").as_deref(), Some("100000040"));
        // 10.000.005-?: 5·2 + 1·3 = 13; 13 mod 11 = 2; 9. And a sum giving 10 -> K: 10.000.003: 3·2 + 1·3 = 9; 11 - 9 = 2.
        assert_eq!(k.normal("10.000.005-9").as_deref(), Some("100000059"));
        // 1.000.000-?: 1·2 = 2; 11 - 2 = 9. 7.000.000-?: 7·2 = 14; 3; 8. 6.000.000-?: 12; 1; 10 -> K.
        assert_eq!(k.normal("6.000.000-k").as_deref(), Some("6000000K"));
        assert_eq!(k.normal("6.000.000-1"), None);

        // Weights 1,2,1,2,… from the left, each product's digits summed, modulus 10, complement, 10 -> 0 (a Luhn sum).
        let luhn = NumberRule {
            strip: " ".into(),
            shapes: vec!["999999999".into()],
            checks: vec![check(&[1, 2, 1, 2, 1, 2, 1, 2], false, 10, true, &[("10", "0")])],
            ..Default::default()
        };
        // 1234 5678 ?: 1 + 4 + 3 + (8→8) + 5 + (12→3) + 7 + (16→7) = 38; 38 mod 10 = 8; 10 - 8 = 2.
        assert_eq!(luhn.normal("1234 5678 2").as_deref(), Some("123456782"));
        assert_eq!(luhn.normal("1234 5678 3"), None);

        // A prefix kept in the normal form, a suffix dropped, a shape with letters.
        let vat = NumberRule { strip: " ".into(), drop: vec!["MVA".into()], prefix: Some("NO".into()), shapes: vec!["AA99".into()], checks: vec![] };
        assert_eq!(vat.normal("no ab12 mva").as_deref(), Some("NOAB12"));
        assert_eq!(vat.normal("AB12").as_deref(), Some("NOAB12"));
        assert_eq!(vat.normal("A112"), None);
    }

    #[test]
    fn a_description_with_a_mistake_is_refused_by_name() {
        let good = include_str!("../data/kr.json");
        assert!(Country::from_json("kr", good).is_ok());
        let edit = |f: &dyn Fn(&mut serde_json::Value)| {
            let mut v: serde_json::Value = serde_json::from_str(good).unwrap();
            f(&mut v);
            Country::from_json("kr", &v.to_string()).unwrap_err()
        };
        assert!(edit(&|v| v["vat_rate"] = serde_json::json!("10")).contains("unknown key vat_rate"));
        assert!(edit(&|v| v["rates"][0]["row"] = serde_json::json!(3)).contains("rates[0].row"));
        assert!(edit(&|v| v["rates"][0]["from"] = serde_json::json!("July 1977")).contains("rates[0].from"));
        assert!(edit(&|v| v["default_category"] = serde_json::json!("Reduced")).contains("default_category Reduced has no rate"));
    }
}
