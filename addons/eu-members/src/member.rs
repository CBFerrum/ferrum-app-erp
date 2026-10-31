//! What member states add to the EU's VAT on a company's sales at home: small businesses that
//! charge no VAT under a national exemption, and the domestic reverse charge on what a member
//! state lists (construction, scrap and the like) to business customers there; each says so on
//! the invoice, in the member state's words (Germany's add-on does the same for § 19 and § 13b
//! UStG).

use crate::util::{b, get, id, rows, s, set};
use async_trait::async_trait;
use ferrum_app_erp::localization::Localization;
use ferrum_app_erp::pricing::Side;
use ferrum_eu::vat::{OUTPUT_TAX, Plan, Treatment, account, kind_of, plan};
use ferrum_kernel::{Ctx, Doc, Kernel, Result, Tx};
use ferrum_meta::EffEntity;
use serde_json::{Value, json};

#[derive(Debug)]
pub struct Member {
    pub package: &'static str,
    pub country: &'static str,
    /// The company's check field for the small business exemption, the reason its lines carry
    /// (EN 16931 category E) and the note on its invoices.
    pub small: Option<(&'static str, &'static str, &'static str)>,
    /// The item's check field for the domestic reverse charge, and the invoice's note.
    pub reverse: Option<(&'static str, &'static str)>,
}

/// A sale at home to a business there.
pub fn domestic_business(home: &str, plan: &Plan, party: Option<&Doc>) -> bool {
    plan.other.as_deref().is_none_or(|o| o == home)
        && party.is_some_and(|p| s(p, "eu_vat_number").is_some() || s(p, "tax_id").is_some() || s(p, "customer_type") == Some("Company"))
}

impl Member {
    /// How a line taxed at home is taxed instead: (category, exemption reason).
    pub fn treat(&self, small: bool, b2b: bool, item: &Doc) -> Option<(&'static str, &'static str)> {
        if let (true, Some((_, reason, _))) = (small, self.small) {
            return Some(("Exempt", reason));
        }
        match self.reverse {
            Some((field, _)) if b2b && b(item, field) => Some(("Reverse Charge", "VATEX-EU-AE")),
            _ => None,
        }
    }

    fn is_small(&self, company: &Doc) -> bool {
        self.small.is_some_and(|(field, _, _)| b(company, field))
    }
}

pub struct Rules(pub &'static Member);

#[async_trait]
impl Localization for Rules {
    fn package(&self) -> &'static str {
        self.0.package
    }

    async fn determine(&self, k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()> {
        let m = self.0;
        if side != Side::Selling || doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 || ent.field("eu_legal_text").is_none() {
            return Ok(());
        }
        let Some((plan, company, party)) = plan(k, ctx, tx, doc, side).await? else { return Ok(()) };
        if plan.home != m.country {
            return Ok(());
        }
        let mut notes: Vec<&str> = Vec::new();
        if let (true, Some((_, _, note))) = (m.is_small(&company), m.small) {
            notes.push(note);
        } else if let Some((field, note)) = m.reverse
            && domestic_business(m.country, &plan, party.as_ref())
        {
            for r in rows(doc, "items") {
                let Some(i) = id(&r, "item") else { continue };
                if b(&get(k, ctx, tx, "Item", i).await?, field) {
                    notes.push(note);
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
        let m = self.0;
        if side != Side::Selling {
            return Ok(());
        }
        let Some((plan, company, party)) = plan(k, ctx, tx, doc, side).await? else { return Ok(()) };
        let Some(cid) = id(&company, "id").filter(|_| plan.home == m.country) else { return Ok(()) };
        let Ok(output) = account(k, ctx, tx, cid, OUTPUT_TAX).await else { return Ok(()) };
        if !taxes.iter().any(|t| s(t, "account") == Some(output.as_str())) {
            return Ok(());
        }
        let (small, b2b) = (m.is_small(&company), domestic_business(m.country, &plan, party.as_ref()));
        for (i, (line, item)) in lines.iter().zip(items).enumerate() {
            if id(line, "item").is_none() || !matches!(plan.treat(kind_of(item)), Treatment::Taxed { ref country, oss: false } if country == m.country) {
                continue;
            }
            let Some((category, reason)) = m.treat(small, b2b, item) else { continue };
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

    static M: Member = Member { package: "xx", country: "BE", small: Some(("xx_small", "Franchise", "No VAT")), reverse: Some(("xx_rc", "Reverse charge")) };

    #[test]
    fn small_businesses_and_the_domestic_reverse_charge() {
        let flagged = json!({"xx_rc": true}).as_object().cloned().unwrap();
        assert_eq!(M.treat(true, true, &flagged), Some(("Exempt", "Franchise")));
        assert_eq!(M.treat(false, true, &flagged), Some(("Reverse Charge", "VATEX-EU-AE")));
        assert_eq!((M.treat(false, false, &flagged), M.treat(false, true, &Doc::new())), (None, None));
        let home = Plan { home: "BE".into(), side: Side::Selling, other: Some("BE".into()), business: true, oss: false };
        let firm = json!({"eu_vat_number": "BE0308357159"}).as_object().cloned().unwrap();
        assert!(domestic_business("BE", &home, Some(&firm)));
        assert!(!domestic_business("BE", &Plan { other: Some("NL".into()), ..home }, Some(&firm)));
    }
}
