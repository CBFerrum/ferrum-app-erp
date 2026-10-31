//! Germany's VAT rules on the EU's, for a company in Germany: a small business (§ 19 UStG)
//! charges no VAT on what it sells at home, and says why; a business customer in Germany owes the
//! VAT on what § 13b UStG lists (construction work, scrap metal, mobile phones and the like, as
//! the item says), and the invoice says so (Article 226(11a) of Directive 2006/112/EC).

use crate::util::{b, get, id, rows, s, set};
use async_trait::async_trait;
use ferrum_app_erp::localization::Localization;
use ferrum_app_erp::pricing::Side;
use ferrum_eu::vat::{OUTPUT_TAX, Plan, Treatment, account, kind_of, plan};
use ferrum_kernel::{Ctx, Doc, Kernel, Result, Tx};
use ferrum_meta::EffEntity;
use serde_json::{Value, json};

/// The exemption reason a small business's lines carry (EN 16931 category E).
pub const SMALL_BUSINESS: &str = "Kleinunternehmerregelung (§ 19 UStG)";
pub const SMALL_BUSINESS_NOTE: &str = "Gemäß § 19 UStG wird keine Umsatzsteuer berechnet.";
pub const REVERSE_CHARGE_NOTE: &str = "Steuerschuldnerschaft des Leistungsempfängers (§ 13b UStG).";

/// A sale in Germany to a business there.
fn domestic_business(plan: &Plan, party: Option<&Doc>) -> bool {
    plan.other.as_deref().is_none_or(|o| o == "DE")
        && party.is_some_and(|p| s(p, "eu_vat_number").is_some() || s(p, "tax_id").is_some() || s(p, "customer_type") == Some("Company"))
}

/// How a line taxed at home in Germany is taxed instead: (category, exemption reason).
fn german(small: bool, b2b: bool, item: &Doc) -> Option<(&'static str, &'static str)> {
    if small {
        Some(("Exempt", SMALL_BUSINESS))
    } else if b2b && b(item, "de_reverse_charge") {
        Some(("Reverse Charge", "VATEX-EU-AE"))
    } else {
        None
    }
}

pub struct Germany;

#[async_trait]
impl Localization for Germany {
    fn package(&self) -> &'static str {
        crate::PACKAGE
    }

    async fn determine(&self, k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()> {
        if side != Side::Selling || doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 || ent.field("eu_legal_text").is_none() {
            return Ok(());
        }
        let Some((plan, company, party)) = plan(k, ctx, tx, doc, side).await? else { return Ok(()) };
        if plan.home != "DE" {
            return Ok(());
        }
        let mut notes: Vec<&str> = Vec::new();
        if b(&company, "de_small_business") {
            notes.push(SMALL_BUSINESS_NOTE);
        } else if domestic_business(&plan, party.as_ref()) {
            for r in rows(doc, "items") {
                let Some(i) = id(&r, "item") else { continue };
                if b(&get(k, ctx, tx, "Item", i).await?, "de_reverse_charge") {
                    notes.push(REVERSE_CHARGE_NOTE);
                    break;
                }
            }
        }
        if notes.is_empty() {
            return Ok(());
        }
        // After the EU's notes, which it wrote just before.
        let mut text = s(doc, "eu_legal_text").unwrap_or_default().to_string();
        for n in notes {
            if !text.contains(n) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(n);
            }
        }
        set(doc, "eu_legal_text", text);
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
        if side != Side::Selling {
            return Ok(());
        }
        let Some((plan, company, party)) = plan(k, ctx, tx, doc, side).await? else { return Ok(()) };
        let Some(cid) = id(&company, "id").filter(|_| plan.home == "DE") else { return Ok(()) };
        let Ok(output) = account(k, ctx, tx, cid, OUTPUT_TAX).await else { return Ok(()) };
        if !taxes.iter().any(|t| s(t, "account") == Some(output.as_str())) {
            return Ok(());
        }
        let (small, b2b) = (b(&company, "de_small_business"), domestic_business(&plan, party.as_ref()));
        for (i, (line, item)) in lines.iter().zip(items).enumerate() {
            if id(line, "item").is_none() || !matches!(plan.treat(kind_of(item)), Treatment::Taxed { ref country, oss: false } if country == "DE") {
                continue;
            }
            let Some((category, reason)) = german(small, b2b, item) else { continue };
            let Value::Object(entry) = json!({"tax_account": output, "rate": "0", "category": category, "exemption_reason": reason}) else { continue };
            rates[i].retain(|r| s(r, "tax_account") != Some(output.as_str()));
            rates[i].push(entry);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_businesses_and_the_domestic_reverse_charge() {
        let flagged = json!({"de_reverse_charge": true}).as_object().cloned().unwrap();
        let plain = Doc::new();
        assert_eq!(german(true, true, &flagged), Some(("Exempt", SMALL_BUSINESS)), "a small business charges no VAT at all");
        assert_eq!(german(false, true, &flagged), Some(("Reverse Charge", "VATEX-EU-AE")));
        assert_eq!(german(false, false, &flagged), None, "consumers pay the VAT");
        assert_eq!(german(false, true, &plain), None);
        let home = Plan { home: "DE".into(), side: Side::Selling, other: Some("DE".into()), business: true, oss: false };
        let firm = json!({"eu_vat_number": "DE987654321"}).as_object().cloned().unwrap();
        assert!(domestic_business(&home, Some(&firm)));
        assert!(!domestic_business(&Plan { other: Some("FR".into()), ..home.clone() }, Some(&firm)), "abroad the EU's rules hold");
        assert!(!domestic_business(&home, Some(&Doc::new())));
    }
}
