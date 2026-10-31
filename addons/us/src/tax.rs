//! US sales and use tax on ERP's documents, for a company in the United States.
//!
//! * Where a sale is taxed: where the goods go (the ship-to, else the customer's billing address;
//!   with neither, over the counter where the company is); within an origin-based state, where
//!   they ship from (the company's location). Outside the United States: an export.
//! * Only in the states where the company is registered to collect. A marketplace facilitator
//!   collects on what is sold through it; a customer's valid exemption certificate for the state
//!   exempts the sale.
//! * The rate: the ZIP code's combined rate (state, county, city and special districts) from the
//!   state's rate table on the invoice's date, else the state's own rate, which submitting refuses
//!   where local taxes apply. A product's taxability in the state (US Product Taxability) exempts
//!   it or reduces its rate; not-taxable tax codes (NT) carry none.
//! * Purchases: use tax, when the supplier charged no sales tax on taxable goods used here, at the
//!   company's location's rate, accrued by the company itself (a reverse charge to Use Tax Payable).
//!
//! States' own rates are the Tax Foundation's (State and Local Sales Tax Rates, Midyear 2026, as of
//! 1 July 2026); economic nexus thresholds are the Sales Tax Institute's chart (as of 1 August 2026).

use crate::util::{b, d, find, get, id, rows, s, set, sys};
use async_trait::async_trait;
use ferrum_app_erp::localization::{Localization, ship_to};
use ferrum_app_erp::pricing::Side;
use ferrum_kernel::query::Op;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, Filter, HookCtx, Kernel, Result, Tx};
use ferrum_meta::EffEntity;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::str::FromStr;
use uuid::Uuid;

/// Sales tax collected posts to the standard chart's output tax.
pub const SALES_TAX: &str = "2131";
pub const USE_TAX_PAYABLE: &str = "2171";
pub const USE_TAX_EXPENSE: &str = "5290";
/// Avalara's tax codes: tangible goods (taxed everywhere there is a sales tax) and not taxable.
pub const GOODS: &str = "P0000000";
pub const NOT_TAXABLE: &str = "NT";

/// When a state measures its economic nexus threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    /// The current or the previous calendar year.
    Calendar,
    /// The previous calendar year.
    Previous,
    /// The twelve months before (the state's quarter-end variants are read as the last twelve).
    Rolling,
}

/// Which sales a state's threshold counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Counted {
    /// Every sale: taxable, exempt and for resale.
    Gross,
    /// All but sales for resale.
    Retail,
    /// Taxable sales only.
    Taxable,
}

#[derive(Debug, PartialEq)]
pub struct State {
    pub code: &'static str,
    pub name: &'static str,
    /// The state's own rate, its statewide local add-ons included (California's, Utah's, Virginia's).
    pub rate: &'static str,
    /// Counties, cities or districts add their own rates.
    pub local: bool,
    /// Sales within the state are taxed where they ship from.
    pub origin: bool,
    /// Economic nexus: sales of at least this much (none where there is no sales tax)...
    pub nexus: u32,
    /// ... or, where `both`, and, at least this many transactions.
    pub transactions: Option<u32>,
    pub both: bool,
    /// The threshold must be exceeded, not only met ("more than").
    pub over: bool,
    pub period: Period,
    pub counted: Counted,
}

#[allow(clippy::too_many_arguments)]
const fn st(
    code: &'static str,
    name: &'static str,
    rate: &'static str,
    local: bool,
    origin: bool,
    nexus: u32,
    transactions: Option<u32>,
    both: bool,
    over: bool,
    period: Period,
    counted: Counted,
) -> State {
    State { code, name, rate, local, origin, nexus, transactions, both, over, period, counted }
}

use Counted::{Gross, Retail, Taxable};
use Period::{Calendar, Previous, Rolling};

/// The fifty states and the District of Columbia.
pub const STATES: &[State] = &[
    st("AL", "Alabama", "4", true, false, 250_000, None, false, false, Previous, Retail),
    // No state sales tax; the remote seller code of the cities and boroughs that adopted it.
    st("AK", "Alaska", "0", true, false, 100_000, None, false, false, Calendar, Gross),
    st("AZ", "Arizona", "5.6", true, true, 100_000, None, false, false, Calendar, Gross),
    st("AR", "Arkansas", "6.5", true, false, 100_000, Some(200), false, false, Calendar, Taxable),
    // Modified origin: the state's, counties' and cities' shares by origin, districts' by
    // destination; the destination's combined rate is used.
    st("CA", "California", "7.25", true, false, 500_000, None, false, false, Calendar, Gross),
    st("CO", "Colorado", "2.9", true, false, 100_000, None, false, false, Calendar, Retail),
    st("CT", "Connecticut", "6.35", false, false, 100_000, Some(200), true, false, Rolling, Retail),
    st("DE", "Delaware", "0", false, false, 0, None, false, false, Calendar, Gross),
    st("DC", "District of Columbia", "6", false, false, 100_000, Some(200), false, false, Calendar, Retail),
    st("FL", "Florida", "6", true, false, 100_000, None, false, false, Previous, Taxable),
    st("GA", "Georgia", "4", true, false, 100_000, Some(200), false, false, Calendar, Retail),
    st("HI", "Hawaii", "4", true, false, 100_000, Some(200), false, false, Calendar, Gross),
    st("ID", "Idaho", "6", true, false, 100_000, None, false, false, Calendar, Gross),
    st("IL", "Illinois", "6.25", true, true, 100_000, None, false, false, Rolling, Retail),
    st("IN", "Indiana", "7", false, false, 100_000, None, false, false, Calendar, Gross),
    st("IA", "Iowa", "6", true, false, 100_000, None, false, false, Calendar, Gross),
    st("KS", "Kansas", "6.5", true, false, 100_000, None, false, false, Calendar, Gross),
    st("KY", "Kentucky", "6", false, false, 100_000, None, false, false, Calendar, Gross),
    st("LA", "Louisiana", "5", true, false, 100_000, None, false, false, Calendar, Gross),
    st("ME", "Maine", "5.5", false, false, 100_000, None, false, false, Calendar, Gross),
    st("MD", "Maryland", "6", false, false, 100_000, Some(200), false, false, Calendar, Gross),
    st("MA", "Massachusetts", "6.25", false, false, 100_000, None, false, false, Calendar, Gross),
    st("MI", "Michigan", "6", false, false, 100_000, Some(200), false, false, Previous, Gross),
    st("MN", "Minnesota", "6.875", true, false, 100_000, Some(200), false, false, Rolling, Retail),
    st("MS", "Mississippi", "7", true, true, 250_000, None, false, true, Rolling, Gross),
    st("MO", "Missouri", "4.225", true, true, 100_000, None, false, false, Rolling, Taxable),
    st("MT", "Montana", "0", false, false, 0, None, false, false, Calendar, Gross),
    st("NE", "Nebraska", "5.5", true, false, 100_000, Some(200), false, false, Calendar, Retail),
    st("NV", "Nevada", "6.85", true, false, 100_000, Some(200), false, false, Calendar, Retail),
    st("NH", "New Hampshire", "0", false, false, 0, None, false, false, Calendar, Gross),
    st("NJ", "New Jersey", "6.625", false, false, 100_000, Some(200), false, false, Calendar, Gross),
    st("NM", "New Mexico", "4.875", true, false, 100_000, None, false, false, Previous, Taxable),
    // $500,000 and more than 100 sales, over the four sales tax quarters before.
    st("NY", "New York", "4", true, false, 500_000, Some(101), true, false, Rolling, Gross),
    st("NC", "North Carolina", "4.75", true, false, 100_000, None, false, false, Calendar, Gross),
    st("ND", "North Dakota", "5", true, false, 100_000, None, false, false, Calendar, Taxable),
    st("OH", "Ohio", "5.75", true, true, 100_000, Some(200), false, false, Calendar, Retail),
    st("OK", "Oklahoma", "4.5", true, false, 100_000, None, false, false, Calendar, Taxable),
    st("OR", "Oregon", "0", false, false, 0, None, false, false, Calendar, Gross),
    st("PA", "Pennsylvania", "6", true, true, 100_000, None, false, false, Previous, Gross),
    st("RI", "Rhode Island", "7", false, false, 100_000, Some(200), false, false, Previous, Gross),
    st("SC", "South Carolina", "6", true, false, 100_000, None, false, false, Calendar, Gross),
    st("SD", "South Dakota", "4.2", true, false, 100_000, None, false, false, Calendar, Gross),
    st("TN", "Tennessee", "7", true, true, 100_000, None, false, false, Rolling, Retail),
    st("TX", "Texas", "6.25", true, true, 500_000, None, false, false, Rolling, Gross),
    st("UT", "Utah", "6.1", true, true, 100_000, None, false, false, Calendar, Gross),
    st("VT", "Vermont", "6", true, false, 100_000, Some(200), false, false, Rolling, Gross),
    st("VA", "Virginia", "5.3", true, true, 100_000, Some(200), false, false, Calendar, Retail),
    st("WA", "Washington", "6.5", true, false, 100_000, None, false, true, Calendar, Gross),
    st("WV", "West Virginia", "6", true, false, 100_000, Some(200), false, false, Calendar, Gross),
    st("WI", "Wisconsin", "5", true, false, 100_000, None, false, false, Calendar, Gross),
    st("WY", "Wyoming", "4", true, false, 100_000, None, false, false, Calendar, Gross),
];

/// A state from its two-letter code or its name.
pub fn state(text: &str) -> Option<&'static State> {
    let t = text.trim().trim_end_matches('.').to_lowercase();
    if t.is_empty() {
        return None;
    }
    let t = match t.as_str() {
        "d.c" | "washington dc" | "washington, dc" | "washington d.c" | "district of columbia" => "dc",
        other => other,
    };
    STATES.iter().find(|s| s.code.eq_ignore_ascii_case(t) || s.name.eq_ignore_ascii_case(t))
}

/// The states a list names ("CA, NY TX; Nevada").
pub fn states(list: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for part in list.split([',', ';', '\n']) {
        let part = part.trim();
        let found: Vec<&'static str> = match state(part) {
            Some(s) => vec![s.code],
            None => part.split_whitespace().filter_map(state).map(|s| s.code).collect(),
        };
        for code in found {
            if !out.contains(&code) {
                out.push(code);
            }
        }
    }
    out
}

/// A ZIP code's first five digits.
pub fn zip5(postal: &str) -> Option<String> {
    let digits: String = postal.trim().chars().take_while(|c| c.is_ascii_digit()).collect();
    match digits.len() {
        5 => Some(digits),
        // Spreadsheets drop New England's leading zeros.
        3 | 4 if postal.trim().len() == digits.len() => Some(format!("{digits:0>5}")),
        _ => None,
    }
}

fn dec(v: &str) -> Decimal {
    Decimal::from_str(v).unwrap_or_default()
}

/// The rate a sale is taxed at somewhere.
#[derive(Debug, Clone, PartialEq)]
pub struct Rate {
    pub state: &'static str,
    pub zip: Option<String>,
    /// The tax region (from the rate table), else the state's name.
    pub region: String,
    pub combined: Decimal,
    /// From the rate tables; else the state's own rate.
    pub from_table: bool,
}

/// A ZIP code's rate on a day: the rate tables' latest from that day, else the state's own.
pub async fn rate(k: &Kernel, ctx: &Ctx, tx: &mut Tx, st: &'static State, zip: Option<&str>, on: &str) -> Result<Rate> {
    if let Some(z) = zip {
        let filters = vec![Filter::eq("zip", z), Filter::eq("state", st.code), Filter { field: "valid_from".into(), op: Op::Lte, value: json!(on) }];
        if let Some(r) = find(k, &sys(ctx), tx, "UsTaxRate", filters, Some("valid_from desc"), 1).await?.first() {
            return Ok(Rate {
                state: st.code,
                zip: Some(z.to_string()),
                region: s(r, "region").unwrap_or(z).to_string(),
                combined: d(r, "combined_rate"),
                from_table: true,
            });
        }
    }
    Ok(Rate { state: st.code, zip: zip.map(str::to_string), region: st.name.to_string(), combined: dec(st.rate), from_table: false })
}

/// Where a sale is taxed.
#[derive(Debug, Clone, PartialEq)]
pub enum Place {
    /// A state (`None`: an address in the United States without a state Ferrum knows).
    State(Option<&'static State>, Option<String>),
    Abroad,
}

/// What a line takes: a rate and a category, and why when it is none.
#[derive(Debug, Clone, PartialEq)]
pub struct Treatment {
    pub rate: Decimal,
    pub category: &'static str,
    pub reason: Option<String>,
}

impl Treatment {
    fn none(category: &'static str, reason: impl Into<String>) -> Self {
        Treatment { rate: Decimal::ZERO, category, reason: Some(reason.into()) }
    }
}

pub struct Plan {
    pub side: Side,
    pub on: String,
    pub place: Place,
    pub registered: Vec<&'static str>,
    pub marketplace: bool,
    /// The customer's valid exemption certificate for the state.
    pub certificate: Option<Doc>,
    /// The rate where the sale (or the purchase's use) is taxed.
    pub rate: Option<Rate>,
}

impl Plan {
    /// How a line of an item with this tax code is taxed (`taxability`: the code's row for the
    /// state, if any).
    pub fn treat(&self, tax_code: &str, taxability: Option<&Doc>) -> Treatment {
        let state = match &self.place {
            Place::Abroad => return Treatment::none("Export", "Export: no US sales tax"),
            Place::State(None, _) => return Treatment::none("Out of Scope", "No US state on the address"),
            Place::State(Some(st), _) => *st,
        };
        if self.side == Side::Selling {
            if self.marketplace {
                return Treatment::none("Out of Scope", "Collected by the marketplace facilitator");
            }
            if !self.registered.contains(&state.code) {
                return Treatment::none("Out of Scope", format!("Not registered for sales tax in {}", state.name));
            }
            if let Some(c) = &self.certificate {
                return Treatment::none(
                    "Exempt",
                    format!("Exemption certificate {} ({})", s(c, "certificate_number").unwrap_or_default(), s(c, "reason").unwrap_or("Other")),
                );
            }
        }
        if tax_code.eq_ignore_ascii_case(NOT_TAXABLE) {
            return Treatment::none("Exempt", format!("Not taxable ({NOT_TAXABLE})"));
        }
        if let Some(t) = taxability {
            if !b(t, "taxable") {
                return Treatment::none("Exempt", format!("Not taxable in {} ({tax_code})", state.name));
            }
            if t.get("rate").is_some_and(|r| !r.is_null()) {
                return Treatment { rate: d(t, "rate"), category: "Reduced", reason: None };
            }
        }
        match &self.rate {
            Some(r) if r.combined > Decimal::ZERO => Treatment { rate: r.combined, category: "Standard", reason: None },
            _ => Treatment::none("Out of Scope", format!("No sales tax in {}", state.name)),
        }
    }
}

/// An item's tax code (tangible goods when it has none).
pub fn tax_code(item: &Doc) -> String {
    s(item, "us_tax_code").map(str::trim).filter(|c| !c.is_empty()).unwrap_or(GOODS).to_ascii_uppercase()
}

fn address_place(a: Option<&Doc>) -> Option<Place> {
    let a = a?;
    if s(a, "country_code").is_some_and(|c| c != "US") {
        return Some(Place::Abroad);
    }
    Some(Place::State(s(a, "state").and_then(state), s(a, "postal_code").and_then(zip5)))
}

/// The company, if it is in the United States.
async fn us_company(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc) -> Result<Option<Doc>> {
    let Some(cid) = id(doc, "company") else { return Ok(None) };
    let company = get(k, &sys(ctx), tx, "Company", cid).await?;
    Ok((s(&company, "country_code") == Some("US")).then_some(company))
}

pub async fn plan(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc, side: Side) -> Result<Option<(Plan, Doc)>> {
    let Some(company) = us_company(k, ctx, tx, doc).await? else { return Ok(None) };
    let on = s(doc, "posting_date").or(s(doc, "transaction_date")).map(str::to_string).unwrap_or_else(|| ctx.today().format("%Y-%m-%d").to_string());
    let home_state = s(&company, "us_state").and_then(state);
    let home_zip = s(&company, "us_zip").and_then(zip5);
    let home = Place::State(home_state, home_zip.clone());
    let registered = states(s(&company, "us_registered_states").unwrap_or_default());
    let sctx = sys(ctx);
    let (place, marketplace, certificate) = match side {
        Side::Selling => {
            let place = address_place(ship_to(k, &sctx, tx, doc).await?.as_ref()).unwrap_or(home);
            let customer = match id(doc, "customer") {
                Some(c) => Some(get(k, &sctx, tx, "Customer", c).await?),
                None => None,
            };
            let marketplace = customer.as_ref().is_some_and(|c| b(c, "us_marketplace_facilitator"));
            let certificate = match (&place, &customer) {
                (Place::State(Some(st), _), Some(c)) => certificate(k, &sctx, tx, id(c, "id"), st.code, &on).await?,
                _ => None,
            };
            (place, marketplace, certificate)
        }
        // Use tax is owed where the goods are used: at the company.
        Side::Buying => (home, false, None),
    };
    // Within an origin-based state, a sale takes the rate where it ships from.
    let rate = match &place {
        Place::State(Some(st), zip) => {
            let zip = if side == Side::Selling && st.origin && home_state.is_some_and(|h| h.code == st.code) { home_zip.as_deref() } else { zip.as_deref() };
            Some(rate(k, &sctx, tx, st, zip, &on).await?)
        }
        _ => None,
    };
    Ok(Some((Plan { side, on, place, registered, marketplace, certificate, rate }, company)))
}

/// A customer's exemption certificate valid in a state on a day.
pub async fn certificate(k: &Kernel, ctx: &Ctx, tx: &mut Tx, customer: Option<Uuid>, state: &str, on: &str) -> Result<Option<Doc>> {
    let Some(customer) = customer else { return Ok(None) };
    let found = find(k, ctx, tx, "UsExemptionCertificate", vec![Filter::eq("customer", customer.to_string())], Some("valid_from desc"), 200).await?;
    Ok(found.into_iter().find(|c| {
        !b(c, "disabled")
            && states(s(c, "states").unwrap_or_default()).contains(&state)
            && s(c, "valid_from").is_some_and(|f| f <= on)
            && s(c, "valid_to").is_none_or(|t| t >= on)
    }))
}

pub async fn account(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, number: &str) -> Result<String> {
    let found = find(k, &sys(ctx), tx, "Account", vec![Filter::eq("company", company.to_string()), Filter::eq("account_number", number)], None, 1).await?;
    found
        .first()
        .and_then(|a| s(a, "id"))
        .map(str::to_string)
        .ok_or_else(|| Error::invalid(format!("the company has no account {number} for US sales and use tax: create its chart of accounts first")))
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

/// The rows of US Product Taxability for a state and the codes given.
async fn taxability(k: &Kernel, ctx: &Ctx, tx: &mut Tx, state: &str, codes: &[String]) -> Result<HashMap<String, Doc>> {
    let mut out = HashMap::new();
    for code in codes {
        if out.contains_key(code) || code == NOT_TAXABLE {
            continue;
        }
        if let Some(t) =
            find(k, &sys(ctx), tx, "UsTaxability", vec![Filter::eq("state", state), Filter::eq("tax_code", code.as_str())], None, 1).await?.into_iter().next()
        {
            out.insert(code.clone(), t);
        }
    }
    Ok(out)
}

/// Each line's treatment under a plan.
async fn treatments(k: &Kernel, ctx: &Ctx, tx: &mut Tx, plan: &Plan, items: &[Doc]) -> Result<Vec<Treatment>> {
    let codes: Vec<String> = items.iter().map(tax_code).collect();
    let rows = match &plan.place {
        Place::State(Some(st), _) => taxability(k, ctx, tx, st.code, &codes).await?,
        _ => HashMap::new(),
    };
    Ok(codes.iter().map(|c| plan.treat(c, rows.get(c))).collect())
}

/// The US's rules for ERP.
pub struct Us;

#[async_trait]
impl Localization for Us {
    fn package(&self) -> &'static str {
        crate::PACKAGE
    }

    async fn determine(&self, k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()> {
        if doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 {
            return Ok(());
        }
        let Some((plan, company)) = plan(k, ctx, tx, doc, side).await? else { return Ok(()) };
        if side == Side::Selling && ent.field("us_state").is_some() {
            let (code, region, rate, source) = match (&plan.place, &plan.rate) {
                (Place::State(Some(st), _), Some(r)) => (
                    Some(st.code),
                    Some(r.region.clone()),
                    Some(r.combined),
                    Some(if r.from_table {
                        format!("Rate table, ZIP {}", r.zip.clone().unwrap_or_default())
                    } else {
                        format!("{}'s state rate only", st.name)
                    }),
                ),
                _ => (None, None, None, None),
            };
            set(doc, "us_state", code.map_or(Value::Null, Value::from));
            set(doc, "us_jurisdiction", region.map_or(Value::Null, Value::from));
            set(doc, "us_rate", rate.map_or(Value::Null, |r| json!(r.to_string())));
            set(doc, "us_rate_source", source.map_or(Value::Null, Value::from));
            set(doc, "us_exemption_certificate", plan.certificate.as_ref().and_then(|c| s(c, "id")).map_or(Value::Null, Value::from));
        }
        if id(doc, "tax_template").is_some() || ent.field("taxes").is_none() || rows(doc, "items").is_empty() {
            return Ok(());
        }
        let cid = id(&company, "id").ok_or_else(|| Error::internal("company without id"))?;
        let mut taxes = rows(doc, "taxes");
        let combined = plan.rate.as_ref().map(|r| r.combined).unwrap_or_default();
        let row = match side {
            Side::Selling => {
                let acc = account(k, ctx, tx, cid, SALES_TAX).await?;
                json!({"charge_type": "On Net Total", "account": acc, "description": "Sales Tax", "rate": combined.to_string()})
            }
            Side::Buying => {
                let expense = account(k, ctx, tx, cid, USE_TAX_EXPENSE).await?;
                if !b(doc, "us_accrue_use_tax") {
                    // Unticked: the use tax row goes.
                    let before = taxes.len();
                    taxes.retain(|t| !(s(t, "account") == Some(expense.as_str()) && b(t, "reverse_charge")));
                    if taxes.len() != before {
                        set(doc, "taxes", Value::Array(taxes.into_iter().map(Value::Object).collect()));
                    }
                    return Ok(());
                }
                let owed = account(k, ctx, tx, cid, USE_TAX_PAYABLE).await?;
                json!({"charge_type": "On Net Total", "account": expense, "description": "Use Tax", "rate": combined.to_string(),
                       "reverse_charge": true, "reverse_charge_account": owed})
            }
        };
        let Value::Object(mut row) = row else { return Ok(()) };
        let acc = row.get("account").cloned();
        match taxes.iter_mut().find(|t| t.get("account") == acc.as_ref()) {
            Some(t) => {
                if (d(t, "rate"), s(t, "description")) == (combined, row.get("description").and_then(Value::as_str))
                    && b(t, "reverse_charge") == b(&row, "reverse_charge")
                {
                    return Ok(());
                }
                for (key, v) in row {
                    t.insert(key, v);
                }
            }
            None => {
                row.insert("id".into(), json!(Uuid::now_v7().to_string()));
                taxes.insert(0, row);
            }
        }
        set(doc, "taxes", Value::Array(taxes.into_iter().map(Value::Object).collect()));
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
        let number = if side == Side::Selling { SALES_TAX } else { USE_TAX_EXPENSE };
        let Ok(acc) = account(k, ctx, tx, cid, number).await else { return Ok(()) };
        if !taxes.iter().any(|t| s(t, "account") == Some(acc.as_str())) {
            return Ok(());
        }
        let treated = treatments(k, ctx, tx, &plan, items).await?;
        for (i, (line, t)) in lines.iter().zip(treated).enumerate() {
            if id(line, "item").is_none() {
                continue;
            }
            let Value::Object(entry) = json!({"tax_account": acc, "rate": t.rate.to_string(), "category": t.category, "exemption_reason": t.reason}) else {
                continue;
            };
            rates[i].retain(|r| s(r, "tax_account") != Some(acc.as_str()));
            rates[i].push(entry);
        }
        Ok(())
    }

    async fn check(&self, hc: &mut HookCtx<'_>, doc: &Doc, side: Side) -> Result<()> {
        if side != Side::Selling || hc.entity.def.name != "SalesInvoice" {
            return Ok(());
        }
        let Some((plan, _)) = plan(hc.kernel, hc.ctx, hc.tx, doc, side).await? else { return Ok(()) };
        let collects = |code: &str| plan.registered.contains(&code) && !plan.marketplace && plan.certificate.is_none();
        match (&plan.place, &plan.rate) {
            (Place::State(None, _), _) if !plan.registered.is_empty() && !plan.marketplace => {
                Err(Error::invalid("the ship-to (or billing) address needs its state, as a two-letter code or a name, for US sales tax"))
            }
            (Place::State(Some(st), zip), Some(r)) if collects(st.code) && st.local && !r.from_table => {
                let items = items_of(hc.kernel, &sys(hc.ctx), hc.tx, doc).await?;
                let taxed = treatments(hc.kernel, hc.ctx, hc.tx, &plan, &items).await?.iter().any(|t| t.category == "Standard");
                if !taxed {
                    return Ok(());
                }
                Err(Error::invalid(match zip.as_deref().or(r.zip.as_deref()) {
                    None => format!("local sales taxes apply in {}: the address needs its ZIP code", st.name),
                    Some(z) => format!(
                        "local sales taxes apply in {}, and the rate tables have no rate for ZIP code {z} on {}: submit the state's rate table (US Rate Table) or add the ZIP code's rate (US Sales Tax Rate)",
                        st.name, plan.on
                    ),
                }))
            }
            _ => Ok(()),
        }
    }

    async fn setup_company(&self, hc: &mut HookCtx<'_>, company: Uuid, accounts: &HashMap<&str, String>) -> Result<()> {
        let sctx = sys(hc.ctx);
        let co = hc.kernel.get_tx(&sctx, hc.tx, "Company", company, false).await?;
        if s(&co, "country_code") != Some("US") {
            return Ok(());
        }
        for (number, name, parent, root) in
            [(USE_TAX_PAYABLE, "Use Tax Payable", "Duties and Taxes", "Liability"), (USE_TAX_EXPENSE, "Use Tax Expense", "Indirect Expenses", "Expense")]
        {
            let Some(parent) = accounts.get(parent) else { continue };
            let kind = if root == "Liability" { "Tax" } else { "Expense" };
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

// ───────────────────────────── rate tables ─────────────────────────────

/// One line of a state's rate table, its rates as percentages.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRow {
    pub zip: String,
    pub region: String,
    pub state_rate: Decimal,
    pub county_rate: Decimal,
    pub city_rate: Decimal,
    pub special_rate: Decimal,
}

impl TableRow {
    pub fn combined(&self) -> Decimal {
        self.state_rate + self.county_rate + self.city_rate + self.special_rate
    }
}

/// One CSV record's fields (RFC 4180: quoted fields may hold commas and doubled quotes).
fn csv_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (c, quoted) {
            ('"', true) if chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|f| f.trim().to_string()).collect()
}

/// A state's rate table (Avalara's columns: State, ZipCode, TaxRegionName, EstimatedCombinedRate,
/// StateRate, EstimatedCountyRate, EstimatedCityRate, EstimatedSpecialRate), its rates as
/// fractions (0.0725) or, when any exceeds one, as percentages. Each problem names its line.
pub fn parse_table(csv: &str, state: &str) -> std::result::Result<Vec<TableRow>, Vec<String>> {
    let mut lines = csv.lines().enumerate().filter(|(_, l)| !l.trim().is_empty());
    let Some((_, header)) = lines.next() else { return Err(vec!["the table is empty".into()]) };
    let header: Vec<String> = csv_fields(header.trim_start_matches('\u{feff}')).into_iter().map(|h| h.to_ascii_lowercase()).collect();
    let col = |names: &[&str]| header.iter().position(|h| names.contains(&h.as_str()));
    let (Some(zc), Some(sc)) = (col(&["zipcode", "zip", "zip code"]), col(&["staterate", "state rate"])) else {
        return Err(vec!["the table needs the columns ZipCode and StateRate".into()]);
    };
    let (stc, rc, cc, cic, spc, comb) = (
        col(&["state"]),
        col(&["taxregionname", "region"]),
        col(&["estimatedcountyrate", "countyrate", "county rate"]),
        col(&["estimatedcityrate", "cityrate", "city rate"]),
        col(&["estimatedspecialrate", "specialrate", "special rate"]),
        col(&["estimatedcombinedrate", "combinedrate", "combined rate"]),
    );
    let mut problems = Vec::new();
    let mut raw: Vec<(usize, String, String, [Decimal; 4], Option<Decimal>)> = Vec::new();
    for (n, line) in lines {
        let f = csv_fields(line);
        let at = |i: Option<usize>| i.and_then(|i| f.get(i)).map(String::as_str).unwrap_or_default();
        let num = |i: Option<usize>, problems: &mut Vec<String>| -> Decimal {
            let v = at(i);
            if v.is_empty() {
                return Decimal::ZERO;
            }
            match Decimal::from_str(v.trim_end_matches('%')) {
                Ok(d) if d >= Decimal::ZERO => d,
                _ => {
                    problems.push(format!("line {}: '{v}' is not a rate", n + 1));
                    Decimal::ZERO
                }
            }
        };
        if stc.is_some() && !at(stc).eq_ignore_ascii_case(state) {
            problems.push(format!("line {}: the state is {}, not {state}", n + 1, at(stc)));
            continue;
        }
        let Some(zip) = zip5(at(Some(zc))) else {
            problems.push(format!("line {}: '{}' is not a ZIP code", n + 1, at(Some(zc))));
            continue;
        };
        let rates = [num(Some(sc), &mut problems), num(cc, &mut problems), num(cic, &mut problems), num(spc, &mut problems)];
        let combined = comb.map(|c| num(Some(c), &mut problems));
        raw.push((n + 1, zip, at(rc).to_string(), rates, combined));
    }
    // Fractions unless a rate reads as a percentage (above one).
    let percent = raw.iter().any(|(_, _, _, r, c)| r.iter().chain(c.iter()).any(|v| *v > Decimal::ONE));
    let scale = if percent { Decimal::ONE } else { Decimal::ONE_HUNDRED };
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut out = Vec::new();
    for (n, zip, region, r, combined) in raw {
        if let Some(first) = seen.insert(zip.clone(), n) {
            problems.push(format!("line {n}: ZIP code {zip} is already on line {first}"));
            continue;
        }
        let row = TableRow {
            zip,
            region,
            state_rate: (r[0] * scale).normalize(),
            county_rate: (r[1] * scale).normalize(),
            city_rate: (r[2] * scale).normalize(),
            special_rate: (r[3] * scale).normalize(),
        };
        if let Some(c) = combined
            && (c * scale - row.combined()).abs() > Decimal::new(1, 4)
        {
            problems.push(format!("line {n}: the rates add up to {}%, not the combined {}%", row.combined(), (c * scale).normalize()));
            continue;
        }
        if row.combined() > Decimal::from(25) {
            problems.push(format!("line {n}: a combined rate of {}% is not a US sales tax", row.combined()));
            continue;
        }
        out.push(row);
    }
    if problems.is_empty() { Ok(out) } else { Err(problems) }
}

/// Rate tables write their ZIP codes' rates when submitted and take them back when cancelled; a
/// rate's combined rate is the sum of its parts.
pub struct RateHook;

#[async_trait]
impl DocHook for RateHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        match hc.entity.def.name.as_str() {
            "UsTaxRate" => {
                let st = s(doc, "state").and_then(state).ok_or_else(|| Error::Validation(vec![FieldError::new("state", "is not a US state")]))?;
                set(doc, "state", st.code);
                let zip = s(doc, "zip").and_then(zip5).ok_or_else(|| Error::Validation(vec![FieldError::new("zip", "is not a five-digit ZIP code")]))?;
                set(doc, "zip", zip);
                let combined = ["state_rate", "county_rate", "city_rate", "special_rate"].iter().map(|f| d(doc, f)).sum::<Decimal>();
                set(doc, "combined_rate", json!(combined.normalize().to_string()));
            }
            "UsRateTable" | "UsTaxability" => {
                let st = s(doc, "state").and_then(state).ok_or_else(|| Error::Validation(vec![FieldError::new("state", "is not a US state")]))?;
                set(doc, "state", st.code);
                if hc.entity.def.name == "UsTaxability" {
                    let code = s(doc, "tax_code").unwrap_or_default().trim().to_ascii_uppercase();
                    set(doc, "tax_code", code);
                }
            }
            "UsExemptionCertificate" => {
                let list = states(s(doc, "states").unwrap_or_default());
                if list.is_empty() {
                    return Err(Error::Validation(vec![FieldError::new("states", "names no US state")]));
                }
                set(doc, "states", list.join(", "));
            }
            _ => {}
        }
        Ok(())
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.def.name != "UsRateTable" {
            return Ok(());
        }
        let rows = parse_table(s(doc, "csv").unwrap_or_default(), s(doc, "state").unwrap_or_default()).map_err(|p| {
            let more = if p.len() > 20 { format!(" (and {} more)", p.len() - 20) } else { String::new() };
            Error::Validation(vec![FieldError::new("csv", format!("{}{more}", p.into_iter().take(20).collect::<Vec<_>>().join("; ")))])
        })?;
        if rows.is_empty() {
            return Err(Error::Validation(vec![FieldError::new("csv", "has no ZIP codes")]));
        }
        set(doc, "zip_count", rows.len() as i64);
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.entity.def.name != "UsRateTable" {
            return Ok(());
        }
        let sctx = sys(hc.ctx);
        let table = id(doc, "id").ok_or_else(|| Error::internal("rate table without id"))?;
        let (st, from) = (s(doc, "state").unwrap_or_default().to_string(), s(doc, "valid_from").unwrap_or_default().to_string());
        let rows = parse_table(s(doc, "csv").unwrap_or_default(), &st).map_err(|p| Error::invalid(p.join("; ")))?;
        for r in rows {
            let Value::Object(values) = json!({"state": st, "zip": r.zip, "region": r.region, "state_rate": r.state_rate.to_string(), "county_rate": r.county_rate.to_string(),
                                               "city_rate": r.city_rate.to_string(), "special_rate": r.special_rate.to_string(), "valid_from": from, "rate_table": table.to_string()})
            else {
                continue;
            };
            let existing =
                find(hc.kernel, &sctx, hc.tx, "UsTaxRate", vec![Filter::eq("zip", r.zip.as_str()), Filter::eq("valid_from", from.as_str())], None, 1).await?;
            match existing.first().and_then(|e| id(e, "id")) {
                Some(rid) => {
                    hc.kernel.update_tx(&sctx, hc.tx, "UsTaxRate", rid, values, None).await?;
                }
                None => {
                    hc.kernel.insert_tx(&sctx, hc.tx, "UsTaxRate", values).await?;
                }
            }
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.entity.def.name != "UsRateTable" {
            return Ok(());
        }
        let sctx = sys(hc.ctx);
        let table = id(doc, "id").ok_or_else(|| Error::internal("rate table without id"))?;
        loop {
            let batch = find(hc.kernel, &sctx, hc.tx, "UsTaxRate", vec![Filter::eq("rate_table", table.to_string())], None, 500).await?;
            if batch.is_empty() {
                return Ok(());
            }
            for r in batch {
                if let Some(rid) = id(&r, "id") {
                    hc.kernel.delete_tx(&sctx, hc.tx, "UsTaxRate", rid, None).await?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: &str) -> Decimal {
        Decimal::from_str(v).unwrap()
    }

    #[test]
    fn states_by_code_and_name() {
        assert_eq!(STATES.len(), 51, "fifty states and the District of Columbia");
        assert_eq!(
            (state("ca").map(|s| s.code), state("New York").map(|s| s.code), state("Washington DC").map(|s| s.code)),
            (Some("CA"), Some("NY"), Some("DC"))
        );
        assert_eq!(state("Ontario").map(|s| s.code), None);
        assert_eq!(states("CA, ny; Texas\nNevada"), vec!["CA", "NY", "TX", "NV"]);
        assert_eq!(states("CA NY"), vec!["CA", "NY"]);
        assert_eq!((zip5("90001-1234"), zip5("2134"), zip5("ABC")), (Some("90001".into()), Some("02134".into()), None));
        let tx = state("TX").unwrap();
        assert!(tx.origin && tx.local && tx.nexus == 500_000);
        let ny = state("NY").unwrap();
        assert!(ny.both && ny.transactions == Some(101));
        assert!(STATES.iter().filter(|s| s.nexus == 0).all(|s| dec(s.rate).is_zero()), "no threshold only where there is no sales tax");
    }

    #[test]
    fn what_each_line_takes() {
        let ca = state("CA").unwrap();
        let plan = |place: Place, registered: Vec<&'static str>| Plan {
            side: Side::Selling,
            on: "2026-10-31".into(),
            place,
            registered,
            marketplace: false,
            certificate: None,
            rate: Some(Rate { state: "CA", zip: Some("90001".into()), region: "LA".into(), combined: n("9.75"), from_table: true }),
        };
        let la = || Place::State(Some(ca), Some("90001".into()));
        let p = plan(la(), vec!["CA"]);
        assert_eq!(p.treat(GOODS, None), Treatment { rate: n("9.75"), category: "Standard", reason: None });
        assert_eq!(p.treat("NT", None).category, "Exempt");
        let not_taxable = json!({"taxable": false}).as_object().cloned().unwrap();
        assert_eq!(p.treat("PF050001", Some(&not_taxable)).reason.as_deref(), Some("Not taxable in California (PF050001)"));
        let reduced = json!({"taxable": true, "rate": "1.5"}).as_object().cloned().unwrap();
        assert_eq!(p.treat("PF050001", Some(&reduced)).rate, n("1.5"));
        assert_eq!(plan(la(), vec!["NY"]).treat(GOODS, None).reason.as_deref(), Some("Not registered for sales tax in California"));
        assert_eq!(plan(Place::Abroad, vec!["CA"]).treat(GOODS, None).category, "Export");
        let resale = Plan { certificate: json!({"certificate_number": "R-1", "reason": "Resale"}).as_object().cloned(), ..plan(la(), vec!["CA"]) };
        assert_eq!(resale.treat(GOODS, None).reason.as_deref(), Some("Exemption certificate R-1 (Resale)"));
        let market = Plan { marketplace: true, ..plan(la(), vec!["CA"]) };
        assert_eq!(market.treat(GOODS, None).category, "Out of Scope");
    }

    #[test]
    fn rate_tables_read_as_published() {
        let csv = "State,ZipCode,TaxRegionName,EstimatedCombinedRate,StateRate,EstimatedCountyRate,EstimatedCityRate,EstimatedSpecialRate,RiskLevel\n\
                   CA,90001,\"LOS ANGELES, COUNTY\",0.097500,0.060000,0.002500,0.000000,0.035000,1\n\
                   CA,9001,TEST,0.0725,0.0725,0,0,0,1\n";
        let rows = parse_table(csv, "CA").unwrap();
        assert_eq!((rows[0].region.as_str(), rows[0].combined(), rows[1].zip.as_str()), ("LOS ANGELES, COUNTY", n("9.75"), "09001"));
        let percent = "ZipCode,StateRate,EstimatedCountyRate\n78701,6.25,2\n";
        assert_eq!(parse_table(percent, "TX").unwrap()[0].combined(), n("8.25"));
        let bad = "State,ZipCode,StateRate,EstimatedCombinedRate\nNV,89501,0.0685,0.0685\nCA,ABCDE,0.06,0.06\nCA,90001,0.06,0.07\nCA,90002,0.06,0.06\nCA,90002,0.06,0.06\n";
        let problems = parse_table(bad, "CA").unwrap_err();
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(problems[0].starts_with("line 2: the state is NV") && problems[3].starts_with("line 6: ZIP code 90002 is already on line 5"));
    }
}
