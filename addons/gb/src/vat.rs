//! UK VAT on ERP's documents, for a company in the United Kingdom (VAT Act 1994): at home the
//! item's rate (standard 20%, reduced 5%, zero-rated, exempt); construction services to a
//! VAT-registered customer who is not the end user reverse-charged (section 55A, the CIS reverse
//! charge); goods exported zero-rated; services to businesses abroad, and digital services to
//! consumers abroad, outside the scope of UK VAT. Purchases: services from abroad and construction
//! services under the CIS are reverse-charged by the company itself; imported goods carry no VAT
//! on the supplier's invoice (it is accounted for at import).

use crate::util::{b, find, get, id, rows, s, set, sys};
use async_trait::async_trait;
use ferrum_app_erp::localization::{Localization, TaxId, field_tax_ids, ship_to};
use ferrum_app_erp::pricing::Side;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, Filter, HookCtx, Kernel, Result, Tx};
use ferrum_meta::EffEntity;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

pub const STANDARD_RATE: Decimal = Decimal::from_parts(20, 0, 0, false, 0);
pub const REDUCED_RATE: Decimal = Decimal::from_parts(5, 0, 0, false, 0);
pub const INPUT_TAX: &str = "1151";
pub const OUTPUT_TAX: &str = "2131";
pub const REVERSE_CHARGE_VAT: &str = "2151";
pub const CIS_NOTE: &str = "Reverse charge: customer to account to HMRC for the VAT on these construction services (section 55A VAT Act 1994).";
pub const EXPORT_NOTE: &str = "Zero-rated export of goods.";
pub const OUTSIDE_NOTE: &str = "Services supplied outside the UK: outside the scope of UK VAT.";

// ───────────────────────────── VAT registration numbers ─────────────────────────────

/// A VAT registration number as HMRC takes it: nine digits (twelve with a branch), without the GB
/// prefix or spaces.
pub fn normalize_vrn(raw: &str) -> String {
    let n: String = raw.chars().filter(|c| c.is_ascii_alphanumeric()).flat_map(char::to_uppercase).collect();
    n.strip_prefix("GB").map(str::to_string).unwrap_or(n)
}

/// HMRC's check: the first seven digits weighted 8 to 2, plus the last two, a multiple of 97 (or,
/// for newer numbers, a multiple of 97 once 55 is added).
pub fn valid_vrn(vrn: &str) -> bool {
    if !(vrn.len() == 9 || vrn.len() == 12) || !vrn.bytes().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let d: Vec<u32> = vrn.bytes().take(9).map(|c| u32::from(c - b'0')).collect();
    let sum: u32 = d[..7].iter().zip([8, 7, 6, 5, 4, 3, 2]).map(|(a, w)| a * w).sum::<u32>() + d[7] * 10 + d[8];
    sum.is_multiple_of(97) || (sum + 55).is_multiple_of(97)
}

/// Normalizes and checks `gb_vrn` on companies, customers and suppliers.
pub struct VrnHook;

#[async_trait]
impl DocHook for VrnHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.field("gb_vrn").is_none() {
            return Ok(());
        }
        let vrn = s(doc, "gb_vrn").map(normalize_vrn).filter(|v| !v.is_empty());
        if let Some(v) = &vrn
            && !valid_vrn(v)
        {
            return Err(Error::Validation(vec![FieldError::new("gb_vrn", "is not a UK VAT registration number (nine digits whose check digits hold)")]));
        }
        set(doc, "gb_vrn", vrn.map_or(Value::Null, Value::from));
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if (hc.entity.def.name.as_str(), name) != ("Company", "gb_connect_hmrc") {
            return Ok(None);
        }
        crate::mtd::connect(hc, doc).await.map(Some)
    }
}

// ───────────────────────────── which VAT applies ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Goods,
    Services,
    Digital,
}

pub fn kind_of(item: &Doc) -> Kind {
    match s(item, "gb_kind") {
        Some("Services") => Kind::Services,
        Some("Digital Services") => Kind::Digital,
        Some("Goods") => Kind::Goods,
        _ if b(item, "is_stock_item") => Kind::Goods,
        _ => Kind::Services,
    }
}

/// How a line is taxed: at the item's UK rate, or not at all, for a category and reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Treatment {
    Taxed,
    Zero { category: &'static str, reason: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub side: Side,
    /// The other party's country, when known; `None` or GB is at home.
    pub other: Option<String>,
    /// The customer or supplier is VAT-registered in the UK.
    pub registered: bool,
    /// The customer or supplier is a business.
    pub business: bool,
}

impl Plan {
    pub fn abroad(&self) -> bool {
        self.other.as_deref().is_some_and(|o| o != "GB")
    }

    pub fn treat(&self, kind: Kind, cis: bool) -> Treatment {
        match (self.side, self.abroad()) {
            (Side::Selling, false) if cis && self.registered => Treatment::Zero { category: "Reverse Charge", reason: "CIS reverse charge" },
            (Side::Selling, false) => Treatment::Taxed,
            (Side::Selling, true) => match (kind, self.business) {
                (Kind::Goods, _) => Treatment::Zero { category: "Export", reason: "Export" },
                (_, true) | (Kind::Digital, false) => Treatment::Zero { category: "Out of Scope", reason: "Outside the scope of UK VAT" },
                (Kind::Services, false) => Treatment::Taxed,
            },
            (Side::Buying, true) if kind == Kind::Goods => Treatment::Zero { category: "Out of Scope", reason: "Import VAT accounted for at import" },
            (Side::Buying, _) => Treatment::Taxed,
        }
    }

    /// The company accounts for its supplier's VAT itself (services from abroad, CIS services).
    pub fn reverse_charged(&self, kinds: &[(Kind, bool)]) -> bool {
        self.side == Side::Buying && kinds.iter().any(|(k, cis)| (self.abroad() && *k != Kind::Goods) || (!self.abroad() && *cis))
    }
}

pub fn rate_of(item: &Doc) -> (Decimal, &'static str) {
    match s(item, "gb_vat_category").unwrap_or("Standard") {
        "Reduced" => (REDUCED_RATE, "Reduced"),
        "Zero Rated" => (Decimal::ZERO, "Zero Rated"),
        "Exempt" => (Decimal::ZERO, "Exempt"),
        _ => (STANDARD_RATE, "Standard"),
    }
}

async fn plan(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc, side: Side) -> Result<Option<(Plan, Doc)>> {
    let Some(cid) = id(doc, "company") else { return Ok(None) };
    let company = get(k, ctx, tx, "Company", cid).await?;
    if s(&company, "country_code") != Some("GB") {
        return Ok(None);
    }
    let (field, entity) = if side == Side::Selling { ("customer", "Customer") } else { ("supplier", "Supplier") };
    let party = match id(doc, field) {
        Some(p) => Some(get(k, ctx, tx, entity, p).await?),
        None => None,
    };
    let address = match side {
        Side::Selling => ship_to(k, ctx, tx, doc).await?,
        Side::Buying => match id(doc, "supplier_address") {
            Some(a) => find(k, ctx, tx, "Address", vec![Filter::eq("id", a.to_string())], None, 1).await?.into_iter().next(),
            None => None,
        },
    };
    let registered = party.as_ref().is_some_and(|p| s(p, "gb_vrn").is_some());
    let other = address.as_ref().and_then(|a| s(a, "country_code")).map(str::to_string).or_else(|| registered.then(|| "GB".to_string()));
    let business = registered || party.as_ref().is_some_and(|p| s(p, "tax_id").is_some() || s(p, "customer_type") == Some("Company") || side == Side::Buying);
    Ok(Some((Plan { side, other, registered, business }, company)))
}

pub async fn account(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, number: &str) -> Result<String> {
    let found = find(k, ctx, tx, "Account", vec![Filter::eq("company", company.to_string()), Filter::eq("account_number", number)], None, 1).await?;
    found
        .first()
        .and_then(|a| s(a, "id"))
        .map(str::to_string)
        .ok_or_else(|| Error::invalid(format!("the company has no account {number} for UK VAT: create its chart of accounts first")))
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

/// The UK's rules for ERP.
pub struct Uk;

#[async_trait]
impl Localization for Uk {
    fn package(&self) -> &'static str {
        crate::PACKAGE
    }

    /// The UK VAT registration number of a company, customer or supplier; with one, a party is a
    /// registered business.
    fn tax_ids(&self, ctx: &Ctx, entity: &str, record: &Doc) -> Vec<TaxId> {
        field_tax_ids(ctx, entity, record, &["gb_vrn"])
    }

    fn registered(&self, _ctx: &Ctx, _entity: &str, record: &Doc) -> Option<bool> {
        s(record, "gb_vrn").map(|_| true)
    }

    async fn determine(&self, k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()> {
        if doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 {
            return Ok(());
        }
        let Some((plan, company)) = plan(k, ctx, tx, doc, side).await? else { return Ok(()) };
        let items = items_of(k, ctx, tx, doc).await?;
        let kinds: Vec<(Kind, bool)> = items.iter().map(|i| (kind_of(i), b(i, "gb_cis_reverse_charge"))).collect();
        if ent.field("gb_vat_notes").is_some() {
            let mut notes: Vec<&str> = Vec::new();
            for (kind, cis) in &kinds {
                let note = match plan.treat(*kind, *cis) {
                    Treatment::Zero { category: "Reverse Charge", .. } => CIS_NOTE,
                    Treatment::Zero { category: "Export", .. } => EXPORT_NOTE,
                    Treatment::Zero { category: "Out of Scope", .. } => OUTSIDE_NOTE,
                    _ => continue,
                };
                if !notes.contains(&note) {
                    notes.push(note);
                }
            }
            set(doc, "gb_vat_notes", if notes.is_empty() { Value::Null } else { json!(notes.join("\n")) });
        }
        if id(doc, "tax_template").is_some() || ent.field("taxes").is_none() || items.is_empty() {
            return Ok(());
        }
        let cid = id(&company, "id").ok_or_else(|| Error::internal("company without id"))?;
        let reverse = plan.reverse_charged(&kinds);
        if side == Side::Buying && reverse && !plan.abroad() && kinds.iter().any(|(_, cis)| !cis) {
            return Err(Error::invalid(
                "this purchase mixes construction services under the CIS reverse charge with other supplies: enter them on separate invoices",
            ));
        }
        let number = if side == Side::Selling { OUTPUT_TAX } else { INPUT_TAX };
        let vat_account = account(k, ctx, tx, cid, number).await?;
        let owed = if reverse { Some(account(k, ctx, tx, cid, REVERSE_CHARGE_VAT).await?) } else { None };
        let mut taxes = rows(doc, "taxes");
        let current = taxes.iter().position(|r| s(r, "account") == Some(vat_account.as_str()));
        if current.is_some_and(|i| b(&taxes[i], "reverse_charge") == reverse && s(&taxes[i], "reverse_charge_account") == owed.as_deref()) {
            return Ok(());
        }
        let Value::Object(row) = json!({
            "id": Uuid::now_v7().to_string(), "charge_type": "On Net Total", "account": vat_account, "description": "VAT", "rate": STANDARD_RATE.to_string(),
            "reverse_charge": reverse, "reverse_charge_account": owed, "category": if reverse { Some("Reverse Charge") } else { None::<&str> },
        }) else {
            return Ok(());
        };
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
        let Some((plan, company)) = plan(k, ctx, tx, doc, side).await? else { return Ok(()) };
        let Some(cid) = id(&company, "id") else { return Ok(()) };
        let number = if side == Side::Selling { OUTPUT_TAX } else { INPUT_TAX };
        let Ok(vat_account) = account(k, ctx, tx, cid, number).await else { return Ok(()) };
        if !taxes.iter().any(|t| s(t, "account") == Some(vat_account.as_str())) {
            return Ok(());
        }
        let kinds: Vec<(Kind, bool)> = items.iter().map(|i| (kind_of(i), b(i, "gb_cis_reverse_charge"))).collect();
        let reverse = plan.reverse_charged(&kinds);
        for (i, (line, item)) in lines.iter().zip(items).enumerate() {
            if id(line, "item").is_none() {
                continue;
            }
            let (rate, category, reason) = match plan.treat(kind_of(item), b(item, "gb_cis_reverse_charge")) {
                Treatment::Zero { category, reason } => (Decimal::ZERO, category, Some(reason)),
                Treatment::Taxed => {
                    let (rate, category) = rate_of(item);
                    let category = if reverse && rate > Decimal::ZERO { "Reverse Charge" } else { category };
                    (rate, category, None)
                }
            };
            let Value::Object(entry) = json!({"tax_account": vat_account, "rate": rate.to_string(), "category": category, "exemption_reason": reason}) else {
                continue;
            };
            rates[i].retain(|r| s(r, "tax_account") != Some(vat_account.as_str()));
            rates[i].push(entry);
        }
        Ok(())
    }

    async fn check(&self, hc: &mut HookCtx<'_>, doc: &Doc, side: Side) -> Result<()> {
        if side != Side::Selling || hc.entity.def.name != "SalesInvoice" {
            return Ok(());
        }
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let company = get(hc.kernel, hc.ctx, hc.tx, "Company", cid).await?;
        if s(&company, "country_code") == Some("GB")
            && s(&company, "gb_vrn").is_none()
            && rows(doc, "taxes").iter().any(|t| crate::util::d(t, "tax_amount") != Decimal::ZERO)
        {
            return Err(Error::invalid("a VAT invoice carries the company's VAT registration number: set it on the company"));
        }
        Ok(())
    }

    async fn setup_company(&self, hc: &mut HookCtx<'_>, company: Uuid, accounts: &HashMap<&str, String>) -> Result<()> {
        let sctx = sys(hc.ctx);
        let co = hc.kernel.get_tx(&sctx, hc.tx, "Company", company, false).await?;
        if s(&co, "country_code") != Some("GB") {
            return Ok(());
        }
        let Some(parent) = accounts.get("Duties and Taxes") else { return Ok(()) };
        let Value::Object(a) = json!({"account_name": "VAT Owed (reverse charge)", "account_number": REVERSE_CHARGE_VAT, "company": company.to_string(), "is_group": false,
                                      "root_type": "Liability", "account_type": "Tax", "parent_account": parent, "account_currency": co.get("default_currency")})
        else {
            return Ok(());
        };
        hc.kernel.insert_tx(&sctx, hc.tx, "Account", a).await.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vat_registration_numbers_hold_their_check_digits() {
        // 1234567 weighs 112: 82 makes 194 (old numbers), 27 makes 139 + 55 = 194 (new ones).
        for good in ["123456782", "123456727", "123456782001"] {
            assert!(valid_vrn(good), "{good}");
        }
        for bad in ["123456781", "12345678", "12345678A"] {
            assert!(!valid_vrn(bad), "{bad}");
        }
        assert_eq!(normalize_vrn("gb 123 4567 82"), "123456782");
    }

    #[test]
    fn where_each_supply_is_taxed() {
        let p = |side, other: Option<&str>, registered, business| Plan { side, other: other.map(str::to_string), registered, business };
        let zero = |category, reason| Treatment::Zero { category, reason };
        assert_eq!(p(Side::Selling, Some("GB"), true, true).treat(Kind::Services, true), zero("Reverse Charge", "CIS reverse charge"));
        assert_eq!(p(Side::Selling, Some("GB"), false, false).treat(Kind::Services, true), Treatment::Taxed, "the end user pays the VAT");
        assert_eq!(p(Side::Selling, Some("US"), false, true).treat(Kind::Goods, false), zero("Export", "Export"));
        assert_eq!(p(Side::Selling, Some("FR"), false, true).treat(Kind::Services, false), zero("Out of Scope", "Outside the scope of UK VAT"));
        assert_eq!(p(Side::Selling, Some("FR"), false, false).treat(Kind::Services, false), Treatment::Taxed);
        assert_eq!(p(Side::Selling, Some("FR"), false, false).treat(Kind::Digital, false), zero("Out of Scope", "Outside the scope of UK VAT"));
        let import = p(Side::Buying, Some("US"), false, true);
        assert!(import.reverse_charged(&[(Kind::Services, false)]) && !import.reverse_charged(&[(Kind::Goods, false)]));
        assert!(p(Side::Buying, Some("GB"), true, true).reverse_charged(&[(Kind::Services, true)]));
        assert!(!p(Side::Buying, None, false, true).reverse_charged(&[(Kind::Goods, false)]));
        assert_eq!(rate_of(&json!({"gb_vat_category": "Reduced"}).as_object().cloned().unwrap()), (REDUCED_RATE, "Reduced"));
    }
}
