//! EU VAT on ERP's documents (Directive 2006/112/EC): which country's VAT a sale or purchase takes
//! and how, for a company in a member state.
//!
//! * At home: the home country's rates.
//! * To a business in another member state (it has a VAT number, confirmed in VIES before the
//!   invoice is submitted): goods are an exempt intra-Community supply (Art. 138, category K),
//!   services are reverse-charged to the customer (Art. 44 and 196, AE); no VAT on the invoice.
//! * To a consumer in another member state: goods and digital services (telecommunications,
//!   broadcasting, electronic services) take the destination's VAT through the One-Stop Shop when
//!   the company says so (above the €10,000 threshold, Art. 59c), else the home country's; other
//!   services take the home country's (Art. 45).
//! * Outside the EU: goods are exempt exports (Art. 146), services to businesses are supplied
//!   outside the EU (Art. 44) and digital services to consumers too (Art. 58); other services to
//!   consumers take the home country's VAT.
//! * Purchases: from another member state's business, and services from outside the EU, the
//!   company reverse-charges the VAT itself (input and owed alike); imported goods carry no VAT on
//!   the supplier's invoice (it is paid at customs).
//!
//! Rates are data (`EuVatRate`, from the Commission's TEDB, each from the day it took effect): a
//! line takes its country's standard rate, or for a reduced, zero-rated or exempt item the rate
//! set on the item for that country, so a reduced rate is never guessed.

use crate::util::{b, d, find, get, id, rows, s, set, sys};
use async_trait::async_trait;
use ferrum_app_erp::localization::{Localization, ship_to};
use ferrum_app_erp::pricing::Side;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, Filter, HookCtx, Kernel, Result, Tx, outbox};
use ferrum_meta::EffEntity;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

/// The member states by their ISO codes (Greece is GR; VIES and TEDB write EL).
pub const MEMBER_STATES: &[&str] = &[
    "AT", "BE", "BG", "CY", "CZ", "DE", "DK", "EE", "ES", "FI", "FR", "GR", "HR", "HU", "IE", "IT", "LT", "LU", "LV", "MT", "NL", "PL", "PT", "RO", "SE", "SI",
    "SK",
];

pub fn member(iso: &str) -> bool {
    MEMBER_STATES.contains(&iso)
}

/// VIES's and TEDB's code for a member state's ISO code (GR → EL).
pub fn vies_code(iso: &str) -> &str {
    if iso == "GR" { "EL" } else { iso }
}

/// The ISO code for VIES's code (EL → GR).
pub fn iso_code(vies: &str) -> &str {
    if vies == "EL" { "GR" } else { vies }
}

/// The accounts EU VAT posts to, by their numbers in a company's chart: the standard chart's input
/// and output tax, and the two the add-on adds.
pub const INPUT_TAX: &str = "1151";
pub const OUTPUT_TAX: &str = "2131";
pub const OSS_VAT: &str = "2150";
pub const REVERSE_CHARGE_VAT: &str = "2151";
const ADDED_ACCOUNTS: &[(&str, &str)] = &[(OSS_VAT, "OSS VAT Payable"), (REVERSE_CHARGE_VAT, "VAT Owed (reverse charge)")];

// ───────────────────────────── VAT numbers ─────────────────────────────

/// A VAT number as VIES takes it: upper case, without spaces, dots or dashes, with its country
/// prefix (GR written as EL).
pub fn normalize_vat(raw: &str) -> String {
    let n: String = raw.chars().filter(|c| !matches!(c, ' ' | '.' | '-' | '\u{a0}')).flat_map(char::to_uppercase).collect();
    match n.strip_prefix("GR") {
        Some(rest) => format!("EL{rest}"),
        None => n,
    }
}

fn digits(s: &str, n: std::ops::RangeInclusive<usize>) -> bool {
    n.contains(&s.len()) && s.bytes().all(|c| c.is_ascii_digit())
}

/// Whether a normalized VAT number has its member state's form (the Commission's VIES FAQ); VIES
/// says whether it is registered.
pub fn valid_vat_format(vat: &str) -> std::result::Result<(), String> {
    if vat.len() < 4 || !vat.is_ascii() {
        return Err("a VAT number starts with its country's two letters".into());
    }
    let (cc, n) = vat.split_at(2);
    let b = n.as_bytes();
    let ok = match cc {
        "AT" => n.starts_with('U') && digits(&n[1..], 8..=8),
        "BE" => digits(n, 10..=10) && matches!(b[0], b'0' | b'1'),
        "BG" => digits(n, 9..=10),
        "CY" => n.len() == 9 && digits(&n[..8], 8..=8) && b[8].is_ascii_uppercase(),
        "CZ" => digits(n, 8..=10),
        "DE" | "EE" | "EL" | "PT" => digits(n, 9..=9),
        "DK" | "FI" | "HU" | "LU" | "MT" | "SI" => digits(n, 8..=8),
        "ES" => n.len() == 9 && b[0].is_ascii_alphanumeric() && digits(&n[1..8], 7..=7) && b[8].is_ascii_alphanumeric(),
        "FR" => n.len() == 11 && b[..2].iter().all(|c| c.is_ascii_alphanumeric() && !matches!(c, b'O' | b'I')) && digits(&n[2..], 9..=9),
        "HR" | "IT" | "LV" => digits(n, 11..=11),
        "IE" => {
            (n.len() == 8 || n.len() == 9)
                && b[0].is_ascii_digit()
                && (b[1].is_ascii_alphanumeric() || matches!(b[1], b'+' | b'*'))
                && b[2..7].iter().all(u8::is_ascii_digit)
                && b[7].is_ascii_uppercase()
                && (n.len() == 8 || b[8].is_ascii_uppercase())
        }
        "LT" => digits(n, 9..=9) || digits(n, 12..=12),
        "NL" => n.len() == 12 && digits(&n[..9], 9..=9) && b[9] == b'B' && digits(&n[10..], 2..=2),
        "PL" | "SK" => digits(n, 10..=10),
        "RO" => digits(n, 2..=10),
        "SE" => digits(n, 12..=12),
        _ => return Err(format!("{cc} is not an EU member state's prefix (Greece is EL)")),
    };
    if ok { Ok(()) } else { Err(format!("this is not the form of a VAT number in {cc}")) }
}

/// The member state (ISO) a VAT number is from.
pub fn vat_country(vat: &str) -> Option<String> {
    let cc = vat.get(..2)?;
    let iso = iso_code(cc);
    member(iso).then(|| iso.to_string())
}

/// Normalizes and checks `eu_vat_number` on companies, customers and suppliers; on customers a new
/// number forgets the last VIES answer and asks VIES again.
pub struct VatNumberHook;

#[async_trait]
impl DocHook for VatNumberHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.field("eu_vat_number").is_none() {
            return Ok(());
        }
        let vat = s(doc, "eu_vat_number").map(normalize_vat).filter(|v| !v.is_empty());
        set(doc, "eu_vat_number", vat.clone().map_or(Value::Null, Value::from));
        if let Some(v) = &vat {
            valid_vat_format(v).map_err(|m| Error::Validation(vec![FieldError::new("eu_vat_number", m)]))?;
        }
        Ok(())
    }

    async fn after_save(&self, hc: &mut HookCtx<'_>, doc: &Doc, old: Option<&Doc>) -> Result<()> {
        if hc.entity.def.name != "Customer" || hc.entity.field("eu_vies_valid").is_none() {
            return Ok(());
        }
        let now = s(doc, "eu_vat_number");
        if now == old.and_then(|o| s(o, "eu_vat_number")) {
            return Ok(());
        }
        let cid = id(doc, "id").ok_or_else(|| Error::internal("customer without id"))?;
        let clear = json!({"eu_vies_valid": false, "eu_vies_name": null, "eu_vies_address": null, "eu_vies_checked_at": null, "eu_vies_consultation": null, "eu_vies_error": null});
        if let Value::Object(v) = clear {
            hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "Customer", cid, v).await?;
        }
        if now.is_some() {
            crate::vies::request(hc, cid, None).await?;
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        match (hc.entity.def.name.as_str(), name) {
            ("Customer", "eu_check_vat") => {
                let cid = id(doc, "id").ok_or_else(|| Error::internal("customer without id"))?;
                if s(doc, "eu_vat_number").is_none() {
                    return Err(Error::field("eu_vat_number", "is required to check it in VIES"));
                }
                let company = params.get("company").and_then(Value::as_str).and_then(|c| c.parse().ok());
                crate::vies::request(hc, cid, company).await?;
                Ok(Some(json!({"status": "Checking", "message": "VIES is asked; the answer appears on the customer shortly"})))
            }
            ("Company", "eu_setup_vat") => {
                let cid = id(doc, "id").ok_or_else(|| Error::internal("company without id"))?;
                let made = add_accounts(hc, cid).await?;
                Ok(Some(json!({"created": made})))
            }
            ("Company", "eu_update_rates") => {
                let on = hc.ctx.today().format("%Y-%m-%d").to_string();
                outbox::enqueue(hc.tx, hc.ctx.tenant, crate::tedb::TOPIC, Some("Company"), id(doc, "id"), json!({"situation_on": on})).await?;
                Ok(Some(json!({"status": "Fetching", "message": "TEDB is asked; the rates appear under EU VAT Rates shortly"})))
            }
            ("Item", "eu_suggest_rates") => crate::tedb::suggest(hc, doc).await.map(Some),
            _ => Ok(None),
        }
    }
}

// ───────────────────────────── which VAT applies ─────────────────────────────

/// What a line is, for where it is taxed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Goods,
    Services,
    /// Telecommunications, broadcasting and electronically supplied services, taxed where the
    /// consumer is (Art. 58).
    Digital,
}

pub fn kind_of(item: &Doc) -> Kind {
    match s(item, "eu_kind") {
        Some("Services") => Kind::Services,
        Some("Digital Services") => Kind::Digital,
        Some("Goods") => Kind::Goods,
        _ if b(item, "is_stock_item") => Kind::Goods,
        _ => Kind::Services,
    }
}

/// How a line is taxed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Treatment {
    /// At the rates of `country` (ISO); `oss` when declared through the One-Stop Shop.
    Taxed { country: String, oss: bool },
    /// No VAT on the invoice, for this category and VATEX reason.
    Zero { category: &'static str, reason: &'static str },
}

/// Where a document's supply goes and to whom, from the company's side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub home: String,
    pub side: Side,
    /// The other party's country (ISO), when known.
    pub other: Option<String>,
    /// The customer or supplier is a business (has a VAT or tax number).
    pub business: bool,
    /// The company declares consumer sales elsewhere in the EU through the One-Stop Shop.
    pub oss: bool,
}

impl Plan {
    pub fn treat(&self, kind: Kind) -> Treatment {
        let home = || Treatment::Taxed { country: self.home.clone(), oss: false };
        let Some(other) = self.other.as_deref().filter(|o| *o != self.home) else { return home() };
        match self.side {
            Side::Selling if member(other) => match (self.business, kind) {
                (true, Kind::Goods) => Treatment::Zero { category: "Intra-Community", reason: "VATEX-EU-IC" },
                (true, _) => Treatment::Zero { category: "Reverse Charge", reason: "VATEX-EU-AE" },
                (false, Kind::Goods | Kind::Digital) if self.oss => Treatment::Taxed { country: other.to_string(), oss: true },
                (false, _) => home(),
            },
            Side::Selling => match (self.business, kind) {
                (false, Kind::Services) => home(),
                _ => Treatment::Zero { category: "Export", reason: "VATEX-EU-G" },
            },
            // Acquisitions from the EU and services from abroad: the company owes the VAT itself.
            Side::Buying if member(other) && self.business => home(),
            Side::Buying if !member(other) && kind == Kind::Goods => Treatment::Zero { category: "Out of Scope", reason: "VATEX-EU-O" },
            Side::Buying if !member(other) => home(),
            Side::Buying => home(),
        }
    }

    /// The purchase is reverse-charged: the company declares the supplier's VAT as its own.
    pub fn reverse_charged(&self) -> bool {
        self.side == Side::Buying && self.other.as_deref().is_some_and(|o| o != self.home && (!member(o) || self.business))
    }

    /// The document's supply in words (its `eu_supply`).
    pub fn label(&self, treatments: &[Treatment]) -> &'static str {
        let other = self.other.as_deref().filter(|o| *o != self.home);
        match (self.side, other) {
            (_, None) => "Domestic",
            (Side::Selling, Some(o)) if member(o) && self.business => "Intra-EU B2B",
            (Side::Selling, Some(o)) if member(o) => {
                if treatments.iter().any(|t| matches!(t, Treatment::Taxed { oss: true, .. })) {
                    "Intra-EU B2C (OSS)"
                } else {
                    "Intra-EU B2C (home VAT)"
                }
            }
            (Side::Selling, Some(_)) => "Export",
            (Side::Buying, Some(o)) if member(o) && self.business => "Intra-EU Acquisition",
            (Side::Buying, Some(o)) if member(o) => "Domestic",
            (Side::Buying, Some(_)) => "Import",
        }
    }
}

/// A company in a member state: its ISO code.
pub fn home_of(company: &Doc) -> Option<String> {
    s(company, "country_code").filter(|c| member(c)).map(str::to_string)
}

async fn party_of(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc, side: Side) -> Result<Option<Doc>> {
    let (field, entity) = match side {
        Side::Selling => ("customer", "Customer"),
        Side::Buying => ("supplier", "Supplier"),
    };
    match id(doc, field) {
        Some(p) => Ok(Some(get(k, ctx, tx, entity, p).await?)),
        None => Ok(None),
    }
}

/// The document's plan, when its company is in a member state.
pub async fn plan(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc, side: Side) -> Result<Option<(Plan, Doc, Option<Doc>)>> {
    let Some(cid) = id(doc, "company") else { return Ok(None) };
    let company = get(k, ctx, tx, "Company", cid).await?;
    let Some(home) = home_of(&company) else { return Ok(None) };
    let party = party_of(k, ctx, tx, doc, side).await?;
    let vat = party.as_ref().and_then(|p| s(p, "eu_vat_number")).map(str::to_string);
    // Where the goods go (sales) or come from (purchases): the address on the document, else the
    // party's VAT number's country.
    let address = match side {
        Side::Selling => ship_to(k, ctx, tx, doc).await?,
        Side::Buying => match id(doc, "supplier_address") {
            Some(a) => find(k, ctx, tx, "Address", vec![Filter::eq("id", a.to_string())], None, 1).await?.into_iter().next(),
            None => None,
        },
    };
    let other = address.as_ref().and_then(|a| s(a, "country_code")).map(str::to_string).or_else(|| vat.as_deref().and_then(vat_country));
    let business = vat.is_some() || party.as_ref().is_some_and(|p| s(p, "tax_id").is_some() || s(p, "customer_type") == Some("Company"));
    let oss = s(&company, "eu_distance_sales") == Some("Destination's VAT (OSS)");
    // Within the EU a business is one with a VAT number; outside it, one with a tax number.
    let business = business && (side == Side::Buying || vat.is_some() || other.as_deref().is_some_and(|o| !member(o)));
    Ok(Some((Plan { home, side, other, business, oss }, company, party)))
}

/// A member state's standard rate on a day: the latest that took effect by then (the earliest
/// known before any).
pub async fn standard_rate(k: &Kernel, ctx: &Ctx, tx: &mut Tx, iso: &str, on: &str) -> Result<Decimal> {
    let ms = vies_code(iso);
    let rates = find(k, ctx, tx, "EuVatRate", vec![Filter::eq("member_state", ms), Filter::eq("kind", "Standard")], Some("valid_from asc"), 500).await?;
    let pick = |rows: Vec<&Doc>| rows.iter().map(|r| d(r, "rate")).max();
    let mut by_day: Vec<(&str, Vec<&Doc>)> = Vec::new();
    for r in &rates {
        let day = s(r, "valid_from").unwrap_or_default();
        match by_day.last_mut() {
            Some((d0, v)) if *d0 == day => v.push(r),
            _ => by_day.push((day, vec![r])),
        }
    }
    // Regional rates (the Canary Islands, Madeira) are lower than the mainland's standard rate.
    let effective = by_day.iter().rev().find(|(day, _)| *day <= on).or(by_day.first());
    effective.and_then(|(_, rows)| pick(rows.clone())).ok_or_else(|| {
        Error::invalid(format!("no VAT rate of {iso} is known: update the EU VAT rates (a company's Update EU VAT Rates) or add one under EU VAT Rates"))
    })
}

async fn account(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, number: &str) -> Result<String> {
    let found = find(k, ctx, tx, "Account", vec![Filter::eq("company", company.to_string()), Filter::eq("account_number", number)], None, 1).await?;
    found.first().and_then(|a| s(a, "id")).map(str::to_string).ok_or_else(|| {
        let name = ADDED_ACCOUNTS.iter().find(|(n, _)| *n == number).map(|(_, a)| *a).unwrap_or(if number == INPUT_TAX { "Input Tax" } else { "Output Tax" });
        Error::invalid(format!("the company has no account {number} ({name}) for EU VAT: run Set Up EU VAT Accounts on it"))
    })
}

/// The OSS and reverse-charge accounts of a company in a member state, made when missing: the
/// numbers of those it made.
async fn add_accounts(hc: &mut HookCtx<'_>, company: Uuid) -> Result<Vec<String>> {
    let sctx = sys(hc.ctx);
    let co = hc.kernel.get_tx(&sctx, hc.tx, "Company", company, false).await?;
    if home_of(&co).is_none() {
        return Err(Error::invalid("the company is not in an EU member state (its country)"));
    }
    let parent =
        find(hc.kernel, hc.ctx, hc.tx, "Account", vec![Filter::eq("company", company.to_string()), Filter::eq("account_name", "Duties and Taxes")], None, 1)
            .await?;
    let parent = parent
        .first()
        .and_then(|a| s(a, "id"))
        .map(str::to_string)
        .ok_or_else(|| Error::invalid("the company's chart has no Duties and Taxes group: create its chart of accounts first"))?;
    let mut made = Vec::new();
    for (number, name) in ADDED_ACCOUNTS {
        let exists =
            find(hc.kernel, hc.ctx, hc.tx, "Account", vec![Filter::eq("company", company.to_string()), Filter::eq("account_number", *number)], None, 1).await?;
        if !exists.is_empty() {
            continue;
        }
        let a = json!({"account_name": name, "account_number": number, "company": company.to_string(), "is_group": false, "root_type": "Liability",
                       "account_type": "Tax", "parent_account": parent, "account_currency": co.get("default_currency")});
        if let Value::Object(a) = a {
            hc.kernel.insert_tx(&sctx, hc.tx, "Account", a).await?;
        }
        made.push(number.to_string());
    }
    Ok(made)
}

/// The legal mentions an invoice carries for what it holds (Art. 226(11) and (11a)).
pub fn legal_text(treatments: &[(Treatment, Kind)], plan: &Plan) -> Option<String> {
    let mut notes: Vec<String> = Vec::new();
    let mut add = |t: String| {
        if !notes.contains(&t) {
            notes.push(t);
        }
    };
    for (t, kind) in treatments {
        match t {
            Treatment::Zero { category: "Intra-Community", .. } => add("Exempt intra-Community supply of goods (Article 138 of Directive 2006/112/EC).".into()),
            Treatment::Zero { category: "Reverse Charge", .. } => {
                add("Reverse charge: VAT to be accounted for by the customer (Article 196 of Directive 2006/112/EC).".into())
            }
            Treatment::Zero { category: "Export", .. } if *kind == Kind::Goods => add("Exempt export of goods (Article 146 of Directive 2006/112/EC).".into()),
            Treatment::Zero { category: "Export", .. } => {
                add("Services supplied outside the EU: not subject to EU VAT (Articles 44 and 59 of Directive 2006/112/EC).".into())
            }
            Treatment::Taxed { country, oss: true } => add(format!("VAT of {country} charged under the One-Stop Shop (Union scheme).")),
            _ => {}
        }
    }
    if plan.side == Side::Buying && plan.reverse_charged() {
        add("Reverse charge: VAT accounted for by the company as the customer.".into());
    }
    (!notes.is_empty()).then(|| notes.join("\n"))
}

/// The EU's rules for ERP.
pub struct Eu;

impl Eu {
    /// The tax row the plan's lines take: (account number, the rate at the row, reverse charge).
    async fn row_for(k: &Kernel, ctx: &Ctx, tx: &mut Tx, plan: &Plan, treatments: &[Treatment], on: &str) -> Result<(String, Decimal, bool, &'static str)> {
        let oss: Vec<&String> = treatments.iter().filter_map(|t| if let Treatment::Taxed { country, oss: true } = t { Some(country) } else { None }).collect();
        let home_taxed = treatments.iter().any(|t| matches!(t, Treatment::Taxed { oss: false, .. }));
        if let Some(dest) = oss.first() {
            if home_taxed || oss.iter().any(|c| c != dest) {
                return Err(Error::invalid(format!(
                    "this invoice mixes supplies taxed in {dest} through the One-Stop Shop and others taxed elsewhere: invoice them separately"
                )));
            }
            return Ok((OSS_VAT.into(), standard_rate(k, ctx, tx, dest, on).await?, false, "VAT (OSS)"));
        }
        let (number, label) = if plan.side == Side::Selling { (OUTPUT_TAX, "VAT") } else { (INPUT_TAX, "VAT") };
        let rate = if home_taxed { standard_rate(k, ctx, tx, &plan.home, on).await? } else { Decimal::ZERO };
        Ok((number.into(), rate, plan.reverse_charged() && home_taxed, label))
    }
}

async fn line_kinds(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc) -> Result<Vec<Kind>> {
    let mut cache: HashMap<Uuid, Kind> = HashMap::new();
    let mut out = Vec::new();
    for r in rows(doc, "items") {
        let Some(item) = id(&r, "item") else {
            out.push(Kind::Goods);
            continue;
        };
        if let std::collections::hash_map::Entry::Vacant(e) = cache.entry(item) {
            e.insert(kind_of(&get(k, ctx, tx, "Item", item).await?));
        }
        out.push(cache[&item]);
    }
    Ok(out)
}

fn day_of(doc: &Doc, ctx: &Ctx) -> String {
    s(doc, "posting_date").map(str::to_string).unwrap_or_else(|| ctx.today().format("%Y-%m-%d").to_string())
}

/// The tax row of the EU's: its account is one of a company's VAT accounts.
fn is_vat_row(row: &Doc, accounts: &[String]) -> bool {
    s(row, "account").is_some_and(|a| accounts.iter().any(|x| x == a))
}

#[async_trait]
impl Localization for Eu {
    fn package(&self) -> &'static str {
        crate::PACKAGE
    }

    async fn determine(&self, k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()> {
        if doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 {
            return Ok(());
        }
        let Some((plan, company, party)) = plan(k, ctx, tx, doc, side).await? else { return Ok(()) };
        let kinds = line_kinds(k, ctx, tx, doc).await?;
        let treatments: Vec<Treatment> = kinds.iter().map(|kd| plan.treat(*kd)).collect();
        let label = plan.label(&treatments);
        let destination =
            treatments.iter().find_map(|t| if let Treatment::Taxed { country, oss: true } = t { Some(country.clone()) } else { None }).or(plan.other.clone());
        if ent.field("eu_supply").is_some() {
            set(doc, "eu_supply", label);
        }
        if ent.field("eu_destination").is_some() {
            set(doc, "eu_destination", destination.clone().map_or(Value::Null, Value::from));
        }
        if ent.field("eu_origin").is_some() {
            set(doc, "eu_origin", plan.other.clone().map_or(Value::Null, Value::from));
        }
        if ent.field("eu_legal_text").is_some() {
            let pairs: Vec<(Treatment, Kind)> = treatments.iter().cloned().zip(kinds.iter().copied()).collect();
            set(doc, "eu_legal_text", legal_text(&pairs, &plan).map_or(Value::Null, Value::from));
        }
        if ent.field("eu_customer_vat").is_some() {
            set(doc, "eu_customer_vat", party.as_ref().filter(|_| side == Side::Selling).and_then(|p| s(p, "eu_vat_number")).map_or(Value::Null, Value::from));
        }
        if ent.field("eu_vies_consultation").is_some() {
            let b2b = label == "Intra-EU B2B";
            set(doc, "eu_vies_consultation", party.as_ref().filter(|_| b2b).and_then(|p| s(p, "eu_vies_consultation")).map_or(Value::Null, Value::from));
        }
        // A tax template chosen by hand stands; otherwise the VAT row is the EU's.
        if id(doc, "tax_template").is_some() || ent.field("taxes").is_none() || treatments.is_empty() {
            return Ok(());
        }
        let cid = id(&company, "id").ok_or_else(|| Error::internal("company without id"))?;
        let (number, rate, reverse, description) = Eu::row_for(k, ctx, tx, &plan, &treatments, &day_of(doc, ctx)).await?;
        let account = account(k, ctx, tx, cid, &number).await?;
        let mut vat_accounts = Vec::new();
        for n in [INPUT_TAX, OUTPUT_TAX, OSS_VAT] {
            if let Ok(a) = self::account(k, ctx, tx, cid, n).await {
                vat_accounts.push(a);
            }
        }
        let owed = if reverse { Some(self::account(k, ctx, tx, cid, REVERSE_CHARGE_VAT).await?) } else { None };
        let mut taxes = rows(doc, "taxes");
        let current = taxes.iter().position(|r| is_vat_row(r, &vat_accounts));
        let same = current.is_some_and(|i| {
            let r = &taxes[i];
            s(r, "account") == Some(account.as_str())
                && d(r, "rate") == rate
                && b(r, "reverse_charge") == reverse
                && s(r, "reverse_charge_account") == owed.as_deref()
        });
        if same {
            return Ok(());
        }
        let row = json!({
            "id": Uuid::now_v7().to_string(), "charge_type": "On Net Total", "account": account, "description": description, "rate": rate.to_string(),
            "reverse_charge": reverse, "reverse_charge_account": owed, "category": if reverse { Some("Reverse Charge") } else { None::<&str> },
        });
        let Value::Object(row) = row else { return Ok(()) };
        match current {
            Some(i) => taxes[i] = row,
            None => taxes.insert(0, row),
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
        let Some((plan, company, _)) = plan(k, ctx, tx, doc, side).await? else { return Ok(()) };
        let Some(cid) = id(&company, "id") else { return Ok(()) };
        let mut vat_accounts = Vec::new();
        for n in [INPUT_TAX, OUTPUT_TAX, OSS_VAT] {
            if let Ok(a) = account(k, ctx, tx, cid, n).await {
                vat_accounts.push(a);
            }
        }
        let Some(row) = taxes.iter().find(|r| is_vat_row(r, &vat_accounts)) else { return Ok(()) };
        let vat_account = s(row, "account").unwrap_or_default().to_string();
        let row_rate = d(row, "rate");
        let mut errors = Vec::new();
        for (i, (line, item)) in lines.iter().zip(items).enumerate() {
            if id(line, "item").is_none() {
                continue;
            }
            let treatment = plan.treat(kind_of(item));
            let (rate, category, reason): (Decimal, Option<&str>, Option<String>) = match &treatment {
                Treatment::Zero { category, reason } => (Decimal::ZERO, Some(category), Some(reason.to_string())),
                Treatment::Taxed { country, .. } => {
                    let own = rows(item, "eu_rates").into_iter().find(|r| s(r, "member_state").map(iso_code) == Some(country.as_str()));
                    match s(item, "eu_vat_category").unwrap_or("Standard") {
                        "Zero Rated" => (Decimal::ZERO, Some("Zero Rated"), None),
                        "Exempt" => (Decimal::ZERO, Some("Exempt"), Some(s(item, "eu_exemption_reason").unwrap_or("Exempt").to_string())),
                        "Reduced" => match own {
                            Some(r) => (d(&r, "rate"), Some("Reduced"), None),
                            None => {
                                let name = s(item, "item_name").or_else(|| s(item, "item_code")).unwrap_or("this item");
                                errors.push(
                                    FieldError::new(
                                        "item",
                                        format!("{name} is reduced-rated: set its rate in {country} (EU Rates by Country, or Suggest EU Rates)"),
                                    )
                                    .in_row("items", i),
                                );
                                continue;
                            }
                        },
                        _ => (own.map(|r| d(&r, "rate")).unwrap_or(row_rate), Some("Standard"), None),
                    }
                }
            };
            let category = if plan.reverse_charged() && rate > Decimal::ZERO { Some("Reverse Charge") } else { category };
            let entry = json!({"tax_account": vat_account, "rate": rate.to_string(), "category": category, "exemption_reason": reason});
            let Value::Object(entry) = entry else { continue };
            rates[i].retain(|r| s(r, "tax_account") != Some(vat_account.as_str()));
            rates[i].push(entry);
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn check(&self, hc: &mut HookCtx<'_>, doc: &Doc, side: Side) -> Result<()> {
        if side != Side::Selling || hc.entity.def.name != "SalesInvoice" {
            return Ok(());
        }
        let Some((plan, company, party)) = plan(hc.kernel, hc.ctx, hc.tx, doc, side).await? else { return Ok(()) };
        if s(&company, "eu_vat_number").is_none() {
            return Err(Error::invalid("set the company's EU VAT number before invoicing: invoices carry it (Article 226 of Directive 2006/112/EC)"));
        }
        if plan.label(&[]) == "Intra-EU B2B" {
            let party = party.unwrap_or_default();
            let vat = s(&party, "eu_vat_number").unwrap_or_default();
            if !b(&party, "eu_vies_valid") {
                return Err(Error::invalid(format!(
                    "the customer's VAT number {vat} is not confirmed in VIES: check it (Check VAT Number on the customer) before invoicing a supply without VAT"
                )));
            }
        }
        Ok(())
    }

    async fn setup_company(&self, hc: &mut HookCtx<'_>, company: Uuid, _accounts: &HashMap<&str, String>) -> Result<()> {
        let co = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "Company", company, false).await?;
        if home_of(&co).is_none() {
            return Ok(());
        }
        add_accounts(hc, company).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vat_numbers_take_their_country_form() {
        for good in ["DE123456789", "ATU12345678", "FRXX123456789", "NL123456789B01", "ESA1234567B", "EL123456789", "IE1234567WA", "IE1S12345L", "BE0123456789"]
        {
            assert_eq!(valid_vat_format(good), Ok(()), "{good}");
        }
        for bad in ["DE12345678", "AT12345678", "FRIO123456789", "NL123456789X01", "US123456789", "D"] {
            assert!(valid_vat_format(bad).is_err(), "{bad}");
        }
        assert_eq!(normalize_vat(" gr 123.456-789 "), "EL123456789");
        assert_eq!(vat_country("EL123456789").as_deref(), Some("GR"));
        assert_eq!(vat_country("XX1"), None);
    }

    fn plan(side: Side, other: Option<&str>, business: bool, oss: bool) -> Plan {
        Plan { home: "DE".into(), side, other: other.map(str::to_string), business, oss }
    }

    #[test]
    fn where_each_supply_is_taxed() {
        let taxed = |c: &str, oss| Treatment::Taxed { country: c.into(), oss };
        let zero = |category, reason| Treatment::Zero { category, reason };
        // At home.
        assert_eq!(plan(Side::Selling, Some("DE"), true, false).treat(Kind::Goods), taxed("DE", false));
        assert_eq!(plan(Side::Selling, None, false, false).treat(Kind::Services), taxed("DE", false));
        // To businesses elsewhere in the EU.
        let b2b = plan(Side::Selling, Some("FR"), true, true);
        assert_eq!(b2b.treat(Kind::Goods), zero("Intra-Community", "VATEX-EU-IC"));
        assert_eq!(b2b.treat(Kind::Services), zero("Reverse Charge", "VATEX-EU-AE"));
        assert_eq!(b2b.label(&[]), "Intra-EU B2B");
        // To consumers elsewhere in the EU: the destination's VAT through the OSS, or the home's.
        let oss = plan(Side::Selling, Some("FR"), false, true);
        assert_eq!((oss.treat(Kind::Goods), oss.treat(Kind::Digital), oss.treat(Kind::Services)), (taxed("FR", true), taxed("FR", true), taxed("DE", false)));
        assert_eq!(oss.label(&[taxed("FR", true)]), "Intra-EU B2C (OSS)");
        assert_eq!(plan(Side::Selling, Some("FR"), false, false).treat(Kind::Goods), taxed("DE", false));
        // Outside the EU.
        let export = plan(Side::Selling, Some("US"), false, false);
        assert_eq!(
            (export.treat(Kind::Goods), export.treat(Kind::Digital), export.treat(Kind::Services)),
            (zero("Export", "VATEX-EU-G"), zero("Export", "VATEX-EU-G"), taxed("DE", false))
        );
        assert_eq!(plan(Side::Selling, Some("US"), true, false).treat(Kind::Services), zero("Export", "VATEX-EU-G"));
        // Purchases: acquisitions and services from abroad are reverse-charged; imported goods carry no VAT.
        let acquisition = plan(Side::Buying, Some("FR"), true, false);
        assert!(acquisition.reverse_charged());
        assert_eq!((acquisition.treat(Kind::Goods), acquisition.label(&[])), (taxed("DE", false), "Intra-EU Acquisition"));
        let import = plan(Side::Buying, Some("CN"), true, false);
        assert_eq!((import.treat(Kind::Goods), import.treat(Kind::Services)), (zero("Out of Scope", "VATEX-EU-O"), taxed("DE", false)));
        assert!(!plan(Side::Buying, Some("DE"), true, false).reverse_charged());
    }

    #[test]
    fn invoices_say_why_there_is_no_vat() {
        let p = plan(Side::Selling, Some("FR"), true, false);
        let text = legal_text(
            &[
                (Treatment::Zero { category: "Intra-Community", reason: "VATEX-EU-IC" }, Kind::Goods),
                (Treatment::Zero { category: "Reverse Charge", reason: "VATEX-EU-AE" }, Kind::Services),
                (Treatment::Zero { category: "Intra-Community", reason: "VATEX-EU-IC" }, Kind::Goods),
            ],
            &p,
        )
        .unwrap();
        assert_eq!(text.lines().count(), 2, "each mention once: {text}");
        assert!(text.contains("Article 138") && text.contains("Reverse charge"));
        assert_eq!(legal_text(&[(Treatment::Taxed { country: "DE".into(), oss: false }, Kind::Goods)], &plan(Side::Selling, None, false, false)), None);
    }
}
