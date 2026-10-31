//! XRechnung 3.0: the EN 16931 invoice in UBL with Germany's CIUS (KoSIT), whose rules (BR-DE-*,
//! from KoSIT's schematron) it is checked against before it is written: payment instructions, the
//! seller's contact, both parties' city and post code, a buyer reference (a public buyer's
//! Leitweg-ID), an IBAN that holds, and both electronic addresses.

use crate::util::{get, id, s};
use async_trait::async_trait;
use ferrum_eu::ubl::{Invoice, invoice, to_xml};
use ferrum_eu::vat::valid_iban;
use ferrum_kernel::{Doc, DocHook, Error, HookCtx, Result};
use serde_json::{Value, json};

pub const XRECHNUNG_3: &str = "urn:cen.eu:en16931:2017#compliant#urn:xeinkauf.de:kosit:xrechnung_3.0";

/// BR-DE-28's address form: one @, no spaces, a dot in the domain, none at its ends.
fn email_ok(e: &str) -> bool {
    let Some((local, domain)) = e.split_once('@') else { return false };
    !local.is_empty()
        && !domain.contains('@')
        && !e.chars().any(char::is_whitespace)
        && domain.split('.').count() >= 2
        && domain.split('.').all(|p| !p.is_empty())
}

/// KoSIT's German rules on an EN 16931 invoice: the problems, empty when it complies.
pub fn validate(inv: &Invoice) -> Vec<String> {
    let mut p = Vec::new();
    let mut rule = |ok: bool, id: &str, what: &str| {
        if !ok {
            p.push(format!("{id}: {what}"));
        }
    };
    rule(inv.payment.is_some(), "BR-DE-1", "the invoice says how to pay (the company's IBAN)");
    match &inv.seller.contact {
        Some((name, phone, email)) => {
            rule(!name.trim().is_empty(), "BR-DE-5", "the seller's contact has a name");
            rule(!phone.trim().is_empty(), "BR-DE-6", "the seller's contact has a telephone number");
            rule(!email.trim().is_empty(), "BR-DE-7", "the seller's contact has an email address");
            rule(phone.chars().filter(char::is_ascii_digit).count() >= 3, "BR-DE-27", "the seller's telephone number has at least three digits");
            rule(email_ok(email), "BR-DE-28", "the seller's email address has the form name@domain.tld");
        }
        None => rule(false, "BR-DE-2", "the seller's contact (the company's invoice contact, telephone and email)"),
    }
    rule(inv.seller.city.is_some(), "BR-DE-3", "the seller's city");
    rule(inv.seller.postal.is_some(), "BR-DE-4", "the seller's post code");
    rule(inv.buyer.city.is_some(), "BR-DE-8", "the buyer's city (its billing address)");
    rule(inv.buyer.postal.is_some(), "BR-DE-9", "the buyer's post code (its billing address)");
    rule(inv.subtotals.iter().all(|t| t.category != "O"), "BR-DE-14", "every VAT breakdown has a rate");
    rule(!inv.buyer_reference.trim().is_empty(), "BR-DE-15", "a buyer reference (a public buyer's Leitweg-ID, or the order)");
    rule(inv.customization.as_deref() == Some(XRECHNUNG_3), "BR-DE-21", "XRechnung's specification identifier");
    if let Some(pay) = &inv.payment
        && matches!(pay.code.as_str(), "30" | "58")
    {
        rule(pay.iban.is_some(), "BR-DE-23-a", "a credit transfer names the account (IBAN)");
        if pay.code == "58" {
            rule(pay.iban.as_deref().is_some_and(valid_iban), "BR-DE-19", "the IBAN holds (its check digits)");
        }
    }
    rule(inv.seller.endpoint.is_some(), "PEPPOL-EN16931-R020", "the seller's electronic address (its Peppol ID or invoice contact email)");
    rule(inv.buyer.endpoint.is_some(), "PEPPOL-EN16931-R010", "the buyer's electronic address (its Peppol ID, Leitweg-ID or email)");
    p
}

/// A submitted invoice as XRechnung: the EU's EN 16931 invoice with Germany's profile.
pub async fn xrechnung(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Invoice> {
    let mut inv = invoice(hc, doc).await?;
    let ctx = crate::util::sys(hc.ctx);
    let customer = get(hc.kernel, &ctx, hc.tx, "Customer", id(doc, "customer").ok_or_else(|| Error::internal("invoice without customer"))?).await?;
    let company = get(hc.kernel, &ctx, hc.tx, "Company", id(doc, "company").ok_or_else(|| Error::internal("invoice without company"))?).await?;
    inv.customization = Some(XRECHNUNG_3.into());
    let leitweg = s(&customer, "de_leitweg_id").map(str::trim).filter(|l| !l.is_empty());
    if let Some(l) = leitweg {
        inv.buyer_reference = l.to_string();
    }
    // Electronic addresses: Peppol IDs, else a public buyer's Leitweg-ID (scheme 0204) or email.
    if inv.buyer.endpoint.is_none() {
        inv.buyer.endpoint = leitweg.map(|l| ("0204".to_string(), l.to_string())).or_else(|| s(&customer, "email").map(|e| ("EM".to_string(), e.to_string())));
    }
    if inv.seller.endpoint.is_none() {
        inv.seller.endpoint = s(&company, "eu_contact_email").map(|e| ("EM".to_string(), e.to_string()));
    }
    let problems = validate(&inv);
    if !problems.is_empty() {
        return Err(Error::invalid(format!("the XRechnung would not pass KoSIT's rules: {}", problems.join("; "))));
    }
    Ok(inv)
}

/// A sales invoice's Download XRechnung.
pub struct XRechnungHook;

#[async_trait]
impl DocHook for XRechnungHook {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "de_xrechnung" {
            return Ok(None);
        }
        let inv = xrechnung(hc, doc).await?;
        let number = s(doc, "number").unwrap_or("invoice").replace(['/', '\\'], "-");
        Ok(Some(json!({"file": {"name": format!("{number}-xrechnung.xml"), "type": "application/xml", "content": to_xml(&inv)}})))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn email_addresses_take_br_de_28s_form() {
        for good in ["rechnung@muster.de", "a@b.cc", "first.last@sub.example.org"] {
            assert!(email_ok(good), "{good}");
        }
        for bad in ["muster.de", "a@b", "a @b.de", "a@b@c.de", "a@.de", "a@b."] {
            assert!(!email_ok(bad), "{bad}");
        }
    }
}
