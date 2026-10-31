//! Spain: on the EU's VAT (21%, 10%, 4% from TEDB), invoicing software keeps VERI*FACTU records
//! (Reglamento de sistemas informáticos de facturación, RD 1007/2023; adapted before 1 January
//! 2027 for companies and 1 July 2027 for others, RDL 15/2025). Each submitted invoice gets an
//! invoicing record (registro de alta) whose SHA-256 fingerprint (huella) chains it to the
//! company's previous record, exactly as AEAT's specification computes it (its test vectors are
//! this module's); cancelling an invoice adds a cancellation record (registro de anulación) to the
//! chain. The invoice carries the QR code's address for AEAT's verification service and the words
//! "VERI*FACTU". When the company sends its records to AEAT (VERI*FACTU), they are queued for its
//! connection to AEAT's web service (with its certificate) through the outbox, which records AEAT's
//! answer. Sales to the Canary Islands, Ceuta and Melilla are outside the EU's VAT area: goods go
//! there as exports and services to businesses there are not subject to Spanish VAT. Retailers
//! under the recargo de equivalencia pay the surcharge with the VAT (5.2%, 1.4%, 0.5%).

use crate::util::{b, d, get, id, rows, s, set, sys};
use async_trait::async_trait;
use chrono::Utc;
use ferrum_app_erp::localization::{Localization, ship_to};
use ferrum_app_erp::pricing::Side;
use ferrum_eu::vat::{OUTPUT_TAX, account, kind_of};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, Result, Tx, outbox};
use ferrum_meta::EffEntity;
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use uuid::Uuid;

pub const SEND_TOPIC: &str = "es.verifactu.send_requested";
pub const QR_TEST: &str = "https://prewww2.aeat.es/wlpl/TIKE-CONT/ValidarQR";
pub const QR_PRODUCTION: &str = "https://www2.agenciatributaria.gob.es/wlpl/TIKE-CONT/ValidarQR";
pub const QR_TEXT: &str = "Factura verificable en la sede electrónica de la AEAT";
/// The surcharge's own account (recargo de equivalencia repercutido).
pub const SURCHARGE: &str = "2152";

// ───────────────────────────── NIFs ─────────────────────────────

const DNI_LETTERS: &[u8] = b"TRWAGMYFPDXBNJZSQVHLCKE";

/// A Spanish NIF: a person's DNI (8 digits and a letter) or NIE (X, Y or Z, 7 digits and a
/// letter), checked modulo 23, or an entity's (a letter, 7 digits and a control digit or letter).
pub fn nif(raw: &str) -> Option<String> {
    let n: String = raw.trim().trim_start_matches(['E', 'S', 'e', 's']).chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_uppercase();
    let n = if raw.trim().len() == 9 && raw.trim().to_uppercase().starts_with("ES") { raw.trim().to_uppercase() } else { n };
    if n.len() != 9 {
        return None;
    }
    let c: Vec<char> = n.chars().collect();
    let letter = |num: u32| DNI_LETTERS[(num % 23) as usize] as char;
    if c[..8].iter().all(char::is_ascii_digit) {
        let num: u32 = n[..8].parse().ok()?;
        return (letter(num) == c[8]).then_some(n);
    }
    if matches!(c[0], 'X' | 'Y' | 'Z') && c[1..8].iter().all(char::is_ascii_digit) {
        let prefix = match c[0] {
            'X' => '0',
            'Y' => '1',
            _ => '2',
        };
        let num: u32 = format!("{prefix}{}", &n[1..8]).parse().ok()?;
        return (letter(num) == c[8]).then_some(n);
    }
    if c[0].is_ascii_alphabetic() && c[1..8].iter().all(char::is_ascii_digit) {
        let digits: Vec<u32> = c[1..8].iter().map(|x| x.to_digit(10).unwrap_or(0)).collect();
        let even: u32 = digits.iter().skip(1).step_by(2).sum();
        let odd: u32 = digits.iter().step_by(2).map(|x| (x * 2) / 10 + (x * 2) % 10).sum();
        let control = (10 - (even + odd) % 10) % 10;
        let as_letter = b"JABCDEFGHI"[control as usize] as char;
        let ok = c[8] == char::from_digit(control, 10).unwrap_or('?') || c[8] == as_letter;
        return ok.then_some(n);
    }
    None
}

// ───────────────────────────── the records ─────────────────────────────

/// An amount as the records write it: two decimals.
pub fn amount(v: Decimal) -> String {
    let mut x = v.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    x.rescale(2);
    x.to_string()
}

/// A day as the records write it: DD-MM-YYYY.
pub fn day(iso: &str) -> String {
    let p: Vec<&str> = iso.split('-').collect();
    if p.len() == 3 { format!("{}-{}-{}", p[2], p[1], p[0]) } else { iso.to_string() }
}

fn sha256_upper(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|b| format!("{b:02X}")).collect()
}

/// A registro de alta's fingerprint (huella), from its fields as AEAT's specification orders them.
#[allow(clippy::too_many_arguments)]
pub fn huella_alta(issuer: &str, number: &str, date: &str, kind: &str, tax: &str, total: &str, previous: &str, generated: &str) -> String {
    sha256_upper(&format!(
        "IDEmisorFactura={}&NumSerieFactura={}&FechaExpedicionFactura={}&TipoFactura={}&CuotaTotal={}&ImporteTotal={}&Huella={}&FechaHoraHusoGenRegistro={}",
        issuer.trim(),
        number.trim(),
        date.trim(),
        kind.trim(),
        tax.trim(),
        total.trim(),
        previous.trim(),
        generated.trim()
    ))
}

/// A registro de anulación's fingerprint.
pub fn huella_anulacion(issuer: &str, number: &str, date: &str, previous: &str, generated: &str) -> String {
    sha256_upper(&format!(
        "IDEmisorFacturaAnulada={}&NumSerieFacturaAnulada={}&FechaExpedicionFacturaAnulada={}&Huella={}&FechaHoraHusoGenRegistro={}",
        issuer.trim(),
        number.trim(),
        date.trim(),
        previous.trim(),
        generated.trim()
    ))
}

/// Percent-encoding of a QR parameter (UTF-8; unreserved characters kept).
fn encode(v: &str) -> String {
    v.bytes().map(|c| if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~') { (c as char).to_string() } else { format!("%{c:02X}") }).collect()
}

/// The address the invoice's QR code holds, for AEAT's verification service.
pub fn qr_url(production: bool, issuer: &str, number: &str, date: &str, total: &str) -> String {
    format!(
        "{}?nif={}&numserie={}&fecha={}&importe={}",
        if production { QR_PRODUCTION } else { QR_TEST },
        encode(issuer),
        encode(number),
        encode(date),
        encode(total)
    )
}

/// The record's tax lines (Desglose): an EN 16931 category, its rate, base and VAT → the
/// record's qualification (S1 subject, S2 reverse charge, N2 not subject, E1–E6 exempt).
pub fn qualification(category: &str) -> (&'static str, Option<&'static str>) {
    match category {
        "S" | "Z" => ("S1", None),
        "AE" => ("S2", None),
        "O" => ("N2", None),
        "G" => ("", Some("E2")),
        "K" => ("", Some("E5")),
        _ => ("", Some("E1")),
    }
}

/// The record's type: F1 a full invoice, F2 a simplified one (no customer NIF); R1 and R5 their
/// corrections.
pub fn invoice_type(correction: bool, buyer_identified: bool) -> &'static str {
    match (correction, buyer_identified) {
        (false, true) => "F1",
        (false, false) => "F2",
        (true, true) => "R1",
        (true, false) => "R5",
    }
}

// ───────────────────────────── where Spanish VAT applies ─────────────────────────────

/// Postal codes outside the EU's VAT area: Las Palmas (35), Santa Cruz de Tenerife (38), Ceuta
/// (51) and Melilla (52).
pub fn outside_vat_area(postal: &str) -> bool {
    let p = postal.trim();
    p.len() == 5 && p.bytes().all(|c| c.is_ascii_digit()) && matches!(&p[..2], "35" | "38" | "51" | "52")
}

/// The surcharge for a VAT rate (recargo de equivalencia).
pub fn surcharge(vat: Decimal) -> Decimal {
    match vat {
        v if v == Decimal::from(21) => Decimal::new(52, 1),
        v if v == Decimal::from(10) => Decimal::new(14, 1),
        v if v == Decimal::from(4) => Decimal::new(5, 1),
        _ => Decimal::ZERO,
    }
}

pub struct Spain;

async fn spanish(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc) -> Result<Option<Doc>> {
    let Some(cid) = id(doc, "company") else { return Ok(None) };
    let company = get(k, &sys(ctx), tx, "Company", cid).await?;
    Ok((s(&company, "country_code") == Some("ES")).then_some(company))
}

async fn retailer(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc) -> Result<bool> {
    Ok(match id(doc, "customer") {
        Some(c) => b(&get(k, &sys(ctx), tx, "Customer", c).await?, "es_recargo_equivalencia"),
        None => false,
    })
}

#[async_trait]
impl Localization for Spain {
    fn package(&self) -> &'static str {
        crate::es::PACKAGE
    }

    async fn determine(&self, k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()> {
        if side != Side::Selling || doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 || ent.field("taxes").is_none() {
            return Ok(());
        }
        let Some(company) = spanish(k, ctx, tx, doc).await? else { return Ok(()) };
        let cid = id(&company, "id").ok_or_else(|| Error::internal("company without id"))?;
        let Ok(rate_account) = account(k, ctx, tx, cid, SURCHARGE).await else { return Ok(()) };
        let mut taxes = rows(doc, "taxes");
        let has = taxes.iter().any(|t| s(t, "account") == Some(rate_account.as_str()));
        let wants = retailer(k, ctx, tx, doc).await?;
        if has == wants {
            return Ok(());
        }
        if wants {
            let Value::Object(row) = json!({"id": Uuid::now_v7().to_string(), "charge_type": "On Net Total", "account": rate_account, "description": "Recargo de equivalencia", "rate": "5.2"})
            else {
                return Ok(());
            };
            taxes.push(row);
        } else {
            taxes.retain(|t| s(t, "account") != Some(rate_account.as_str()));
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
        if side != Side::Selling {
            return Ok(());
        }
        let Some(company) = spanish(k, ctx, tx, doc).await? else { return Ok(()) };
        let Some(cid) = id(&company, "id") else { return Ok(()) };
        let Ok(output) = account(k, ctx, tx, cid, OUTPUT_TAX).await else { return Ok(()) };
        let surcharge_account = account(k, ctx, tx, cid, SURCHARGE).await.ok().filter(|a| taxes.iter().any(|t| s(t, "account") == Some(a.as_str())));
        let destination = ship_to(k, &sys(ctx), tx, doc).await?;
        let away = destination.as_ref().is_some_and(|a| s(a, "country_code") == Some("ES") && s(a, "postal_code").is_some_and(outside_vat_area));
        let business = match id(doc, "customer") {
            Some(c) => {
                let cust = get(k, &sys(ctx), tx, "Customer", c).await?;
                s(&cust, "eu_vat_number").is_some() || s(&cust, "tax_id").is_some() || s(&cust, "customer_type") == Some("Company")
            }
            None => false,
        };
        for (i, (line, item)) in lines.iter().zip(items).enumerate() {
            if id(line, "item").is_none() {
                continue;
            }
            if away && taxes.iter().any(|t| s(t, "account") == Some(output.as_str())) {
                let goods = matches!(kind_of(item), ferrum_eu::vat::Kind::Goods);
                let entry = if goods {
                    Some(
                        json!({"tax_account": output, "rate": "0", "category": "Export", "exemption_reason": "Exenta por exportación (art. 21 LIVA): Canarias, Ceuta o Melilla"}),
                    )
                } else if business {
                    Some(
                        json!({"tax_account": output, "rate": "0", "category": "Out of Scope", "exemption_reason": "No sujeta (art. 69 LIVA): servicio a empresario en Canarias, Ceuta o Melilla"}),
                    )
                } else {
                    None
                };
                if let Some(Value::Object(e)) = entry {
                    rates[i].retain(|r| s(r, "tax_account") != Some(output.as_str()));
                    rates[i].push(e);
                }
            }
            // The surcharge follows each line's VAT rate.
            if let Some(acc) = &surcharge_account {
                let vat: Decimal = rates[i].iter().find(|r| s(r, "tax_account") == Some(output.as_str())).map(|r| d(r, "rate")).unwrap_or_default();
                let Value::Object(e) =
                    json!({"tax_account": acc, "rate": surcharge(vat).to_string(), "category": if vat.is_zero() { "Exempt" } else { "Standard" }})
                else {
                    continue;
                };
                rates[i].retain(|r| s(r, "tax_account") != Some(acc.as_str()));
                rates[i].push(e);
            }
        }
        Ok(())
    }

    async fn setup_company(&self, hc: &mut HookCtx<'_>, company: Uuid, accounts: &HashMap<&str, String>) -> Result<()> {
        let sctx = sys(hc.ctx);
        let co = hc.kernel.get_tx(&sctx, hc.tx, "Company", company, false).await?;
        if s(&co, "country_code") != Some("ES") {
            return Ok(());
        }
        let Some(parent) = accounts.get("Duties and Taxes") else { return Ok(()) };
        let Value::Object(a) = json!({"account_name": "Recargo de Equivalencia", "account_number": SURCHARGE, "company": company.to_string(), "is_group": false,
                                      "root_type": "Liability", "account_type": "Tax", "parent_account": parent, "account_currency": co.get("default_currency")})
        else {
            return Ok(());
        };
        hc.kernel.insert_tx(&sctx, hc.tx, "Account", a).await.map(|_| ())
    }
}

// ───────────────────────────── the records' upkeep ─────────────────────────────

/// Invoicing records on submit and cancel, NIFs checked, records sent to AEAT.
pub struct VerifactuHook;

fn now_with_offset() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}

/// Takes the company's chain (its row locked until the transaction ends): the previous
/// fingerprint, and stores the new one.
async fn chain(hc: &mut HookCtx<'_>, company: Uuid, next: impl FnOnce(&str) -> String) -> Result<(String, String)> {
    let ctx = sys(hc.ctx);
    let co = hc.kernel.get_tx(&ctx, hc.tx, "Company", company, true).await?;
    let previous = s(&co, "es_last_huella").unwrap_or_default().to_string();
    let huella = next(&previous);
    let Value::Object(patch) = json!({"es_last_huella": huella}) else { return Err(Error::internal("chain")) };
    hc.kernel.set_values_tx(&ctx, hc.tx, "Company", company, patch).await?;
    Ok((previous, huella))
}

#[async_trait]
impl DocHook for VerifactuHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.field("eu_vat_number").is_some()
            && let Some(v) = s(doc, "eu_vat_number").map(str::to_uppercase).filter(|v| v.starts_with("ES"))
            && nif(&v).is_none()
        {
            return Err(Error::Validation(vec![FieldError::new("eu_vat_number", "is not a Spanish NIF: ES and a DNI, NIE or entity NIF whose check holds")]));
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.entity.def.name != "SalesInvoice" || hc.entity.field("es_huella").is_none() {
            return Ok(());
        }
        let Some(company) = spanish(hc.kernel, hc.ctx, hc.tx, doc).await? else { return Ok(()) };
        let cid = id(&company, "id").ok_or_else(|| Error::internal("company without id"))?;
        let issuer = s(&company, "eu_vat_number").and_then(nif).ok_or_else(|| Error::invalid("set the company's NIF (its EU VAT number, ES and the NIF)"))?;
        let customer = match id(doc, "customer") {
            Some(c) => get(hc.kernel, &sys(hc.ctx), hc.tx, "Customer", c).await?,
            None => Doc::new(),
        };
        let identified = s(doc, "eu_customer_vat").or(s(&customer, "eu_vat_number")).or(s(&customer, "tax_id")).is_some();
        let correction = b(doc, "is_return");
        let kind = invoice_type(correction, identified);
        let number = s(doc, "number").unwrap_or_default().to_string();
        let date = day(s(doc, "posting_date").unwrap_or_default());
        let tax = amount(d(doc, "total_taxes"));
        let total = amount(d(doc, "grand_total"));
        let generated = now_with_offset();
        let (previous, huella) = chain(hc, cid, |prev| huella_alta(&issuer, &number, &date, kind, &tax, &total, prev, &generated)).await?;
        let production = s(&company, "es_verifactu_env") == Some("Production");
        let send = b(&company, "es_verifactu");
        let url = qr_url(production, &issuer, &number, &date, &total);
        let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let Value::Object(values) = json!({
            "es_tipo_factura": kind, "es_huella": huella, "es_huella_anterior": if previous.is_empty() { None } else { Some(previous.clone()) },
            "es_fecha_gen": generated, "es_qr_url": url, "es_qr_text": if send { "VERI*FACTU" } else { "" },
            "es_verifactu_status": if send { Some("Pending") } else { None }, "es_verifactu_error": null,
        }) else {
            return Err(Error::internal("record"));
        };
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", iid, values).await?;
        if send {
            let breakdown: Vec<Value> = rows(doc, "tax_breakdown")
                .iter()
                .map(|g| {
                    let category = ferrum_eu::ubl::category_code(s(g, "category"), d(g, "rate"));
                    let (calificacion, exenta) = qualification(category);
                    json!({"Impuesto": "01", "ClaveRegimen": "01", "CalificacionOperacion": calificacion, "OperacionExenta": exenta, "TipoImpositivo": amount(d(g, "rate")),
                           "BaseImponibleOimporteNoSujeto": amount(d(g, "taxable_amount")), "CuotaRepercutida": amount(d(g, "tax_amount"))})
                })
                .collect();
            let record = json!({
                "RegistroAlta": {
                    "IDVersion": "1.0", "IDFactura": {"IDEmisorFactura": issuer, "NumSerieFactura": number, "FechaExpedicionFactura": date},
                    "NombreRazonEmisor": s(&company, "company_name"), "TipoFactura": kind, "DescripcionOperacion": format!("Factura {number}"),
                    "Destinatarios": if identified { json!([{"NombreRazon": s(&customer, "customer_name"), "NIF": s(doc, "eu_customer_vat").or(s(&customer, "eu_vat_number")).or(s(&customer, "tax_id"))}]) } else { Value::Null },
                    "Desglose": breakdown, "CuotaTotal": tax, "ImporteTotal": total,
                    "Encadenamiento": if previous.is_empty() { json!({"PrimerRegistro": "S"}) } else { json!({"RegistroAnterior": {"Huella": previous}}) },
                    "FechaHoraHusoGenRegistro": generated, "TipoHuella": "01", "Huella": huella,
                }
            });
            outbox::enqueue(hc.tx, hc.ctx.tenant, SEND_TOPIC, Some("SalesInvoice"), Some(iid), json!({"invoice": iid, "company": cid, "record": record}))
                .await?;
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.entity.def.name != "SalesInvoice" || s(doc, "es_huella").is_none() {
            return Ok(());
        }
        let Some(company) = spanish(hc.kernel, hc.ctx, hc.tx, doc).await? else { return Ok(()) };
        let cid = id(&company, "id").ok_or_else(|| Error::internal("company without id"))?;
        let issuer = s(&company, "eu_vat_number").and_then(nif).unwrap_or_default();
        let number = s(doc, "number").unwrap_or_default().to_string();
        let date = day(s(doc, "posting_date").unwrap_or_default());
        let generated = now_with_offset();
        let (previous, huella) = chain(hc, cid, |prev| huella_anulacion(&issuer, &number, &date, prev, &generated)).await?;
        let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let send = b(&company, "es_verifactu");
        let Value::Object(values) = json!({"es_huella_anulacion": huella, "es_verifactu_status": if send { Some("Pending") } else { None }}) else {
            return Ok(());
        };
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", iid, values).await?;
        if send {
            let record = json!({
                "RegistroAnulacion": {
                    "IDVersion": "1.0", "IDFactura": {"IDEmisorFacturaAnulada": issuer, "NumSerieFacturaAnulada": number, "FechaExpedicionFacturaAnulada": date},
                    "Encadenamiento": if previous.is_empty() { json!({"PrimerRegistro": "S"}) } else { json!({"RegistroAnterior": {"Huella": previous}}) },
                    "FechaHoraHusoGenRegistro": generated, "TipoHuella": "01", "Huella": huella,
                }
            });
            outbox::enqueue(hc.tx, hc.ctx.tenant, SEND_TOPIC, Some("SalesInvoice"), Some(iid), json!({"invoice": iid, "company": cid, "record": record}))
                .await?;
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "es_record_aeat" || hc.entity.field("es_verifactu_status").is_none() {
            return Ok(None);
        }
        if s(doc, "es_verifactu_status") != Some("Pending") {
            return Err(Error::invalid("only a record waiting for AEAT is recorded"));
        }
        let state = params.get("state").and_then(Value::as_str).unwrap_or_default();
        let accepted = matches!(state, "Correcto" | "AceptadoConErrores");
        let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let Value::Object(values) = json!({
            "es_verifactu_status": if accepted { "Accepted" } else { "Rejected" }, "es_csv": params.get("csv").and_then(Value::as_str),
            "es_verifactu_error": if state == "Correcto" { None } else { params.get("message").and_then(Value::as_str).map(|m| m.chars().take(2000).collect::<String>()) },
        }) else {
            return Ok(None);
        };
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", iid, values).await?;
        Ok(Some(json!({"status": if accepted { "Accepted" } else { "Rejected" }})))
    }
}

pub fn rules() -> Spain {
    Spain
}

fn hooks(h: &mut ferrum_kernel::HookRegistry) {
    for entity in ["Company", "Customer", "Supplier", "SalesInvoice"] {
        h.register(entity, std::sync::Arc::new(VerifactuHook));
    }
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

crate::app!("es");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aeats_own_test_vectors() {
        let first = huella_alta("89890001K", "12345678/G33", "01-01-2024", "F1", "12.35", "123.45", "", "2024-01-01T19:20:30+01:00");
        assert_eq!(first, "3C464DAF61ACB827C65FDA19F352A4E3BDC2C640E9E9FC4CC058073F38F12F60");
        let second = huella_alta("89890001K", "12345679/G34", "01-01-2024", "F1", "12.35", "123.45", &first, "2024-01-01T19:20:35+01:00");
        assert_eq!(second, "F7B94CFD8924EDFF273501B01EE5153E4CE8F259766F88CF6ACB8935802A2B97");
        let cancel = huella_anulacion("89890001K", "12345679/G34", "01-01-2024", &second, "2024-01-01T19:20:40+01:00");
        assert!(cancel.starts_with("177547C0D57AC74748561D054A9CEC14B4C4EA23D1BEFD6F2E69E3A388"), "{cancel}");
        assert_eq!(cancel.len(), 64);
    }

    #[test]
    fn the_qr_code_and_the_records_fields() {
        assert_eq!(
            qr_url(false, "89890001K", "12345678&G33", "01-01-2024", "241.40"),
            "https://prewww2.aeat.es/wlpl/TIKE-CONT/ValidarQR?nif=89890001K&numserie=12345678%26G33&fecha=01-01-2024&importe=241.40"
        );
        assert!(
            qr_url(true, "B12345674", "F/1", "31-10-2026", "121.00")
                .starts_with("https://www2.agenciatributaria.gob.es/wlpl/TIKE-CONT/ValidarQR?nif=B12345674&numserie=F%2F1")
        );
        assert_eq!((day("2026-10-31"), amount(Decimal::new(1231, 1))), ("31-10-2026".to_string(), "123.10".to_string()));
        assert_eq!((invoice_type(false, true), invoice_type(false, false), invoice_type(true, true)), ("F1", "F2", "R1"));
        assert_eq!(
            (qualification("S"), qualification("AE"), qualification("K"), qualification("G")),
            (("S1", None), ("S2", None), ("", Some("E5")), ("", Some("E2")))
        );
    }

    #[test]
    fn nifs_and_the_islands() {
        assert_eq!(nif("12345678Z").as_deref(), Some("12345678Z"), "a DNI");
        assert_eq!(nif("X1234567L").as_deref(), Some("X1234567L"), "a NIE");
        assert_eq!(nif("ES B12345674").as_deref(), Some("B12345674"), "an entity's");
        assert_eq!((nif("12345678A"), nif("B12345675")), (None, None));
        assert!(outside_vat_area("35001") && outside_vat_area("38002") && outside_vat_area("51001") && !outside_vat_area("28001"));
        assert_eq!(
            (surcharge(Decimal::from(21)), surcharge(Decimal::from(10)), surcharge(Decimal::from(4))),
            (Decimal::new(52, 1), Decimal::new(14, 1), Decimal::new(5, 1))
        );
    }
}
