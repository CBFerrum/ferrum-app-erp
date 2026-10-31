//! Belgium: on the EU's VAT (21%, 12%, 6% from TEDB), invoices between Belgian businesses are
//! structured e-invoices through Peppol since 1 January 2026, so a submitted invoice to a Belgian
//! VAT-registered customer goes to the company's access point (the EU's Peppol hand-over) and one
//! without both parties' Peppol IDs is refused. Enterprise numbers (KBO/BCE) are checked (their
//! last two digits are 97 less the first eight modulo 97) and give the Peppol ID (scheme 0208)
//! when none is set. Small businesses under the franchise (article 56bis of the VAT Code) charge
//! no VAT; construction work for a registered contractor is reverse-charged (article 20, Royal
//! Decree no. 1), each with its note in French and Dutch.

use crate::member::{Member, Rules};
use crate::util::{b, get, id, s, set, sys};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result};
use serde_json::Value;

pub static MEMBER: Member = Member {
    package: "be",
    country: "BE",
    small: Some((
        "be_small_business",
        "Régime de franchise des petites entreprises (article 56bis du Code de la TVA)",
        "Régime particulier de franchise des petites entreprises – TVA non applicable. Bijzondere vrijstellingsregeling kleine ondernemingen – btw niet van toepassing.",
    )),
    reverse: Some((
        "be_reverse_charge",
        "Autoliquidation – article 51, § 2, alinéa 1er, 5° du Code de la TVA. Verlegging van heffing – artikel 51, § 2, eerste lid, 5° van het btw-wetboek.",
    )),
};

/// An enterprise number (KBO/BCE): ten digits from 0 or 1 (nine of old, a leading zero added),
/// the last two 97 less the first eight modulo 97; as 0123.456.789.
pub fn enterprise_number(raw: &str) -> Option<String> {
    let mut n: String = raw.trim().trim_start_matches(['B', 'E', 'b', 'e']).chars().filter(char::is_ascii_digit).collect();
    if raw.chars().filter(|c| c.is_ascii_alphabetic()).count() > 2 {
        return None;
    }
    if n.len() == 9 {
        n.insert(0, '0');
    }
    if n.len() != 10 || !n.starts_with(['0', '1']) {
        return None;
    }
    let head: u64 = n[..8].parse().ok()?;
    let check: u64 = n[8..].parse().ok()?;
    (97 - head % 97 == check).then(|| format!("{}.{}.{}", &n[..4], &n[4..7], &n[7..]))
}

/// The enterprise number's digits, as Peppol's scheme 0208 takes them.
pub fn peppol_id(number: &str) -> String {
    format!("0208:{}", number.replace('.', ""))
}

/// Enterprise numbers checked, taken from a Belgian VAT number when not given, and Peppol IDs
/// from them; Belgian business invoices sent through Peppol.
pub struct BelgiumHook;

#[async_trait]
impl DocHook for BelgiumHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.field("be_enterprise_number").is_none() {
            return Ok(());
        }
        let given = s(doc, "be_enterprise_number").map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
        let from_vat = s(doc, "eu_vat_number").filter(|v| v.to_uppercase().starts_with("BE")).and_then(enterprise_number);
        let number = match given {
            Some(g) => Some(enterprise_number(&g).ok_or_else(|| {
                Error::Validation(vec![FieldError::new("be_enterprise_number", "is not an enterprise number (ten digits from 0 or 1 whose check digits hold)")])
            })?),
            None => from_vat,
        };
        if let Some(n) = &number {
            set(doc, "be_enterprise_number", n.as_str());
            if s(doc, "eu_peppol_id").is_none() && hc.entity.field("eu_peppol_id").is_some() {
                set(doc, "eu_peppol_id", peppol_id(n));
            }
        }
        Ok(())
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.def.name != "SalesInvoice" || !peppol_due(hc, doc).await? {
            return Ok(());
        }
        let ctx = sys(hc.ctx);
        let company = get(hc.kernel, &ctx, hc.tx, "Company", id(doc, "company").ok_or_else(|| Error::internal("invoice without company"))?).await?;
        let customer = get(hc.kernel, &ctx, hc.tx, "Customer", id(doc, "customer").ok_or_else(|| Error::internal("invoice without customer"))?).await?;
        if s(&company, "eu_peppol_id").is_none() || s(&customer, "eu_peppol_id").is_none() {
            return Err(Error::invalid(
                "Belgian businesses exchange invoices through Peppol: set the company's and the customer's Peppol IDs (their enterprise numbers give 0208 IDs)",
            ));
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.entity.def.name == "SalesInvoice" && peppol_due(hc, doc).await? {
            ferrum_eu::ubl::send_peppol(hc, doc, None).await?;
        }
        Ok(())
    }
}

/// A Belgian company's invoice to a Belgian VAT-registered customer, which it sends on submit.
async fn peppol_due(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<bool> {
    let (Some(cid), Some(cust)) = (id(doc, "company"), id(doc, "customer")) else { return Ok(false) };
    let ctx = sys(hc.ctx);
    let company = get(hc.kernel, &ctx, hc.tx, "Company", cid).await?;
    if s(&company, "country_code") != Some("BE") || !b(&company, "be_peppol_on_submit") {
        return Ok(false);
    }
    let customer = get(hc.kernel, &ctx, hc.tx, "Customer", cust).await?;
    let vat = s(doc, "eu_customer_vat").or(s(&customer, "eu_vat_number")).unwrap_or_default().to_uppercase();
    Ok(vat.starts_with("BE") && !matches!(s(doc, "eu_peppol_status"), Some("Sent" | "Pending")) && doc.get("docstatus").and_then(Value::as_i64) != Some(2))
}

pub fn rules() -> Rules {
    Rules(&MEMBER)
}

fn hooks(h: &mut ferrum_kernel::HookRegistry) {
    for entity in ["Company", "Customer", "Supplier", "SalesInvoice"] {
        h.register(entity, std::sync::Arc::new(BelgiumHook));
    }
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

crate::app!("be");

#[cfg(test)]
mod tests {
    #[test]
    fn enterprise_numbers() {
        assert_eq!(super::enterprise_number("0308.357.159"), Some("0308.357.159".into()), "einvoice.belgium.be's example");
        assert_eq!(super::enterprise_number("BE 0308357159"), Some("0308.357.159".into()));
        assert_eq!(super::enterprise_number("308357159"), Some("0308.357.159".into()), "nine digits of old");
        assert_eq!((super::enterprise_number("0308357158"), super::enterprise_number("2308357159")), (None, None));
        assert_eq!(super::peppol_id("0308.357.159"), "0208:0308357159");
    }
}
