//! France: on the EU's VAT (20%, 10%, 5.5%, 2.1% from TEDB), the e-invoicing reform (receiving
//! for every business and issuing for large and mid-sized ones since 1 September 2026, issuing for
//! the rest from 1 September 2027): invoices between French VAT-registered businesses are
//! e-invoices through the company's plateforme agréée (PA), and sales that are not (to consumers,
//! abroad) are e-reported. A submitted invoice to a French business is queued for the company's PA
//! through the outbox as EN 16931 UBL with both parties' SIREN (scheme 0002), and the PA's
//! lifecycle statuses are recorded; the e-reporting report sums the rest by day and rate. The
//! invoice carries France's mentions: the category of the operation and the option for VAT on
//! debits (from the reform), and, between businesses, the late-payment penalties and the €40
//! recovery indemnity (Code de commerce L441-10, D441-5). SIREN and SIRET numbers are checked
//! (Luhn), and French VAT numbers' key against the SIREN. Small businesses under the franchise en
//! base charge no VAT (article 293 B); subcontracting in construction is reverse-charged (283-2
//! nonies).

use crate::member::{Member, Rules, domestic_business};
use crate::util::{b, get, id, rows, s, set, sys};
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_app_erp::localization::Localization;
use ferrum_app_erp::pricing::Side;
use ferrum_domain::reports::{Params, ReportSource, col, param_def, period, read_tx, resolve_company};
use ferrum_eu::ubl::{invoice, to_xml};
use ferrum_eu::vat::{Kind, OUTPUT_TAX, kind_of, plan};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, Result, Tx, outbox};
use ferrum_meta::EffEntity;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

pub const SEND_TOPIC: &str = "fr.pa.send_requested";

pub static MEMBER: Member = Member {
    package: "fr",
    country: "FR",
    small: Some(("fr_franchise", "TVA non applicable, art. 293 B du CGI", "TVA non applicable, art. 293 B du CGI.")),
    reverse: Some(("fr_reverse_charge", "Autoliquidation : TVA due par le preneur (article 283-2 nonies du CGI).")),
};

/// Luhn's check on a number's digits.
fn luhn(n: &str) -> bool {
    let sum: u32 = n
        .bytes()
        .rev()
        .enumerate()
        .map(|(i, c)| {
            let d = u32::from(c - b'0');
            if i % 2 == 1 {
                let x = d * 2;
                if x > 9 { x - 9 } else { x }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// A SIREN: nine digits passing Luhn's check.
pub fn siren(raw: &str) -> Option<String> {
    let n: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    (n.len() == 9 && n.bytes().all(|c| c.is_ascii_digit()) && luhn(&n)).then_some(n)
}

/// A SIRET: fourteen digits passing Luhn's check (La Poste's, SIREN 356000000, by the sum of
/// their digits, a multiple of five).
pub fn siret(raw: &str) -> Option<String> {
    let n: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    if n.len() != 14 || !n.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let ok = if n.starts_with("356000000") { n.bytes().map(|c| u32::from(c - b'0')).sum::<u32>().is_multiple_of(5) } else { luhn(&n) };
    ok.then_some(n)
}

/// A French VAT number's SIREN, when its key (12 + 3 × (SIREN mod 97), mod 97) holds.
pub fn vat_siren(vat: &str) -> Option<String> {
    let v: String = vat.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase();
    let rest = v.strip_prefix("FR")?;
    if rest.len() != 11 {
        return None;
    }
    let (key, number) = rest.split_at(2);
    let number = siren(number)?;
    let n: u64 = number.parse().ok()?;
    match key.parse::<u64>() {
        Ok(k) => (k == (12 + 3 * (n % 97)) % 97).then_some(number),
        // Keys with letters (newer allocations) are not computed from the SIREN.
        Err(_) => Some(number),
    }
}

/// The category of the operation, as the reform's mention says it.
pub fn category(kinds: &[Kind]) -> &'static str {
    let goods = kinds.iter().any(|k| matches!(k, Kind::Goods));
    let services = kinds.iter().any(|k| !matches!(k, Kind::Goods));
    match (goods, services) {
        (true, true) => "Catégorie de l'opération : livraison de biens et prestation de services.",
        (true, false) => "Catégorie de l'opération : livraison de biens.",
        _ => "Catégorie de l'opération : prestation de services.",
    }
}

/// The late-payment mention between businesses.
pub fn penalties(rate: &str) -> String {
    format!(
        "En cas de retard de paiement : pénalités au taux de {rate} et indemnité forfaitaire pour frais de recouvrement de 40 € (articles L441-10 et D441-5 du Code de commerce). Pas d'escompte pour paiement anticipé."
    )
}

pub struct France;

#[async_trait]
impl Localization for France {
    fn package(&self) -> &'static str {
        crate::fr::PACKAGE
    }

    async fn determine(&self, k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()> {
        Rules(&MEMBER).determine(k, ctx, tx, ent, doc, side).await?;
        if side != Side::Selling || doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 || ent.field("eu_legal_text").is_none() {
            return Ok(());
        }
        let Some((plan, company, party)) = plan(k, ctx, tx, doc, side).await? else { return Ok(()) };
        if plan.home != "FR" {
            return Ok(());
        }
        let mut kinds = Vec::new();
        for r in rows(doc, "items") {
            if let Some(i) = id(&r, "item") {
                kinds.push(kind_of(&get(k, ctx, tx, "Item", i).await?));
            }
        }
        let mut notes: Vec<String> = vec![category(&kinds).to_string()];
        if b(&company, "fr_vat_on_debits") {
            notes.push("Option pour le paiement de la taxe d'après les débits.".into());
        }
        let business =
            party.as_ref().is_some_and(|p| s(p, "eu_vat_number").is_some() || s(p, "fr_siren").is_some() || s(p, "customer_type") == Some("Company"));
        if business {
            notes.push(penalties(s(&company, "fr_penalty_rate").unwrap_or("trois fois le taux d'intérêt légal")));
        }
        let mut text = s(doc, "eu_legal_text").unwrap_or_default().to_string();
        for n in notes {
            if !text.contains(&n) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&n);
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
        Rules(&MEMBER).line_rates(k, ctx, tx, doc, lines, items, taxes, rates, side).await
    }
}

/// An invoice from a French company to a French business: an e-invoice through the PA.
async fn e_invoice_due(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Option<Doc>> {
    let Some((plan, company, party)) = plan(hc.kernel, hc.ctx, hc.tx, doc, Side::Selling).await? else { return Ok(None) };
    if plan.home != "FR" || !b(&company, "fr_pa_on_submit") {
        return Ok(None);
    }
    Ok(domestic_business("FR", &plan, party.as_ref()).then_some(company))
}

/// The invoice as EN 16931 UBL with both parties' SIREN.
async fn ubl(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<String> {
    let mut inv = invoice(hc, doc).await?;
    let ctx = sys(hc.ctx);
    let company = get(hc.kernel, &ctx, hc.tx, "Company", id(doc, "company").ok_or_else(|| Error::internal("invoice without company"))?).await?;
    let customer = get(hc.kernel, &ctx, hc.tx, "Customer", id(doc, "customer").ok_or_else(|| Error::internal("invoice without customer"))?).await?;
    let seller = s(&company, "fr_siren").map(str::to_string).or_else(|| s(&company, "eu_vat_number").and_then(vat_siren));
    let buyer = s(&customer, "fr_siren").map(str::to_string).or_else(|| s(&customer, "eu_vat_number").and_then(vat_siren));
    let seller = seller.ok_or_else(|| Error::invalid("the company's SIREN (or its French VAT number) goes on its e-invoices"))?;
    inv.seller.legal_id = Some(("0002".into(), seller));
    inv.buyer.legal_id = buyer.map(|b| ("0002".to_string(), b));
    Ok(to_xml(&inv))
}

/// SIREN, SIRET and French VAT numbers checked; e-invoices queued for the PA; its statuses recorded.
pub struct PaHook;

impl PaHook {
    async fn save(hc: &mut HookCtx<'_>, doc: &Doc, values: Value) -> Result<()> {
        let inv = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let Value::Object(v) = values else { return Ok(()) };
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", inv, v).await
    }
}

/// The PA's lifecycle statuses Ferrum records, and what each makes of the invoice.
pub const STATUSES: &[(&str, &str)] =
    &[("Deposited", "Deposited"), ("Rejected", "Rejected"), ("Refused", "Refused"), ("Disputed", "Disputed"), ("Approved", "Approved"), ("Paid", "Paid")];

#[async_trait]
impl DocHook for PaHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        for (field, check) in [("fr_siren", siren as fn(&str) -> Option<String>), ("fr_siret", siret)] {
            if hc.entity.field(field).is_none() {
                continue;
            }
            let Some(v) = s(doc, field).map(str::to_string).filter(|v| !v.trim().is_empty()) else { continue };
            let n = check(&v).ok_or_else(|| Error::Validation(vec![FieldError::new(field, "is not valid (its Luhn check does not hold)")]))?;
            set(doc, field, n);
        }
        if hc.entity.field("eu_vat_number").is_some()
            && let Some(v) = s(doc, "eu_vat_number").map(str::to_uppercase).filter(|v| v.starts_with("FR"))
        {
            let from_vat = vat_siren(&v)
                .ok_or_else(|| Error::Validation(vec![FieldError::new("eu_vat_number", "is not a French VAT number: FR, its key and a SIREN")]))?;
            if hc.entity.field("fr_siren").is_some() && s(doc, "fr_siren").is_none() {
                set(doc, "fr_siren", from_vat);
            }
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.entity.def.name != "SalesInvoice" || hc.entity.field("fr_pa_status").is_none() || e_invoice_due(hc, doc).await?.is_none() {
            return Ok(());
        }
        send(hc, doc).await.map(|_| ())
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if matches!(s(doc, "fr_pa_status"), Some("Pending" | "Deposited" | "Approved" | "Paid")) {
            return Err(Error::invalid("the PA has this invoice: issue a credit note (avoir) instead of cancelling it"));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if hc.entity.field("fr_pa_status").is_none() || !name.starts_with("fr_") {
            return Ok(None);
        }
        match name {
            "fr_ubl" => {
                let xml = ubl(hc, doc).await?;
                let file = format!("{}.xml", s(doc, "number").unwrap_or("facture").replace(['/', '\\'], "-"));
                Ok(Some(json!({"file": {"name": file, "type": "application/xml", "content": xml}})))
            }
            "fr_send_pa" => send(hc, doc).await.map(Some),
            "fr_record_pa" => {
                let status = params.get("status").and_then(Value::as_str).and_then(|st| STATUSES.iter().find(|(k, _)| *k == st)).map(|(_, v)| *v);
                let status = status.ok_or_else(|| Error::field("status", "is not one of the PA's statuses"))?;
                let message = params.get("message").and_then(Value::as_str).map(|m| m.chars().take(2000).collect::<String>());
                let pa_id = params.get("pa_id").and_then(Value::as_str).map(str::to_string).or(s(doc, "fr_pa_id").map(str::to_string));
                Self::save(hc, doc, json!({"fr_pa_status": status, "fr_pa_id": pa_id, "fr_pa_error": message})).await?;
                Ok(Some(json!({"status": status})))
            }
            _ => Ok(None),
        }
    }
}

async fn send(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
    if matches!(s(doc, "fr_pa_status"), Some("Pending" | "Deposited" | "Approved" | "Paid")) {
        return Err(Error::invalid(format!("the invoice is already {}", s(doc, "fr_pa_status").unwrap_or_default().to_lowercase())));
    }
    let xml = ubl(hc, doc).await?;
    let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
    let event = json!({"invoice": iid, "company": s(doc, "company"), "number": s(doc, "number"), "syntax": "UBL", "xml": xml});
    PaHook::save(hc, doc, json!({"fr_pa_status": "Pending", "fr_pa_error": null})).await?;
    outbox::enqueue(hc.tx, hc.ctx.tenant, SEND_TOPIC, Some("SalesInvoice"), Some(iid), event).await?;
    Ok(json!({"status": "Pending"}))
}

/// E-reporting: the sales that are not e-invoices (to consumers, and abroad), by day and rate.
pub struct EReporting;

#[async_trait]
impl ReportSource for EReporting {
    fn catalog(&self) -> Vec<Value> {
        let params: Vec<Value> = ["company", "from_date", "to_date"].iter().map(|p| param_def(p)).collect();
        vec![
            json!({"name": "fr_e_reporting", "label": "E-reporting (transactions)", "module": "accounting", "entity": "SalesInvoice", "scoped": false, "params": params}),
        ]
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
        if name != "fr_e_reporting" {
            return Err(Error::NotFound(format!("report {name}")));
        }
        let p = Params { raw };
        let mut tx = read_tx(k, ctx).await?;
        let co = resolve_company(&mut tx, ctx, &p).await?;
        let (from, to) = period(&p, &co)?;
        let parent = ferrum_meta::table_for("SalesInvoice");
        let breakdown = ferrum_kernel::ddl::q(&ferrum_meta::child_table(&parent, "tax_breakdown"));
        let parent = ferrum_kernel::ddl::q(&parent);
        let output: Option<Uuid> = sqlx::query_scalar("select id from e_account where tenant_id = $1 and company = $2 and account_number = $3")
            .bind(ctx.tenant)
            .bind(co.id)
            .bind(OUTPUT_TAX)
            .fetch_optional(&mut *tx)
            .await?;
        // Not e-invoices: no PA status (consumers, and businesses abroad).
        let data: Vec<(NaiveDate, Option<String>, Option<Decimal>, Option<Decimal>, Option<Decimal>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select i.posting_date, i.eu_supply, b.rate, sum(b.base_taxable_amount), sum(b.base_tax_amount) \
             from {parent} i join {breakdown} b on b.tenant_id = i.tenant_id and b.parent_id = i.id \
             where i.tenant_id = $1 and i.company = $2 and i.docstatus = 1 and i.posting_date between $3 and $4 and b.account = $5 and i.fr_pa_status is null \
             group by 1, 2, 3 order by 1, 2, 3"
        )))
        .bind(ctx.tenant)
        .bind(co.id)
        .bind(from)
        .bind(to)
        .bind(output)
        .fetch_all(&mut *tx)
        .await?;
        tx.rollback().await?;
        let mut rows: BTreeMap<(NaiveDate, String, Decimal), (Decimal, Decimal)> = BTreeMap::new();
        for (day, supply, rate, taxable, tax) in data {
            let e = rows.entry((day, supply.unwrap_or_else(|| "Domestic".into()), rate.unwrap_or_default().normalize())).or_default();
            e.0 += taxable.unwrap_or_default();
            e.1 += tax.unwrap_or_default();
        }
        let rows: Vec<Value> = rows
            .into_iter()
            .map(|((day, supply, rate), (taxable, tax))| {
                json!({"date": day, "supply": supply, "rate": rate.to_string(), "taxable": crate::util::money(taxable), "tax": crate::util::money(tax)})
            })
            .collect();
        Ok(json!({"name": name, "label": "E-reporting (transactions)", "rows": rows, "meta": {"from_date": from, "to_date": to},
                  "columns": [col("date", "Date", "date"), col("supply", "Supply", "data"), col("rate", "Rate %", "data"), col("taxable", "Taxable", "currency"), col("tax", "VAT", "currency")]}))
    }
}

pub fn rules() -> France {
    France
}

fn hooks(h: &mut ferrum_kernel::HookRegistry) {
    for entity in ["Company", "Customer", "Supplier", "SalesInvoice"] {
        h.register(entity, std::sync::Arc::new(PaHook));
    }
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![std::sync::Arc::new(EReporting)]
}

crate::app!("fr");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sirens_sirets_and_vat_numbers() {
        assert_eq!(siren("732 829 320").as_deref(), Some("732829320"));
        assert_eq!(siren("732829321"), None);
        assert_eq!(siret("73282932000074").as_deref(), Some("73282932000074"));
        assert_eq!(siret("73282932000075"), None);
        assert_eq!(vat_siren("FR44732829320").as_deref(), Some("732829320"), "the key 44 of SIREN 732829320");
        assert_eq!(vat_siren("FR45732829320"), None);
    }

    #[test]
    fn the_mentions() {
        assert_eq!(category(&[Kind::Goods]), "Catégorie de l'opération : livraison de biens.");
        assert_eq!(category(&[Kind::Goods, Kind::Services]), "Catégorie de l'opération : livraison de biens et prestation de services.");
        assert!(penalties("10 %").contains("indemnité forfaitaire pour frais de recouvrement de 40 €"));
    }
}
