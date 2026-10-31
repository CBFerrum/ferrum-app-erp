//! Portugal: on the EU's VAT (23%, 13%, 6% from TEDB), invoices come from software certified by
//! the AT (Portaria 363/2010; the technical requirements of Despacho 8632/2014). On submit, an
//! invoice takes the next number of its document type's series (FT, or NC for a credit note: a
//! series communicated to the AT, whose validation code it holds) and its ATCUD (the validation
//! code, a hyphen and the number, Portaria 195/2020); it is signed with the software producer's
//! RSA key (SHA-1, PKCS#1 v1.5) over "InvoiceDate;SystemEntryDate;InvoiceNo;GrossTotal;Hash",
//! chained to the hash of the series' previous document, and carries the four hash characters
//! (positions 1, 11, 21, 31) with "Processado por programa certificado n.º …/AT", and the QR
//! code's message in the AT's field order (A to S, `*`-separated, amounts with two decimals).
//! The producer's key and certificate number are the deployment's (`FERRUM_PT_SIGNING_KEY`,
//! `FERRUM_PT_CERTIFICATE`, `FERRUM_PT_KEY_VERSION`): without them, Portuguese invoices are
//! refused, as invoices from uncertified software would not be valid. NIFs are checked (modulo
//! 11). Small businesses under article 53 of the CIVA charge no VAT; the reverse charge on
//! construction (article 2(1)(j)) is the item's.

use crate::member::{Member, Rules};
use crate::util::{b, d, get, id, rows, s, sys};
use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc, Weekday};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, Filter, HookCtx, Result};
use rsa::RsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::signature::{SignatureEncoding, Signer};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::{Value, json};
use sha1::Sha1;

pub static MEMBER: Member = Member {
    package: "pt",
    country: "PT",
    small: Some(("pt_small_business", "Isento artigo 53.º do CIVA", "IVA - regime de isenção [art. 53.º do CIVA].")),
    reverse: Some(("pt_reverse_charge", "IVA - autoliquidação [art. 2.º, n.º 1, alínea j) do CIVA].")),
};

/// A NIF: nine digits, the last a check digit (weights 9 to 2, modulo 11).
pub fn nif(raw: &str) -> Option<String> {
    let n: String = raw.trim().trim_start_matches(['P', 'T', 'p', 't']).chars().filter(|c| !c.is_whitespace()).collect();
    if n.len() != 9 || !n.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let d: Vec<u32> = n.bytes().map(|c| u32::from(c - b'0')).collect();
    let sum: u32 = d[..8].iter().enumerate().map(|(i, x)| x * (9 - i as u32)).sum();
    let check = match 11 - sum % 11 {
        10 | 11 => 0,
        c => c,
    };
    (check == d[8]).then_some(n)
}

fn money(v: Decimal) -> String {
    let mut x = v.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    x.rescale(2);
    x.to_string()
}

/// The text a document's signature is over (the previous document's hash empty for a series' first).
pub fn message(invoice_date: &str, system_entry: &str, number: &str, gross: Decimal, previous: &str) -> String {
    format!("{invoice_date};{system_entry};{number};{};{previous}", money(gross))
}

/// The signature (the document's hash): RSA with SHA-1 and PKCS#1 v1.5, in base 64.
pub fn sign(key: &RsaPrivateKey, text: &str) -> String {
    let signer = SigningKey::<Sha1>::new(key.clone());
    base64::engine::general_purpose::STANDARD.encode(signer.sign(text.as_bytes()).to_bytes())
}

/// The hash's characters at positions 1, 11, 21 and 31.
pub fn four(hash: &str) -> String {
    hash.chars().enumerate().filter(|(i, _)| matches!(i, 0 | 10 | 20 | 30)).map(|(_, c)| c).collect()
}

/// The print's mention of the signature and the certificate.
pub fn mention(hash: &str, certificate: &str) -> String {
    format!("{}-Processado por programa certificado n.º {certificate}/AT", four(hash))
}

/// What the QR code's message is made of.
#[derive(Debug, Clone, Default)]
pub struct Qr {
    pub issuer: String,
    pub buyer: String,
    pub buyer_country: String,
    pub doc_type: String,
    pub status: String,
    pub date: String,
    pub number: String,
    pub atcud: String,
    pub region: String,
    pub exempt: Decimal,
    /// (base, VAT) at the reduced, intermediate and normal rates.
    pub reduced: (Decimal, Decimal),
    pub intermediate: (Decimal, Decimal),
    pub normal: (Decimal, Decimal),
    pub not_subject: Decimal,
    pub stamp_duty: Decimal,
    pub taxes: Decimal,
    pub gross: Decimal,
    pub withholding: Decimal,
    pub hash: String,
    pub certificate: String,
    pub other: Option<String>,
}

/// The QR code's message (Especificações Técnicas, Portaria 195/2020): fields in order, empty
/// optional ones left out.
pub fn qr_message(q: &Qr) -> String {
    let mut f: Vec<String> = vec![
        format!("A:{}", q.issuer),
        format!("B:{}", q.buyer),
        format!("C:{}", q.buyer_country),
        format!("D:{}", q.doc_type),
        format!("E:{}", q.status),
        format!("F:{}", q.date),
        format!("G:{}", q.number),
        format!("H:{}", q.atcud),
        format!("I1:{}", q.region),
    ];
    let mut opt = |code: &str, v: Decimal| {
        if !v.is_zero() {
            f.push(format!("{code}:{}", money(v)));
        }
    };
    opt("I2", q.exempt);
    opt("I3", q.reduced.0);
    opt("I4", q.reduced.1);
    opt("I5", q.intermediate.0);
    opt("I6", q.intermediate.1);
    opt("I7", q.normal.0);
    opt("I8", q.normal.1);
    opt("L", q.not_subject);
    opt("M", q.stamp_duty);
    f.push(format!("N:{}", money(q.taxes)));
    f.push(format!("O:{}", money(q.gross)));
    if !q.withholding.is_zero() {
        f.push(format!("P:{}", money(q.withholding)));
    }
    f.push(format!("Q:{}", four(&q.hash)));
    f.push(format!("R:{}", q.certificate));
    if let Some(o) = q.other.as_ref().filter(|o| !o.is_empty()) {
        f.push(format!("S:{}", o.replace('*', "").chars().take(65).collect::<String>()));
    }
    f.join("*")
}

/// Mainland Portugal's time (WET, WEST from the last Sunday of March to the last Sunday of
/// October, at 01:00 UTC), as SystemEntryDate takes it.
pub fn lisbon(now: DateTime<Utc>) -> String {
    let last_sunday = |y: i32, m: u32| -> DateTime<Utc> {
        let first_next = if m == 12 { NaiveDate::from_ymd_opt(y + 1, 1, 1) } else { NaiveDate::from_ymd_opt(y, m + 1, 1) }.unwrap_or_default();
        let mut d = first_next - Duration::days(1);
        while d.weekday() != Weekday::Sun {
            d -= Duration::days(1);
        }
        Utc.from_utc_datetime(&d.and_hms_opt(1, 0, 0).unwrap_or_default())
    };
    let y = now.year();
    let summer = now >= last_sunday(y, 3) && now < last_sunday(y, 10);
    (now + Duration::hours(if summer { 1 } else { 0 })).format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// The deployment's signing configuration: the producer's key, its version and the certificate.
pub fn signing() -> Result<(RsaPrivateKey, String, String)> {
    let pem = std::env::var("FERRUM_PT_SIGNING_KEY").ok().filter(|k| !k.trim().is_empty());
    let certificate = std::env::var("FERRUM_PT_CERTIFICATE").ok().filter(|c| !c.trim().is_empty());
    let (Some(pem), Some(certificate)) = (pem, certificate) else {
        return Err(Error::invalid(
            "Portuguese invoices come from software certified by the AT: this Ferrum has no certification (signing key and certificate number) configured",
        ));
    };
    use rsa::pkcs1::DecodeRsaPrivateKey;
    use rsa::pkcs8::DecodePrivateKey;
    let key = RsaPrivateKey::from_pkcs8_pem(&pem)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(&pem))
        .map_err(|e| Error::internal(format!("the AT signing key: {e}")))?;
    let version = std::env::var("FERRUM_PT_KEY_VERSION").unwrap_or_else(|_| "1".into());
    Ok((key, version, certificate))
}

/// VAT bands by mainland rate: (reduced, intermediate, normal).
fn band(rate: Decimal) -> usize {
    if rate <= Decimal::from(6) {
        0
    } else if rate <= Decimal::from(13) {
        1
    } else {
        2
    }
}

/// Series, numbers, ATCUD, signatures and QR codes on submitted invoices; NIFs checked.
pub struct CertifiedHook;

#[async_trait]
impl DocHook for CertifiedHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.field("eu_vat_number").is_some()
            && let Some(v) = s(doc, "eu_vat_number").map(str::to_uppercase).filter(|v| v.starts_with("PT"))
            && nif(&v).is_none()
        {
            return Err(Error::Validation(vec![FieldError::new("eu_vat_number", "is not a Portuguese NIF: PT and nine digits whose check digit holds")]));
        }
        if hc.entity.def.name == "PtSeries"
            && let Some(code) = s(doc, "validation_code")
            && code.trim().len() < 8
        {
            return Err(Error::Validation(vec![FieldError::new("validation_code", "is the AT's code for the series: at least eight characters")]));
        }
        Ok(())
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.def.name != "SalesInvoice" || hc.entity.field("pt_atcud").is_none() {
            return Ok(());
        }
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let ctx = sys(hc.ctx);
        let company = get(hc.kernel, &ctx, hc.tx, "Company", cid).await?;
        if s(&company, "country_code") != Some("PT") {
            return Ok(());
        }
        let (key, version, certificate) = signing()?;
        let issuer =
            s(&company, "eu_vat_number").and_then(nif).ok_or_else(|| Error::invalid("set the company's NIF (its EU VAT number, PT and nine digits)"))?;
        let credit = b(doc, "is_return");
        let doc_type = if credit { "NC" } else { "FT" };
        // The series, locked until the transaction ends so its numbers and hashes stay in order.
        let series = crate::util::find(
            hc.kernel,
            &ctx,
            hc.tx,
            "PtSeries",
            vec![Filter::eq("company", cid.to_string()), Filter::eq("document_type", doc_type), Filter::eq("disabled", false)],
            Some("created_at asc"),
            1,
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| Error::invalid(format!("communicate a series for {doc_type} documents to the AT and enter it with its validation code (PT Series)")))?;
        let sid = id(&series, "id").ok_or_else(|| Error::internal("series without id"))?;
        let series = hc.kernel.get_tx(&ctx, hc.tx, "PtSeries", sid, true).await?;
        let n = series.get("last_number").and_then(Value::as_i64).unwrap_or(0) + 1;
        let number = format!("{doc_type} {}/{n}", s(&series, "series").unwrap_or_default());
        let atcud = format!("{}-{n}", s(&series, "validation_code").unwrap_or_default().trim());
        let date = s(doc, "posting_date").unwrap_or_default().to_string();
        let entry = lisbon(Utc::now());
        let gross = d(doc, "base_grand_total").abs();
        let previous = s(&series, "last_hash").unwrap_or_default().to_string();
        let hash = sign(&key, &message(&date, &entry, &number, gross, &previous));
        let Value::Object(patch) = json!({"last_number": n, "last_hash": hash}) else { return Err(Error::internal("series")) };
        hc.kernel.set_values_tx(&ctx, hc.tx, "PtSeries", sid, patch).await?;
        // The QR code: the buyer, and the bases and VAT by band.
        let customer = match id(doc, "customer") {
            Some(c) => get(hc.kernel, &ctx, hc.tx, "Customer", c).await?,
            None => Doc::new(),
        };
        let buyer_vat = s(doc, "eu_customer_vat").or(s(&customer, "eu_vat_number")).map(str::to_uppercase);
        let buyer = buyer_vat.as_deref().map(|v| v.trim_start_matches("PT").to_string()).unwrap_or_else(|| "999999990".into());
        let buyer_country = buyer_vat
            .as_deref()
            .filter(|v| v.len() > 2 && !v.starts_with(|c: char| c.is_ascii_digit()))
            .map(|v| v[..2].to_string())
            .unwrap_or_else(|| "PT".into());
        let mut q = Qr {
            issuer,
            buyer,
            buyer_country,
            doc_type: doc_type.into(),
            status: "N".into(),
            date: date.replace('-', ""),
            number: number.clone(),
            atcud: atcud.clone(),
            region: "PT".into(),
            gross,
            hash: hash.clone(),
            certificate: certificate.clone(),
            ..Default::default()
        };
        let output = ferrum_eu::vat::account(hc.kernel, &ctx, hc.tx, cid, ferrum_eu::vat::OUTPUT_TAX).await.ok();
        for g in rows(doc, "tax_breakdown").iter().filter(|g| output.as_deref().is_some_and(|o| s(g, "account") == Some(o))) {
            let (base, tax) = (d(g, "base_taxable_amount").abs(), d(g, "base_tax_amount").abs());
            match ferrum_eu::ubl::category_code(s(g, "category"), d(g, "rate")) {
                "S" => {
                    let slot = match band(d(g, "rate")) {
                        0 => &mut q.reduced,
                        1 => &mut q.intermediate,
                        _ => &mut q.normal,
                    };
                    slot.0 += base;
                    slot.1 += tax;
                }
                "E" | "Z" | "G" | "K" => q.exempt += base,
                _ => q.not_subject += base,
            }
            q.taxes += tax;
        }
        crate::util::set(doc, "pt_document_no", number);
        crate::util::set(doc, "pt_atcud", atcud);
        crate::util::set(doc, "pt_system_entry", entry);
        crate::util::set(doc, "pt_hash", hash.clone());
        crate::util::set(doc, "pt_key_version", version);
        crate::util::set(doc, "pt_hash_mention", mention(&hash, &certificate));
        crate::util::set(doc, "pt_qr", qr_message(&q));
        crate::util::set(doc, "pt_status", "N");
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.entity.def.name != "SalesInvoice" || s(doc, "pt_atcud").is_none() {
            return Ok(());
        }
        // Cancelled (anulado): its number and signature stay; its status and QR code say so.
        let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let qr = s(doc, "pt_qr").unwrap_or_default().replacen("*E:N*", "*E:A*", 1);
        let Value::Object(v) = json!({"pt_status": "A", "pt_qr": qr}) else { return Ok(()) };
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", iid, v).await
    }
}

pub fn rules() -> Rules {
    Rules(&MEMBER)
}

fn hooks(h: &mut ferrum_kernel::HookRegistry) {
    for entity in ["Company", "Customer", "Supplier", "SalesInvoice", "PtSeries"] {
        h.register(entity, std::sync::Arc::new(CertifiedHook));
    }
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

crate::app!("pt");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ats_qr_example() {
        // Especificações Técnicas, exemplo 3 (fatura pró-forma).
        let q = Qr {
            issuer: "500000000".into(),
            buyer: "123456789".into(),
            buyer_country: "PT".into(),
            doc_type: "PF".into(),
            status: "N".into(),
            date: "20190123".into(),
            number: "PF G2019CB/145789".into(),
            atcud: "HB6FT7RV-145789".into(),
            region: "PT".into(),
            exempt: Decimal::new(1234534, 2),
            reduced: (Decimal::new(1253265, 2), Decimal::new(75196, 2)),
            intermediate: (Decimal::new(5278900, 2), Decimal::new(686257, 2)),
            normal: (Decimal::new(3242569, 2), Decimal::new(745791, 2)),
            taxes: Decimal::new(1507244, 2),
            gross: Decimal::new(12516512, 2),
            hash: "r000000000/000000000f000000000Y".into(),
            certificate: "9999".into(),
            ..Default::default()
        };
        assert_eq!(
            qr_message(&q),
            "A:500000000*B:123456789*C:PT*D:PF*E:N*F:20190123*G:PF G2019CB/145789*H:HB6FT7RV-145789*I1:PT*I2:12345.34*I3:12532.65*I4:751.96*I5:52789.00*I6:6862.57*I7:32425.69*I8:7457.91*N:15072.44*O:125165.12*Q:r/fY*R:9999"
        );
    }

    #[test]
    fn nifs_messages_and_lisbon_time() {
        assert_eq!(nif("PT 501 964 843").as_deref(), Some("501964843"));
        assert_eq!(nif("501964844"), None);
        assert_eq!(
            message("2010-05-18", "2010-05-18T11:22:19", "FAC 001/14", Decimal::new(312, 2), ""),
            "2010-05-18;2010-05-18T11:22:19;FAC 001/14;3.12;",
            "the Despacho's first record"
        );
        assert_eq!(mention("AbcdefghijXklmnopqrsYtuvwxyz12Z", "0000"), "AXYZ-Processado por programa certificado n.º 0000/AT");
        let at = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        assert_eq!(lisbon(at("2026-07-01T12:00:00Z")), "2026-07-01T13:00:00", "summer time");
        assert_eq!(lisbon(at("2026-12-01T12:00:00Z")), "2026-12-01T12:00:00", "winter time");
        assert_eq!(lisbon(at("2026-03-29T00:59:59Z")), "2026-03-29T00:59:59", "before the change on the last Sunday of March");
        assert_eq!(lisbon(at("2026-03-29T01:00:00Z")), "2026-03-29T02:00:00");
    }
}
