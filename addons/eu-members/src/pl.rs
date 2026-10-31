//! Poland: on the EU's VAT (23%, 8%, 5% from TEDB), invoices are issued in KSeF (the National
//! e-Invoice System: large taxpayers since 1 February 2026, the rest since 1 April 2026, the
//! smallest from 1 January 2027) as structured invoices in schema FA(3). A submitted invoice's
//! FA(3) is written from the EU's EN 16931 model with the Polish rates' codes (23, 8, 5, 0 KR,
//! 0 WDT, 0 EX, zw, np I, np II, oo) and amounts by rate (P_13_x, P_14_x), and queued for the
//! company's KSeF channel through the outbox; the channel records the KSeF number. Corrections
//! (credit notes) are KOR invoices citing the corrected one. NIPs are checked (weights 6, 5, 7,
//! 2, 3, 4, 5, 6, 7, modulo 11). Small businesses under the exemption of article 113 charge no
//! VAT; the domestic reverse charge is the item's.

use crate::member::{Member, Rules};
use crate::util::{d, get, id, rows, s, sys};
use async_trait::async_trait;
use chrono::Utc;
use ferrum_eu::ubl::invoice;
use ferrum_eu::vat::{INPUT_TAX, OSS_VAT, OUTPUT_TAX};
use ferrum_kernel::{Doc, DocHook, Error, FieldError, Filter, HookCtx, Result, outbox};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::{Value, json};
use std::fmt::Write;

pub const SEND_TOPIC: &str = "pl.ksef.send_requested";
pub const NAMESPACE: &str = "http://crd.gov.pl/wzor/2025/06/25/13775/";

pub static MEMBER: Member = Member {
    package: "pl",
    country: "PL",
    small: Some((
        "pl_small_business",
        "Zwolnienie podmiotowe (art. 113 ust. 1 ustawy o VAT)",
        "Sprzedaż zwolniona z VAT na podstawie art. 113 ust. 1 ustawy o VAT.",
    )),
    reverse: Some(("pl_reverse_charge", "Odwrotne obciążenie.")),
};

/// A NIP: ten digits, the first nine weighted 6, 5, 7, 2, 3, 4, 5, 6, 7, modulo 11, the tenth.
pub fn nip(raw: &str) -> Option<String> {
    let n: String = raw.trim().trim_start_matches(['P', 'L', 'p', 'l']).chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    if n.len() != 10 || !n.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let d: Vec<u32> = n.bytes().map(|c| u32::from(c - b'0')).collect();
    let sum = d[..9].iter().zip([6, 5, 7, 2, 3, 4, 5, 6, 7]).map(|(a, w)| a * w).sum::<u32>() % 11;
    (sum != 10 && sum == d[9]).then_some(n)
}

/// A line's rate code (P_12) from its EN 16931 category and rate and where the buyer is.
pub fn rate_code(category: &str, rate: Decimal, buyer_home: bool) -> &'static str {
    match category {
        "S" if rate >= Decimal::from(22) => "23",
        "S" if rate >= Decimal::from(7) => "8",
        "S" => "5",
        "K" => "0 WDT",
        "G" => "0 EX",
        "E" => "zw",
        "AE" if buyer_home => "oo",
        "AE" => "np II",
        "O" => "np I",
        _ => "0 KR",
    }
}

/// The FA(3) amount fields a rate code's net and VAT go to.
pub fn fields(code: &str) -> (&'static str, Option<&'static str>) {
    match code {
        "23" | "22" => ("P_13_1", Some("P_14_1")),
        "8" | "7" => ("P_13_2", Some("P_14_2")),
        "5" => ("P_13_3", Some("P_14_3")),
        "0 KR" => ("P_13_6_1", None),
        "0 WDT" => ("P_13_6_2", None),
        "0 EX" => ("P_13_6_3", None),
        "zw" => ("P_13_7", None),
        "np I" => ("P_13_8", None),
        "np II" => ("P_13_9", None),
        _ => ("P_13_10", None),
    }
}

fn esc(v: &str) -> String {
    v.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn r(v: Decimal, dp: u32) -> String {
    let mut x = v.round_dp_with_strategy(dp, RoundingStrategy::MidpointAwayFromZero);
    x.rescale(dp);
    x.to_string()
}

/// How the buyer is identified.
#[derive(Debug, Clone, PartialEq)]
pub enum BuyerId {
    Nip(String),
    Eu(String, String),
    Other(String, String),
    None,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub name: String,
    pub unit: String,
    pub quantity: Decimal,
    pub price: Decimal,
    pub net: Decimal,
    pub code: &'static str,
}

/// An FA(3) invoice's content.
#[derive(Debug, Clone)]
pub struct Faktura {
    pub seller_nip: String,
    pub seller_name: String,
    pub seller_address: (String, String, String),
    pub buyer: BuyerId,
    pub buyer_name: String,
    pub buyer_address: Option<(String, String, String)>,
    pub currency: String,
    pub date: String,
    pub number: String,
    pub lines: Vec<Line>,
    /// VAT by amount field (P_14_x).
    pub vat: Vec<(&'static str, Decimal)>,
    pub total: Decimal,
    pub exemption: Option<String>,
    /// A correction: the corrected invoice's (number, date, KSeF number).
    pub corrected: Option<(String, String, Option<String>)>,
    pub due: Option<String>,
    pub account: Option<String>,
    pub created: String,
}

impl Faktura {
    /// Net amounts by field (P_13_x), in the schema's order.
    pub fn nets(&self) -> Vec<(&'static str, Decimal)> {
        let order = ["P_13_1", "P_13_2", "P_13_3", "P_13_6_1", "P_13_6_2", "P_13_6_3", "P_13_7", "P_13_8", "P_13_9", "P_13_10"];
        order
            .iter()
            .filter_map(|f| {
                let lines: Vec<&Line> = self.lines.iter().filter(|l| fields(l.code).0 == *f).collect();
                (!lines.is_empty()).then(|| (*f, lines.iter().map(|l| l.net).sum()))
            })
            .collect()
    }
}

/// What KSeF's FA(3) checks a file written here could fail: the problems, empty when it passes.
pub fn validate(f: &Faktura) -> Vec<String> {
    let mut p = Vec::new();
    let mut rule = |ok: bool, what: &str| {
        if !ok {
            p.push(what.to_string());
        }
    };
    rule(nip(&f.seller_nip).is_some(), "the company's NIP (its EU VAT number, PL and 10 digits whose check holds)");
    rule(!f.seller_address.1.is_empty(), "the company's address (street, postal code and town)");
    rule(!f.lines.is_empty(), "the invoice has lines");
    rule(!matches!(&f.buyer, BuyerId::Nip(n) if nip(n).is_none()), "the customer's NIP holds its check digit");
    rule(f.lines.iter().all(|l| l.code != "zw") || f.exemption.is_some(), "an exempt sale names its legal basis (P_19A)");
    let sum: Decimal = f.nets().iter().map(|(_, v)| v).sum::<Decimal>() + f.vat.iter().map(|(_, v)| v).sum::<Decimal>();
    rule((sum - f.total).abs() <= Decimal::new(1, 2), "the total (P_15) is the nets and VAT by rate");
    p
}

/// The FA(3) XML.
pub fn to_xml(f: &Faktura) -> String {
    let mut x = String::new();
    let _ = write!(
        x,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Faktura xmlns=\"{NAMESPACE}\"><Naglowek><KodFormularza kodSystemowy=\"FA (3)\" wersjaSchemy=\"1-0E\">FA</KodFormularza><WariantFormularza>3</WariantFormularza><DataWytworzeniaFa>{}</DataWytworzeniaFa><SystemInfo>Ferrum</SystemInfo></Naglowek>",
        esc(&f.created)
    );
    let address = |x: &mut String, (country, l1, l2): &(String, String, String)| {
        let _ = write!(x, "<Adres><KodKraju>{}</KodKraju><AdresL1>{}</AdresL1>", esc(country), esc(l1));
        if !l2.is_empty() {
            let _ = write!(x, "<AdresL2>{}</AdresL2>", esc(l2));
        }
        x.push_str("</Adres>");
    };
    let _ = write!(x, "<Podmiot1><DaneIdentyfikacyjne><NIP>{}</NIP><Nazwa>{}</Nazwa></DaneIdentyfikacyjne>", esc(&f.seller_nip), esc(&f.seller_name));
    address(&mut x, &f.seller_address);
    x.push_str("</Podmiot1><Podmiot2><DaneIdentyfikacyjne>");
    match &f.buyer {
        BuyerId::Nip(n) => {
            let _ = write!(x, "<NIP>{}</NIP>", esc(n));
        }
        BuyerId::Eu(c, v) => {
            let _ = write!(x, "<KodUE>{}</KodUE><NrVatUE>{}</NrVatUE>", esc(c), esc(v));
        }
        BuyerId::Other(c, v) => {
            let _ = write!(x, "<KodKraju>{}</KodKraju><NrID>{}</NrID>", esc(c), esc(v));
        }
        BuyerId::None => x.push_str("<BrakID>1</BrakID>"),
    }
    let _ = write!(x, "<Nazwa>{}</Nazwa></DaneIdentyfikacyjne>", esc(&f.buyer_name));
    if let Some(a) = &f.buyer_address {
        address(&mut x, a);
    }
    x.push_str("<JST>2</JST><GV>2</GV></Podmiot2>");
    let _ = write!(x, "<Fa><KodWaluty>{}</KodWaluty><P_1>{}</P_1><P_2>{}</P_2>", esc(&f.currency), esc(&f.date), esc(&f.number));
    let nets = f.nets();
    for (field, net) in &nets {
        let _ = write!(x, "<{field}>{}</{field}>", r(*net, 2));
        let vat_field = match *field {
            "P_13_1" => Some("P_14_1"),
            "P_13_2" => Some("P_14_2"),
            "P_13_3" => Some("P_14_3"),
            _ => None,
        };
        if let Some(vf) = vat_field {
            let vat = f.vat.iter().filter(|(fld, _)| *fld == vf).map(|(_, v)| *v).sum::<Decimal>();
            let _ = write!(x, "<{vf}>{}</{vf}>", r(vat, 2));
        }
    }
    let _ = write!(x, "<P_15>{}</P_15>", r(f.total, 2));
    let reverse = f.lines.iter().any(|l| l.code == "oo");
    let _ = write!(x, "<Adnotacje><P_16>2</P_16><P_17>2</P_17><P_18>{}</P_18><P_18A>2</P_18A>", if reverse { 1 } else { 2 });
    match &f.exemption {
        Some(basis) if f.lines.iter().any(|l| l.code == "zw") => {
            let _ = write!(x, "<Zwolnienie><P_19>1</P_19><P_19A>{}</P_19A></Zwolnienie>", esc(&basis.chars().take(256).collect::<String>()));
        }
        _ => x.push_str("<Zwolnienie><P_19N>1</P_19N></Zwolnienie>"),
    }
    x.push_str("<NoweSrodkiTransportu><P_22N>1</P_22N></NoweSrodkiTransportu><P_23>2</P_23><PMarzy><P_PMarzyN>1</P_PMarzyN></PMarzy></Adnotacje>");
    match &f.corrected {
        Some((number, date, ksef)) => {
            let _ = write!(
                x,
                "<RodzajFaktury>KOR</RodzajFaktury><DaneFaKorygowanej><DataWystFaKorygowanej>{}</DataWystFaKorygowanej><NrFaKorygowanej>{}</NrFaKorygowanej>",
                esc(date),
                esc(number)
            );
            match ksef {
                Some(k) => {
                    let _ = write!(x, "<NrKSeF>1</NrKSeF><NrKSeFFaKorygowanej>{}</NrKSeFFaKorygowanej>", esc(k));
                }
                None => x.push_str("<NrKSeFN>1</NrKSeFN>"),
            }
            x.push_str("</DaneFaKorygowanej>");
        }
        None => x.push_str("<RodzajFaktury>VAT</RodzajFaktury>"),
    }
    for (i, l) in f.lines.iter().enumerate() {
        let _ = write!(
            x,
            "<FaWiersz><NrWierszaFa>{}</NrWierszaFa><P_7>{}</P_7><P_8A>{}</P_8A><P_8B>{}</P_8B><P_9A>{}</P_9A><P_11>{}</P_11><P_12>{}</P_12></FaWiersz>",
            i + 1,
            esc(&l.name),
            esc(&l.unit),
            r(l.quantity, 6),
            r(l.price, 8),
            r(l.net, 2),
            l.code
        );
    }
    if f.due.is_some() || f.account.is_some() {
        x.push_str("<Platnosc>");
        if let Some(due) = &f.due {
            let _ = write!(x, "<TerminPlatnosci><Termin>{}</Termin></TerminPlatnosci>", esc(due));
        }
        x.push_str("<FormaPlatnosci>6</FormaPlatnosci>");
        if let Some(acc) = &f.account {
            let _ = write!(x, "<RachunekBankowy><NrRB>{}</NrRB></RachunekBankowy>", esc(acc));
        }
        x.push_str("</Platnosc>");
    }
    x.push_str("</Fa></Faktura>");
    x
}

/// A submitted sales invoice's FA(3).
pub async fn faktura(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Faktura> {
    let inv = invoice(hc, doc).await?;
    let ctx = sys(hc.ctx);
    let company = get(hc.kernel, &ctx, hc.tx, "Company", id(doc, "company").ok_or_else(|| Error::internal("invoice without company"))?).await?;
    let cid = id(&company, "id").unwrap_or_default();
    let mut vat_accounts = Vec::new();
    for n in [OUTPUT_TAX, INPUT_TAX, OSS_VAT] {
        let a = crate::util::find(hc.kernel, &ctx, hc.tx, "Account", vec![Filter::eq("company", cid.to_string()), Filter::eq("account_number", n)], None, 1)
            .await?;
        vat_accounts.extend(a.first().and_then(|a| s(a, "id")).map(str::to_string));
    }
    let buyer_country = inv.buyer.country.clone().unwrap_or_else(|| "PL".into());
    let buyer_home = buyer_country == "PL";
    // A correction states the differences: negative for a credit note.
    let sign = if inv.credit_note { -Decimal::ONE } else { Decimal::ONE };
    let mut lines = Vec::new();
    for row in rows(doc, "items") {
        let account = vat_accounts.iter().find(|a| row.get("item_tax_rates").and_then(|m| m.get(a.as_str())).is_some()).cloned();
        let rate: Decimal = account.as_ref().and_then(|a| row["item_tax_rates"][a.as_str()].as_str()).and_then(|v| v.parse().ok()).unwrap_or_default();
        let category = account.as_ref().and_then(|a| row["tax_categories"][a.as_str()]["category"].as_str()).map(str::to_string);
        let code = rate_code(ferrum_eu::ubl::category_code(category.as_deref(), rate), rate, buyer_home);
        let qty = d(&row, "qty");
        let net = d(&row, "net_amount");
        let unit = match id(&row, "uom") {
            Some(u) => s(&get(hc.kernel, &ctx, hc.tx, "Uom", u).await?, "uom_name").unwrap_or("szt.").to_string(),
            None => "szt.".into(),
        };
        lines.push(Line {
            name: s(&row, "item_name").unwrap_or("-").chars().take(512).collect(),
            unit,
            quantity: qty,
            price: if qty.is_zero() { net } else { (net / qty).abs() },
            net,
            code,
        });
    }
    let vat: Vec<(&'static str, Decimal)> =
        inv.subtotals.iter().filter(|t| t.category == "S").filter_map(|t| fields(rate_code("S", t.percent, buyer_home)).1.map(|f| (f, sign * t.tax))).collect();
    let seller_nip = s(&company, "eu_vat_number").and_then(nip).ok_or_else(|| Error::invalid("set the company's NIP (its EU VAT number, PL and 10 digits)"))?;
    let buyer_vat = s(doc, "eu_customer_vat").map(str::to_string).or(inv.buyer.vat.clone());
    let buyer = match buyer_vat {
        Some(v) if buyer_home => nip(&v).map(BuyerId::Nip).unwrap_or(BuyerId::None),
        Some(v) if v.len() > 2 && v[..2].chars().all(|c| c.is_ascii_alphabetic()) && ferrum_eu::vat::vat_country(&v).is_some() => {
            BuyerId::Eu(v[..2].to_uppercase(), v[2..].to_string())
        }
        Some(v) => BuyerId::Other(buyer_country.clone(), v),
        None => BuyerId::None,
    };
    let line2 = |postal: &Option<String>, city: &Option<String>| [postal.clone(), city.clone()].into_iter().flatten().collect::<Vec<_>>().join(" ");
    let exemption = inv
        .subtotals
        .iter()
        .find(|t| t.category == "E")
        .map(|t| t.reason.clone().or(t.reason_code.clone()).unwrap_or_else(|| "art. 43 ust. 1 ustawy o VAT".into()));
    let corrected = match &inv.original {
        Some((number, date)) => {
            let orig = match id(doc, "return_against") {
                Some(o) => s(&get(hc.kernel, &ctx, hc.tx, "SalesInvoice", o).await?, "pl_ksef_number").map(str::to_string),
                None => None,
            };
            Some((number.clone(), date.clone(), orig))
        }
        None => None,
    };
    let f = Faktura {
        seller_nip,
        seller_name: inv.seller.name.clone(),
        seller_address: ("PL".into(), inv.seller.street.clone().unwrap_or_default(), line2(&inv.seller.postal, &inv.seller.city)),
        buyer,
        buyer_name: inv.buyer.name.clone(),
        buyer_address: inv.buyer.street.clone().map(|street| (buyer_country.clone(), street, line2(&inv.buyer.postal, &inv.buyer.city))),
        currency: inv.currency.clone(),
        date: inv.issue_date.clone(),
        number: inv.number.clone(),
        lines,
        vat,
        total: sign * inv.tax_inclusive,
        exemption,
        corrected,
        due: inv.due_date.clone(),
        account: inv.payment.as_ref().and_then(|p| p.iban.clone()).map(|i| i.trim_start_matches("PL").to_string()),
        created: Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    };
    let problems = validate(&f);
    if !problems.is_empty() {
        return Err(Error::invalid(format!("the FA(3) invoice would be refused: {}", problems.join("; "))));
    }
    Ok(f)
}

/// FA(3) on submitted invoices: download, send to KSeF (through the outbox), record its number.
pub struct KsefHook;

impl KsefHook {
    async fn save(hc: &mut HookCtx<'_>, doc: &Doc, values: Value) -> Result<()> {
        let inv = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let Value::Object(v) = values else { return Ok(()) };
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", inv, v).await
    }
}

/// A KSeF number: the seller's NIP, the day, twelve hexadecimal characters and a CRC-8 (polynomial
/// 0x07) of what comes before, as two.
pub fn ksef_number_ok(n: &str) -> bool {
    let parts: Vec<&str> = n.split('-').collect();
    if parts.len() != 4 || parts[0].len() != 10 || parts[1].len() != 8 || parts[2].len() != 12 || parts[3].len() != 2 {
        return false;
    }
    let body = &n[..n.len() - 3];
    let mut crc: u8 = 0;
    for b in body.bytes() {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 { (crc << 1) ^ 0x07 } else { crc << 1 };
        }
    }
    format!("{crc:02X}") == parts[3]
}

#[async_trait]
impl DocHook for KsefHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.field("eu_vat_number").is_some()
            && let Some(v) = s(doc, "eu_vat_number").map(str::to_uppercase).filter(|v| v.starts_with("PL"))
            && nip(&v).is_none()
        {
            return Err(Error::Validation(vec![FieldError::new("eu_vat_number", "is not a Polish NIP: PL and ten digits whose check digit holds")]));
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.entity.def.name != "SalesInvoice" || hc.entity.field("pl_ksef_status").is_none() {
            return Ok(());
        }
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let company = get(hc.kernel, &sys(hc.ctx), hc.tx, "Company", cid).await?;
        if s(&company, "country_code") == Some("PL") && crate::util::b(&company, "pl_ksef_on_submit") {
            send(hc, doc).await?;
        }
        Ok(())
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if matches!(s(doc, "pl_ksef_status"), Some("Pending" | "Accepted")) {
            return Err(Error::invalid("KSeF has this invoice: issue a correction (a credit note) instead of cancelling it"));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if hc.entity.field("pl_ksef_status").is_none() || !name.starts_with("pl_") {
            return Ok(None);
        }
        match name {
            "pl_fa3" => {
                let f = faktura(hc, doc).await?;
                let file = format!("{}.xml", f.number.replace(['/', '\\'], "-"));
                Ok(Some(json!({"file": {"name": file, "type": "application/xml", "content": to_xml(&f)}})))
            }
            "pl_send_ksef" => send(hc, doc).await.map(Some),
            "pl_record_ksef" => {
                if s(doc, "pl_ksef_status") != Some("Pending") {
                    return Err(Error::invalid("only an invoice waiting for KSeF is recorded"));
                }
                match params.get("ksef_number").and_then(Value::as_str).map(str::trim) {
                    Some(n) if ksef_number_ok(n) => {
                        Self::save(hc, doc, json!({"pl_ksef_status": "Accepted", "pl_ksef_number": n, "pl_ksef_error": null})).await?;
                        Ok(Some(json!({"status": "Accepted"})))
                    }
                    Some(_) => Err(Error::field("ksef_number", "is not a KSeF number (its check characters do not hold)")),
                    None => {
                        let why = params.get("message").and_then(Value::as_str).unwrap_or("KSeF refused the invoice").chars().take(2000).collect::<String>();
                        Self::save(hc, doc, json!({"pl_ksef_status": "Rejected", "pl_ksef_error": why})).await?;
                        Ok(Some(json!({"status": "Rejected"})))
                    }
                }
            }
            _ => Ok(None),
        }
    }
}

async fn send(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
    if matches!(s(doc, "pl_ksef_status"), Some("Pending" | "Accepted")) {
        return Err(Error::invalid(format!("the invoice is already {}", s(doc, "pl_ksef_status").unwrap_or_default().to_lowercase())));
    }
    let f = faktura(hc, doc).await?;
    let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
    let event = json!({"invoice": iid, "company": s(doc, "company"), "number": f.number, "nip": f.seller_nip, "xml": to_xml(&f)});
    KsefHook::save(hc, doc, json!({"pl_ksef_status": "Pending", "pl_ksef_error": null})).await?;
    outbox::enqueue(hc.tx, hc.ctx.tenant, SEND_TOPIC, Some("SalesInvoice"), Some(iid), event).await?;
    Ok(json!({"status": "Pending"}))
}

pub fn rules() -> Rules {
    Rules(&MEMBER)
}

fn hooks(h: &mut ferrum_kernel::HookRegistry) {
    for entity in ["Company", "Customer", "Supplier", "SalesInvoice"] {
        h.register(entity, std::sync::Arc::new(KsefHook));
    }
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    Vec::new()
}

crate::app!("pl");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nips_codes_and_ksef_numbers() {
        assert_eq!(nip("PL 526-587-76-35").as_deref(), Some("5265877635"));
        assert_eq!(nip("5265877636"), None);
        assert_eq!(
            (rate_code("S", Decimal::from(23), true), rate_code("S", Decimal::from(8), true), rate_code("K", Decimal::ZERO, false)),
            ("23", "8", "0 WDT")
        );
        assert_eq!((rate_code("AE", Decimal::ZERO, true), rate_code("AE", Decimal::ZERO, false), rate_code("E", Decimal::ZERO, true)), ("oo", "np II", "zw"));
        assert_eq!(fields("np II"), ("P_13_9", None));
        assert!(ksef_number_ok("5265877635-20250826-0100001AF629-AF"), "KSeF's own example");
        assert!(!ksef_number_ok("5265877635-20250826-0100001AF629-AE"));
    }

    #[test]
    fn an_fa3_as_ksef_takes_it() {
        let f = Faktura {
            seller_nip: "5265877635".into(),
            seller_name: "Firma Sp. z o.o.".into(),
            seller_address: ("PL".into(), "ul. Prosta 1".into(), "00-001 Warszawa".into()),
            buyer: BuyerId::Eu("DE".into(), "123456789".into()),
            buyer_name: "Kunde GmbH".into(),
            buyer_address: Some(("DE".into(), "Hauptstr. 1".into(), "10115 Berlin".into())),
            currency: "PLN".into(),
            date: "2026-10-31".into(),
            number: "FV/1/2026".into(),
            lines: vec![
                Line { name: "Towar".into(), unit: "szt.".into(), quantity: Decimal::from(2), price: Decimal::from(50), net: Decimal::from(100), code: "23" },
                Line { name: "Usługa".into(), unit: "h".into(), quantity: Decimal::ONE, price: Decimal::from(200), net: Decimal::from(200), code: "np II" },
            ],
            vat: vec![("P_14_1", Decimal::from(23))],
            total: Decimal::from(323),
            exemption: None,
            corrected: None,
            due: Some("2026-11-14".into()),
            account: Some("61109010140000071219812874".into()),
            created: "2026-10-31T10:00:00Z".into(),
        };
        assert!(validate(&f).is_empty(), "{:?}", validate(&f));
        let x = to_xml(&f);
        for part in [
            "<Faktura xmlns=\"http://crd.gov.pl/wzor/2025/06/25/13775/\"><Naglowek><KodFormularza kodSystemowy=\"FA (3)\" wersjaSchemy=\"1-0E\">FA</KodFormularza><WariantFormularza>3</WariantFormularza>",
            "<Podmiot1><DaneIdentyfikacyjne><NIP>5265877635</NIP>",
            "<KodUE>DE</KodUE><NrVatUE>123456789</NrVatUE>",
            "<JST>2</JST><GV>2</GV></Podmiot2>",
            "<P_13_1>100.00</P_13_1><P_14_1>23.00</P_14_1><P_13_9>200.00</P_13_9><P_15>323.00</P_15>",
            "<P_18>2</P_18>",
            "<Zwolnienie><P_19N>1</P_19N></Zwolnienie>",
            "<RodzajFaktury>VAT</RodzajFaktury>",
            "<P_12>np II</P_12>",
            "<TerminPlatnosci><Termin>2026-11-14</Termin></TerminPlatnosci><FormaPlatnosci>6</FormaPlatnosci><RachunekBankowy><NrRB>61109010140000071219812874</NrRB>",
        ] {
            assert!(x.contains(part), "{part}\nin {x}");
        }
        let wrong = Faktura { total: Decimal::from(300), ..f };
        assert_eq!(validate(&wrong).len(), 1);
    }
}
