//! India GST: GSTIN validation (format, state and checksum), place of supply, automatic choice
//! between intra-state (CGST+SGST), inter-state (IGST) and export templates, and HSN checks.

use async_trait::async_trait;
use ferrum_app_erp::localization::{Localization, ship_to};
use ferrum_app_erp::pricing::Side;
use ferrum_domain::util::{b, get, id, rows, s, set, sys};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, Filter, HookCtx, Kernel, Result, Tx};
use ferrum_meta::EffEntity;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

pub const STATES: &[(&str, &str)] = &[
    ("01", "Jammu and Kashmir"),
    ("02", "Himachal Pradesh"),
    ("03", "Punjab"),
    ("04", "Chandigarh"),
    ("05", "Uttarakhand"),
    ("06", "Haryana"),
    ("07", "Delhi"),
    ("08", "Rajasthan"),
    ("09", "Uttar Pradesh"),
    ("10", "Bihar"),
    ("11", "Sikkim"),
    ("12", "Arunachal Pradesh"),
    ("13", "Nagaland"),
    ("14", "Manipur"),
    ("15", "Mizoram"),
    ("16", "Tripura"),
    ("17", "Meghalaya"),
    ("18", "Assam"),
    ("19", "West Bengal"),
    ("20", "Jharkhand"),
    ("21", "Odisha"),
    ("22", "Chhattisgarh"),
    ("23", "Madhya Pradesh"),
    ("24", "Gujarat"),
    ("26", "Dadra and Nagar Haveli and Daman and Diu"),
    ("27", "Maharashtra"),
    ("29", "Karnataka"),
    ("30", "Goa"),
    ("31", "Lakshadweep"),
    ("32", "Kerala"),
    ("33", "Tamil Nadu"),
    ("34", "Puducherry"),
    ("35", "Andaman and Nicobar Islands"),
    ("36", "Telangana"),
    ("37", "Andhra Pradesh"),
    ("38", "Ladakh"),
    ("97", "Other Territory"),
    ("99", "Centre Jurisdiction"),
];

/// Place-of-supply code used for exports.
pub const FOREIGN: &str = "96";

pub fn state_name(code: &str) -> Option<&'static str> {
    STATES.iter().find(|(c, _)| *c == code).map(|(_, n)| *n)
}

const CHARS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// The 15th character of a GSTIN: a Luhn mod-36 check over the first 14.
pub fn gstin_check_char(first14: &str) -> Option<char> {
    let mut sum = 0u32;
    for (i, c) in first14.bytes().enumerate() {
        let v = CHARS.iter().position(|x| *x == c)? as u32;
        let p = v * if i % 2 == 0 { 1 } else { 2 };
        sum += p / 36 + p % 36;
    }
    Some(CHARS[((36 - sum % 36) % 36) as usize] as char)
}

/// Validates a normalized (trimmed, upper-case) GSTIN.
pub fn validate_gstin(g: &str) -> std::result::Result<(), String> {
    if g.len() != 15 || !g.bytes().all(|c| c.is_ascii_digit() || c.is_ascii_uppercase()) {
        return Err("a GSTIN has 15 letters and digits".into());
    }
    let state = &g[..2];
    if state_name(state).is_none() {
        return Err(format!("'{state}' is not a valid GST state code"));
    }
    let pan = &g[2..12];
    let pan_ok = pan[..5].bytes().all(|c| c.is_ascii_uppercase()) && pan[5..9].bytes().all(|c| c.is_ascii_digit()) && pan.as_bytes()[9].is_ascii_uppercase();
    // Government (UIN/TAN-based) registrations do not embed a PAN; only regular ones are checked.
    if !pan_ok && g.as_bytes()[13] == b'Z' {
        return Err("characters 3-12 must be the holder's PAN".into());
    }
    match gstin_check_char(&g[..14]) {
        Some(c) if g.ends_with(c) => Ok(()),
        _ => Err("the GSTIN check digit does not match; please re-check the number".into()),
    }
}

fn normalize(doc: &mut Doc, field: &str) -> Option<String> {
    let v = s(doc, field).map(|x| x.trim().to_uppercase()).filter(|x| !x.is_empty());
    set(doc, field, v.clone().map(Value::String).unwrap_or(Value::Null));
    v
}

/// GSTIN, state code and category on Company, Customer and Supplier.
pub struct PartyGstHook;

#[async_trait]
impl DocHook for PartyGstHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.field("in_gstin").is_none() {
            return Ok(());
        }
        let gstin = normalize(doc, "in_gstin");
        normalize(doc, "in_state_code");
        if let Some(g) = &gstin
            && s(doc, "in_state_code").is_none()
            && g.len() >= 2
        {
            set(doc, "in_state_code", g[..2].to_string());
        }
        if hc.entity.field("in_gst_category").is_some() {
            match (gstin.is_some(), s(doc, "in_gst_category")) {
                (true, None | Some("Unregistered") | Some("Consumer")) => set(doc, "in_gst_category", "Registered"),
                // The GSTIN is the registration: without one the party is unregistered.
                (false, Some("Registered")) => set(doc, "in_gst_category", "Unregistered"),
                _ => {}
            }
        }
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        let category = s(doc, "in_gst_category");
        if let Some(g) = s(doc, "in_gstin") {
            if let Err(m) = validate_gstin(g) {
                errors.push(FieldError::new("in_gstin", m));
            } else if s(doc, "in_state_code").is_some_and(|sc| sc != &g[..2]) {
                errors.push(FieldError::new("in_state_code", "does not match the GSTIN"));
            }
            if category == Some("Overseas") {
                errors.push(FieldError::new("in_gstin", "overseas parties have no GSTIN"));
            }
        } else if category == Some("SEZ") {
            errors.push(FieldError::new("in_gstin", "is required for SEZ units"));
        }
        if let Some(sc) = s(doc, "in_state_code")
            && state_name(sc).is_none()
        {
            errors.push(FieldError::new("in_state_code", format!("'{sc}' is not a GST state code")));
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }
}

fn state_of(doc: &Doc) -> Option<String> {
    s(doc, "in_state_code").map(str::to_string).or_else(|| s(doc, "in_gstin").filter(|g| g.len() >= 2).map(|g| g[..2].to_string()))
}

/// The GST state code of an address in India (its region as a state code or a state's name);
/// none for an address abroad or a region the list does not know.
fn state_of_address(a: &Doc) -> Option<String> {
    if s(a, "country_code").is_some_and(|c| c != "IN") {
        return None;
    }
    let region = s(a, "state")?.trim();
    STATES.iter().find(|(c, n)| *c == region || n.eq_ignore_ascii_case(region)).map(|(c, _)| c.to_string())
}

fn party_of(side: Side) -> (&'static str, &'static str) {
    match side {
        Side::Selling => ("customer", "Customer"),
        Side::Buying => ("supplier", "Supplier"),
    }
}

fn kind(side: Side) -> &'static str {
    match side {
        Side::Selling => "Sales",
        Side::Buying => "Purchase",
    }
}

/// The GST accounts a company in India keeps beside the standard chart: (number, name, parent,
/// root type). The RCM accounts hold the GST it owes on purchases under reverse charge (paid in
/// cash, not from input credit).
const GST_ACCOUNTS: &[(&str, &str, &str, &str)] = &[
    ("1153", "CGST Input", "Tax Assets", "Asset"),
    ("1154", "SGST Input", "Tax Assets", "Asset"),
    ("1155", "IGST Input", "Tax Assets", "Asset"),
    ("2133", "CGST Output", "Duties and Taxes", "Liability"),
    ("2134", "SGST Output", "Duties and Taxes", "Liability"),
    ("2135", "IGST Output", "Duties and Taxes", "Liability"),
    ("2136", "CGST RCM Payable", "Duties and Taxes", "Liability"),
    ("2137", "SGST RCM Payable", "Duties and Taxes", "Liability"),
    ("2138", "IGST RCM Payable", "Duties and Taxes", "Liability"),
];

/// Whether a company is one India's rules are for: its country is India (by its ISO code), or it
/// names no country (before countries were set, every company of a workspace with India was
/// Indian). Without a code (a platform from before codes, a name the code list does not know) the
/// name decides.
fn indian(company: &Doc) -> bool {
    match s(company, "country_code") {
        Some(code) => code == "IN",
        None => s(company, "country").is_none_or(|c| matches!(c.trim().to_ascii_lowercase().as_str(), "" | "in" | "ind" | "india" | "bharat")),
    }
}

/// India's rules for ERP (its localization): the GST template for the place of supply on sales
/// and purchase documents, HSN/SAC codes on outward supplies, and a new company's GST accounts and
/// templates.
pub struct India;

#[async_trait]
impl Localization for India {
    fn package(&self) -> &'static str {
        crate::PACKAGE
    }

    async fn determine(&self, k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()> {
        if doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 {
            return Ok(());
        }
        // A line's HSN/SAC is its item's; a line without an item keeps the one entered on it.
        if ent.field("items").and_then(|f| f.of.as_deref()).and_then(|c| ctx.schema.entity(c)).is_some_and(|c| c.field("in_hsn_code").is_some()) {
            let mut items = rows(doc, "items");
            for r in items.iter_mut() {
                if let Some(item) = id(r, "item") {
                    let it = get(k, ctx, tx, "Item", item).await?;
                    set(r, "in_hsn_code", it.get("hsn_code").cloned().unwrap_or(Value::Null));
                }
            }
            set(doc, "items", Value::Array(items.into_iter().map(Value::Object).collect()));
        }
        let (Some(cid), Some(pid)) = (id(doc, "company"), id(doc, party_of(side).0)) else { return Ok(()) };
        let company = get(k, ctx, tx, "Company", cid).await?;
        if !indian(&company) {
            return Ok(());
        }
        let Some(home) = state_of(&company) else { return Ok(()) };
        let party = get(k, ctx, tx, party_of(side).1, pid).await?;
        let category = s(&party, "in_gst_category").unwrap_or("Unregistered");
        // A purchase under reverse charge (from a supplier marked so: a GTA, an advocate, notified
        // goods, services from abroad; or an invoice marked so) takes the company's reverse-charge
        // templates; others keep whichever of the supply type is chosen.
        let rcm = side == Side::Buying && (b(&party, "in_reverse_charge") || b(doc, "in_reverse_charge"));
        if rcm && ent.field("in_reverse_charge").is_some() {
            set(doc, "in_reverse_charge", true);
        }
        let has_pos = ent.field("in_place_of_supply").is_some();
        let given = if has_pos { normalize(doc, "in_place_of_supply") } else { None };
        // Goods sold go where the document says they are delivered (its ship-to, or its delivery
        // region without one), read through ERP's seam like every country's rules; a party's
        // billing address does not move the place of supply.
        let delivered = match side {
            Side::Selling if ["ship_to", "delivery_country", "delivery_region"].iter().any(|f| doc.get(*f).is_some_and(|v| !v.is_null())) => {
                ship_to(k, ctx, tx, doc).await?.as_ref().and_then(state_of_address)
            }
            _ => None,
        };
        let pos = match delivered.or(given) {
            Some(p) => p,
            // Services from abroad are supplied where the company is.
            None if category == "Overseas" && rcm => home.clone(),
            None if category == "Overseas" => FOREIGN.to_string(),
            None => state_of(&party).unwrap_or_else(|| home.clone()),
        };
        if pos != FOREIGN && state_name(&pos).is_none() {
            return Err(Error::field("in_place_of_supply", format!("'{pos}' is not a GST state code")));
        }
        if has_pos {
            set(doc, "in_place_of_supply", pos.clone());
        }
        let supply = match (side, category) {
            (Side::Selling, "Overseas") => "Export",
            // An import of services: IGST, which the company pays.
            (Side::Buying, "Overseas") if rcm => "Inter-State",
            // Imports of goods carry no GST on the supplier's invoice (it is paid at customs).
            (Side::Buying, "Overseas") => return Ok(()),
            (_, "SEZ") => "Inter-State",
            _ if pos == home => "Intra-State",
            _ => "Inter-State",
        };
        if let Some(current) = id(doc, "tax_template") {
            let t = get(k, ctx, tx, "TaxTemplate", current).await?;
            if s(&t, "in_supply_type").is_none_or(|x| x == supply) && (!rcm || b(&t, "in_reverse_charge")) {
                return Ok(());
            }
        }
        let sctx = sys(ctx);
        let candidates = k
            .find_tx(
                &sctx,
                tx,
                "TaxTemplate",
                &[Filter::eq("company", cid.to_string()), Filter::eq("kind", kind(side)), Filter::eq("in_supply_type", supply)],
                None,
                50,
            )
            .await?;
        let candidates: Vec<&Doc> = candidates.iter().filter(|t| b(t, "in_reverse_charge") == rcm).collect();
        let chosen = candidates.iter().find(|t| b(t, "is_default")).or(candidates.first()).copied();
        match chosen {
            Some(t) => {
                set(doc, "tax_template", s(t, "id").unwrap_or_default().to_string());
                doc.remove("taxes");
            }
            // A reverse-charge supplier's purchase never takes the ordinary GST templates.
            None if rcm => {
                return Err(Error::field(
                    "tax_template",
                    format!("this supplier's supplies are under reverse charge: create an {supply} purchase template marked reverse charge"),
                ));
            }
            None if id(doc, "tax_template").is_some() => {
                return Err(Error::field(
                    "tax_template",
                    format!("this is an {supply} supply but the template is for another supply type; create a {supply} template"),
                ));
            }
            None => {}
        }
        Ok(())
    }

    async fn check(&self, hc: &mut HookCtx<'_>, doc: &Doc, side: Side) -> Result<()> {
        // Outward supplies must carry HSN/SAC codes once the company is GST-registered.
        let (Side::Selling, Some(cid)) = (side, id(doc, "company")) else { return Ok(()) };
        let company = get(hc.kernel, hc.ctx, hc.tx, "Company", cid).await?;
        if !indian(&company) || s(&company, "in_gstin").is_none() {
            return Ok(());
        }
        let lines_carry =
            hc.entity.field("items").and_then(|f| f.of.as_deref()).and_then(|c| hc.ctx.schema.entity(c)).is_some_and(|c| c.field("in_hsn_code").is_some());
        let mut missing = Vec::new();
        let mut lines = Vec::new();
        for (i, r) in rows(doc, "items").iter().enumerate() {
            let Some(item) = id(r, "item") else {
                // A line without an item carries its own code (a SAC code for a service).
                if lines_carry && s(r, "in_hsn_code").is_none_or(|c| c.trim().is_empty()) {
                    lines.push(format!("line {} ({})", i + 1, s(r, "description").unwrap_or("no description").trim()));
                }
                continue;
            };
            let it = get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?;
            if s(&it, "hsn_code").is_none() {
                missing.push(s(&it, "item_code").unwrap_or("?").to_string());
            }
        }
        if !missing.is_empty() {
            missing.sort();
            missing.dedup();
            return Err(Error::invalid(format!("set the HSN/SAC code on these items before invoicing: {}", missing.join(", "))));
        }
        if !lines.is_empty() {
            return Err(Error::invalid(format!("give these lines without an item their HSN/SAC code before invoicing: {}", lines.join(", "))));
        }
        Ok(())
    }

    async fn setup_company(&self, hc: &mut HookCtx<'_>, company: Uuid, accounts: &HashMap<&str, String>) -> Result<()> {
        let sctx = sys(hc.ctx);
        let co = hc.kernel.get_tx(&sctx, hc.tx, "Company", company, false).await?;
        if !indian(&co) {
            return Ok(());
        }
        let mut ids: HashMap<&str, String> = HashMap::new();
        for (number, name, parent, root) in GST_ACCOUNTS {
            let parent_id = accounts.get(parent).ok_or_else(|| Error::internal(format!("the standard chart of accounts has no '{parent}' for {name}")))?;
            let mut a = Doc::new();
            a.insert("account_name".into(), json!(name));
            a.insert("account_number".into(), json!(number));
            a.insert("company".into(), json!(company.to_string()));
            a.insert("is_group".into(), json!(false));
            a.insert("root_type".into(), json!(root));
            a.insert("account_currency".into(), co.get("default_currency").cloned().unwrap_or(Value::Null));
            a.insert("account_type".into(), json!("Tax"));
            a.insert("parent_account".into(), json!(parent_id));
            let created = hc.kernel.insert_tx(&sctx, hc.tx, "Account", a).await?;
            ids.insert(name, s(&created, "id").unwrap_or_default().to_string());
        }
        let row = |acc: &str, desc: &str, rate: &str| json!({"charge_type": "On Net Total", "account": ids.get(acc), "description": desc, "rate": rate});
        // Input tax the company claims, owed in cash on the RCM account.
        let rcm = |acc: &str, owed: &str, desc: &str, rate: &str| {
            json!({"charge_type": "On Net Total", "account": ids.get(acc), "description": desc, "rate": rate, "reverse_charge": true,
                   "reverse_charge_account": ids.get(owed), "category": "Reverse Charge"})
        };
        let templates = [
            ("GST 18% Intra-State (Sales)", "Sales", "Intra-State", true, false, vec![row("CGST Output", "CGST 9%", "9"), row("SGST Output", "SGST 9%", "9")]),
            ("GST 18% Inter-State (Sales)", "Sales", "Inter-State", false, false, vec![row("IGST Output", "IGST 18%", "18")]),
            ("GST Export (Sales)", "Sales", "Export", false, false, vec![row("IGST Output", "IGST 0% (LUT)", "0")]),
            (
                "GST 18% Intra-State (Purchase)",
                "Purchase",
                "Intra-State",
                true,
                false,
                vec![row("CGST Input", "CGST 9%", "9"), row("SGST Input", "SGST 9%", "9")],
            ),
            ("GST 18% Inter-State (Purchase)", "Purchase", "Inter-State", false, false, vec![row("IGST Input", "IGST 18%", "18")]),
            (
                "GST 18% Intra-State RCM (Purchase)",
                "Purchase",
                "Intra-State",
                false,
                true,
                vec![rcm("CGST Input", "CGST RCM Payable", "CGST 9% (RCM)", "9"), rcm("SGST Input", "SGST RCM Payable", "SGST 9% (RCM)", "9")],
            ),
            ("GST 18% Inter-State RCM (Purchase)", "Purchase", "Inter-State", false, true, vec![rcm("IGST Input", "IGST RCM Payable", "IGST 18% (RCM)", "18")]),
        ];
        for (title, kind, supply, default, reverse_charge, rows_) in templates {
            let mut t = Doc::new();
            t.insert("title".into(), json!(title));
            t.insert("company".into(), json!(company.to_string()));
            t.insert("kind".into(), json!(kind));
            // The RCM templates are chosen by the supplier's flag, never as a plain default.
            t.insert("is_default".into(), json!(default));
            t.insert("in_supply_type".into(), json!(supply));
            t.insert("in_reverse_charge".into(), json!(reverse_charge));
            t.insert("rows".into(), Value::Array(rows_));
            hc.kernel.insert_tx(&sctx, hc.tx, "TaxTemplate", t).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksum_of_known_gstins() {
        assert_eq!(gstin_check_char("27AAPFU0939F1Z"), Some('V'));
        assert!(validate_gstin("27AAPFU0939F1ZV").is_ok());
        assert!(validate_gstin("27AAPFU0939F1ZX").is_err());
        assert!(validate_gstin("00AAPFU0939F1ZV").is_err());
        assert!(validate_gstin("27AAPFU0939F1Z").is_err());
    }

    #[test]
    fn every_generated_check_char_validates() {
        for base in ["29ABCDE1234F1Z", "07AAACR5055K1Z", "33AAACI1195H1Z"] {
            let c = gstin_check_char(base).unwrap();
            assert!(validate_gstin(&format!("{base}{c}")).is_ok(), "{base}{c}");
        }
    }
}
