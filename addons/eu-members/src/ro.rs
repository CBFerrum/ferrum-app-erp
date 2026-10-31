//! Romania: on the EU's VAT (21% from August 2025, 11% reduced, from TEDB), every invoice goes to
//! ANAF's e-Factura system as an EN 16931 UBL invoice under the CIUS-RO profile, within five days
//! of issue (between businesses since 2024, to consumers since 2025). A submitted invoice's
//! CIUS-RO file is written from the EU's EN 16931 model, its addresses' counties coded as
//! ISO 3166-2:RO (RO-B for Bucharest, whose town is its sector, SECTOR1 to SECTOR6), checked
//! against CIUS-RO's rules, and queued for the company's ANAF connection (OAuth with its
//! qualified certificate) through the outbox; the connection records ANAF's upload index and
//! verdict ([`crate::anaf`] uploads and asks after them). CIFs are checked (key 753217532). Small businesses under article 310 charge no VAT;
//! the domestic reverse charge (article 331: cereals, wood, scrap, buildings...) is the item's.

use crate::member::{Member, Rules};
use crate::util::{get, id, s, set, sys};
use async_trait::async_trait;
use ferrum_eu::ubl::{Invoice, invoice, to_xml};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, HookCtx, Result, outbox};
use serde_json::{Value, json};

pub const CIUS_RO: &str = "urn:cen.eu:en16931:2017#compliant#urn:efactura.mfinante.ro:CIUS-RO:1.0.1";
pub const UPLOAD_TOPIC: &str = "ro.efactura.upload_requested";

pub static MEMBER: Member = Member {
    package: "ro",
    country: "RO",
    small: Some((
        "ro_small_business",
        "Scutit de TVA (art. 310 Cod fiscal)",
        "Regim special de scutire pentru întreprinderile mici: scutit de TVA conform art. 310 din Codul fiscal.",
    )),
    reverse: Some(("ro_reverse_charge", "Taxare inversă (art. 331 din Codul fiscal).")),
};

/// Romania's counties and Bucharest (ISO 3166-2:RO).
pub const COUNTIES: &[(&str, &str)] = &[
    ("AB", "Alba"),
    ("AR", "Arad"),
    ("AG", "Argeș"),
    ("BC", "Bacău"),
    ("BH", "Bihor"),
    ("BN", "Bistrița-Năsăud"),
    ("BT", "Botoșani"),
    ("BV", "Brașov"),
    ("BR", "Brăila"),
    ("B", "București"),
    ("BZ", "Buzău"),
    ("CS", "Caraș-Severin"),
    ("CL", "Călărași"),
    ("CJ", "Cluj"),
    ("CT", "Constanța"),
    ("CV", "Covasna"),
    ("DB", "Dâmbovița"),
    ("DJ", "Dolj"),
    ("GL", "Galați"),
    ("GR", "Giurgiu"),
    ("GJ", "Gorj"),
    ("HR", "Harghita"),
    ("HD", "Hunedoara"),
    ("IL", "Ialomița"),
    ("IS", "Iași"),
    ("IF", "Ilfov"),
    ("MM", "Maramureș"),
    ("MH", "Mehedinți"),
    ("MS", "Mureș"),
    ("NT", "Neamț"),
    ("OT", "Olt"),
    ("PH", "Prahova"),
    ("SM", "Satu Mare"),
    ("SJ", "Sălaj"),
    ("SB", "Sibiu"),
    ("SV", "Suceava"),
    ("TR", "Teleorman"),
    ("TM", "Timiș"),
    ("TL", "Tulcea"),
    ("VS", "Vaslui"),
    ("VL", "Vâlcea"),
    ("VN", "Vrancea"),
];

/// Letters without Romanian diacritics, lower case, for matching names as people write them.
fn plain(v: &str) -> String {
    v.trim()
        .to_lowercase()
        .chars()
        .map(|c| match c {
            'ă' | 'â' => 'a',
            'î' => 'i',
            'ș' | 'ş' => 's',
            'ț' | 'ţ' => 't',
            c => c,
        })
        .collect()
}

/// A county's ISO 3166-2:RO code (RO-CJ) from its code or name ("Cluj", "CJ", "RO-CJ", "Bucuresti").
pub fn county(text: &str) -> Option<String> {
    let t = plain(text);
    let t = t.strip_prefix("ro-").unwrap_or(&t);
    let t = t.strip_prefix("judetul ").unwrap_or(t).trim();
    let t = if t.starts_with("municipiul bucuresti") || t == "bucharest" { "bucuresti" } else { t };
    COUNTIES.iter().find(|(code, name)| code.eq_ignore_ascii_case(t) || plain(name) == t).map(|(code, _)| format!("RO-{code}"))
}

/// A Bucharest sector as CIUS-RO writes it (SECTOR1 to SECTOR6), from "Sector 3" and the like.
pub fn sector(text: &str) -> Option<String> {
    let t = plain(text).replace([' ', '.'], "");
    let n = t.strip_prefix("sectorul").or_else(|| t.strip_prefix("sector")).or_else(|| t.strip_prefix("s"))?;
    matches!(n, "1" | "2" | "3" | "4" | "5" | "6").then(|| format!("SECTOR{n}"))
}

/// A CIF (CUI): up to ten digits, the last checking the rest with the key 753217532.
pub fn cif(raw: &str) -> Option<String> {
    let n: String = raw.trim().trim_start_matches(['R', 'O', 'r', 'o']).chars().filter(|c| !c.is_whitespace()).collect();
    if !(2..=10).contains(&n.len()) || !n.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let digits: Vec<u32> = n.bytes().map(|c| u32::from(c - b'0')).collect();
    let (body, check) = digits.split_at(digits.len() - 1);
    let key = [7, 5, 3, 2, 1, 7, 5, 3, 2];
    let padded: Vec<u32> = std::iter::repeat_n(0, 9 - body.len()).chain(body.iter().copied()).collect();
    let sum: u32 = padded.iter().zip(key).map(|(a, k)| a * k).sum();
    let c = sum * 10 % 11;
    ((if c == 10 { 0 } else { c }) == check[0]).then_some(n)
}

/// CIUS-RO's rules on an EN 16931 invoice: the problems, empty when it complies.
pub fn validate(inv: &Invoice) -> Vec<String> {
    let mut p = Vec::new();
    let mut rule = |ok: bool, id: &str, what: &str| {
        if !ok {
            p.push(format!("{id}: {what}"));
        }
    };
    rule(inv.number.chars().any(|c| c.is_ascii_digit()), "BR-RO-010", "the invoice number has a digit");
    rule(inv.customization.as_deref() == Some(CIUS_RO), "BR-RO-001", "CIUS-RO's specification identifier");
    for (who, party) in [("seller", &inv.seller), ("buyer", &inv.buyer)] {
        rule(party.street.is_some(), "BR-RO-080", &format!("the {who}'s street"));
        rule(party.city.is_some(), "BR-RO-090", &format!("the {who}'s town"));
        if party.country.as_deref() == Some("RO") {
            let sub = party.subdivision.as_deref().unwrap_or_default();
            rule(
                sub.starts_with("RO-") && COUNTIES.iter().any(|(c, _)| format!("RO-{c}") == sub),
                "BR-RO-100",
                &format!("the {who}'s county as ISO 3166-2:RO (RO-CJ)"),
            );
            if sub == "RO-B" {
                rule(
                    party.city.as_deref().is_some_and(|c| sector(c).as_deref() == Some(c)),
                    "BR-RO-110",
                    &format!("the {who}'s Bucharest town is its sector, SECTOR1 to SECTOR6"),
                );
            }
        }
    }
    rule(inv.seller.vat.is_some() || inv.seller.tax_number.is_some(), "BR-RO-120", "the seller's VAT number (RO and its CIF) or tax number");
    p
}

/// A submitted sales invoice as CIUS-RO: the EU's EN 16931 invoice with Romania's profile and codes.
pub async fn efactura(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Invoice> {
    let mut inv = invoice(hc, doc).await?;
    let ctx = sys(hc.ctx);
    let company = get(hc.kernel, &ctx, hc.tx, "Company", id(doc, "company").ok_or_else(|| Error::internal("invoice without company"))?).await?;
    inv.customization = Some(CIUS_RO.into());
    inv.seller.subdivision = s(&company, "ro_county").and_then(county);
    if inv.seller.subdivision.as_deref() == Some("RO-B") {
        inv.seller.city = inv.seller.city.as_deref().and_then(sector).or(inv.seller.city.clone());
    }
    if inv.buyer.country.as_deref() == Some("RO") {
        inv.buyer.subdivision = inv.buyer.subdivision.as_deref().and_then(county);
        if inv.buyer.subdivision.as_deref() == Some("RO-B") {
            inv.buyer.city = inv.buyer.city.as_deref().and_then(sector).or(inv.buyer.city.clone());
        }
    }
    let problems = validate(&inv);
    if !problems.is_empty() {
        return Err(Error::invalid(format!("the e-Factura would not comply with CIUS-RO: {}", problems.join("; "))));
    }
    Ok(inv)
}

/// CIUS-RO on submitted invoices: download, upload to ANAF (through the outbox), record ANAF's
/// answer; CIFs checked on companies and customers.
pub struct EfacturaHook;

impl EfacturaHook {
    async fn save(hc: &mut HookCtx<'_>, doc: &Doc, values: Value) -> Result<()> {
        let inv = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let Value::Object(v) = values else { return Ok(()) };
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", inv, v).await
    }
}

async fn upload(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
    if matches!(s(doc, "ro_efactura_status"), Some("Pending" | "Uploaded" | "Validated")) {
        return Err(Error::invalid(format!("the invoice is already {}", s(doc, "ro_efactura_status").unwrap_or_default().to_lowercase())));
    }
    let inv = efactura(hc, doc).await?;
    let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
    let cif = inv.seller.vat.as_deref().and_then(cif).unwrap_or_default();
    let event = json!({"invoice": iid, "company": s(doc, "company"), "number": inv.number, "cif": cif, "standard": "UBL", "xml": to_xml(&inv)});
    EfacturaHook::save(hc, doc, json!({"ro_efactura_status": "Pending", "ro_efactura_error": null})).await?;
    outbox::enqueue(hc.tx, hc.ctx.tenant, UPLOAD_TOPIC, Some("SalesInvoice"), Some(iid), event).await?;
    Ok(json!({"status": "Pending"}))
}

#[async_trait]
impl DocHook for EfacturaHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.field("eu_vat_number").is_some()
            && let Some(v) = s(doc, "eu_vat_number").map(str::to_uppercase).filter(|v| v.starts_with("RO"))
            && cif(&v).is_none()
        {
            return Err(Error::Validation(vec![FieldError::new("eu_vat_number", "is not a Romanian VAT number: RO and a CIF whose check digit holds")]));
        }
        if hc.entity.field("ro_county").is_some()
            && let Some(c) = s(doc, "ro_county").map(str::to_string)
        {
            let code = county(&c).ok_or_else(|| Error::Validation(vec![FieldError::new("ro_county", "is not a Romanian county")]))?;
            set(doc, "ro_county", code);
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.entity.def.name != "SalesInvoice" || hc.entity.field("ro_efactura_status").is_none() {
            return Ok(());
        }
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let company = get(hc.kernel, &sys(hc.ctx), hc.tx, "Company", cid).await?;
        if s(&company, "country_code") == Some("RO") && crate::util::b(&company, "ro_efactura_on_submit") {
            upload(hc, doc).await?;
        }
        Ok(())
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if matches!(s(doc, "ro_efactura_status"), Some("Pending" | "Uploaded" | "Validated")) {
            return Err(Error::invalid("ANAF has this invoice: issue a credit note (storno) instead of cancelling it"));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if hc.entity.field("ro_efactura_status").is_none() || !name.starts_with("ro_") {
            return Ok(None);
        }
        match name {
            "ro_efactura" => {
                let inv = efactura(hc, doc).await?;
                let file = format!("{}.xml", inv.number.replace(['/', '\\'], "-"));
                Ok(Some(json!({"file": {"name": file, "type": "application/xml", "content": to_xml(&inv)}})))
            }
            "ro_upload_efactura" => upload(hc, doc).await.map(Some),
            "ro_record_anaf" => {
                let index = params.get("upload_index").and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
                let state = params.get("state").and_then(Value::as_str).unwrap_or_default();
                let (status, error) = match state {
                    "ok" => ("Validated", None),
                    "nok" => (
                        "Rejected",
                        Some(params.get("message").and_then(Value::as_str).unwrap_or("ANAF refused the invoice").chars().take(2000).collect::<String>()),
                    ),
                    _ if index.is_some() => ("Uploaded", None),
                    _ => return Err(Error::field("upload_index", "is required (ANAF's index_incarcare), or the state")),
                };
                let download = params.get("download_id").and_then(Value::as_str).map(str::to_string);
                Self::save(
                    hc,
                    doc,
                    json!({"ro_efactura_status": status, "ro_upload_index": index.map(str::to_string).or(s(doc, "ro_upload_index").map(str::to_string)),
                           "ro_download_id": download.or(s(doc, "ro_download_id").map(str::to_string)), "ro_efactura_error": error}),
                )
                .await?;
                Ok(Some(json!({"status": status})))
            }
            _ => Ok(None),
        }
    }
}

pub fn rules() -> Rules {
    Rules(&MEMBER)
}

fn hooks(h: &mut ferrum_kernel::HookRegistry) {
    for entity in ["Company", "Customer", "Supplier", "SalesInvoice"] {
        h.register(entity, std::sync::Arc::new(EfacturaHook));
    }
    h.register("Company", std::sync::Arc::new(crate::anaf::AnafHook));
    h.add_event_sink(std::sync::Arc::new(crate::anaf::Anaf::default()));
    h.add_site(PACKAGE, std::sync::Arc::new(crate::anaf::AnafSite));
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

crate::app!("ro");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counties_sectors_and_cifs() {
        assert_eq!((county("Cluj").as_deref(), county("RO-CJ").as_deref(), county("Județul Iași").as_deref()), (Some("RO-CJ"), Some("RO-CJ"), Some("RO-IS")));
        assert_eq!((county("Bucuresti").as_deref(), county("Municipiul București").as_deref(), county("Atlantis")), (Some("RO-B"), Some("RO-B"), None));
        assert_eq!((sector("Sector 3").as_deref(), sector("SECTOR6").as_deref(), sector("Sectorul 7")), (Some("SECTOR3"), Some("SECTOR6"), None));
        assert_eq!(cif("RO 18547290").as_deref(), Some("18547290"));
        assert_eq!(cif("18547291"), None);
    }
}
