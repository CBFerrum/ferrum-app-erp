//! The member states' VAT rates from the Commission's TEDB (Taxes in Europe Database), through its
//! VAT Retrieval Service (SOAP, `retrieveVatRates`): every standard, reduced, super-reduced and
//! parking rate on a day, with the goods (CN codes) and services (CPA codes) each reduced rate is
//! for. They are kept as `EuVatRate` rows, each from the day TEDB says it applies; an item's rate
//! in each country is suggested from its CN code, never assumed.

use crate::util::{id, rows, s, sys};
use crate::vat::MEMBER_STATES;
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, Doc, Error, EventSink, HookCtx, Kernel, Result};
use quick_xml::Reader;
use quick_xml::events::Event as Xml;
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;
use std::time::Duration;

pub const TOPIC: &str = "eu.tedb.requested";
const DEFAULT_URL: &str = "https://ec.europa.eu/taxation_customs/tedb/ws/";
const ACTION: &str = "urn:ec.europa.eu:taxud:tedb:services:v1:VatRetrievalService/RetrieveVatRates";

/// TEDB's address: the Commission's, or `FERRUM_EU_TEDB_URL` (a proxy, a test double).
fn url() -> String {
    std::env::var("FERRUM_EU_TEDB_URL").ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| DEFAULT_URL.into())
}

/// The request for every member state's rates on a day.
pub fn envelope(on: &str) -> String {
    let states: String = MEMBER_STATES.iter().map(|ms| format!("<t:isoCode>{}</t:isoCode>", crate::vat::vies_code(ms))).collect();
    format!(
        "<soapenv:Envelope xmlns:soapenv=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         xmlns:m=\"urn:ec.europa.eu:taxud:tedb:services:v1:IVatRetrievalService\" \
         xmlns:t=\"urn:ec.europa.eu:taxud:tedb:services:v1:IVatRetrievalService:types\">\
         <soapenv:Header/><soapenv:Body><m:retrieveVatRatesReqMsg><t:memberStates>{states}</t:memberStates>\
         <t:situationOn>{on}</t:situationOn></m:retrieveVatRatesReqMsg></soapenv:Body></soapenv:Envelope>"
    )
}

/// One of TEDB's rates.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rate {
    pub member_state: String,
    /// STANDARD or REDUCED.
    pub kind: String,
    /// DEFAULT, REDUCED_RATE, SUPER_REDUCED_RATE, PARKING_RATE, EXEMPTED, NOT_APPLICABLE, OUT_OF_SCOPE.
    pub rate_kind: String,
    pub value: Option<Decimal>,
    pub situation_on: String,
    pub category: Option<String>,
    pub comment: Option<String>,
    pub cn_codes: Vec<String>,
    pub cpa_codes: Vec<String>,
}

impl Rate {
    /// Its `EuVatRate.kind`, if it is one Ferrum keeps.
    pub fn kept_kind(&self) -> Option<&'static str> {
        match (self.kind.as_str(), self.rate_kind.as_str()) {
            ("STANDARD", "DEFAULT") => Some("Standard"),
            (_, "REDUCED_RATE") | ("REDUCED", "DEFAULT") => Some("Reduced"),
            (_, "SUPER_REDUCED_RATE") => Some("Super Reduced"),
            (_, "PARKING_RATE") => Some("Parking"),
            (_, "EXEMPTED") => Some("Exempt"),
            _ => None,
        }
    }
}

/// Text without its HTML (TEDB's comments carry some).
fn plain(html: &str) -> String {
    let mut out = String::new();
    let mut tag = false;
    for c in html.chars() {
        match c {
            '<' => tag = true,
            '>' => {
                tag = false;
                out.push(' ');
            }
            _ if !tag => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// TEDB's answer: its rates, or its fault's description.
pub fn parse(xml: &str) -> std::result::Result<Vec<Rate>, String> {
    let mut reader = Reader::from_str(xml);
    let mut path: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut out = Vec::new();
    let mut cur: Option<Rate> = None;
    let mut fault: Vec<String> = Vec::new();
    loop {
        match reader.read_event().map_err(|e| format!("TEDB's answer is not XML: {e}"))? {
            Xml::Start(e) => {
                let name = e.local_name().as_ref().to_string();
                if name == "vatRateResults" {
                    cur = Some(Rate::default());
                }
                path.push(name);
                text.clear();
            }
            Xml::Text(t) => text.push_str(&t.xml10_content()),
            Xml::CData(c) => text.push_str(&c.xml10_content()),
            // Entity and character references come apart from the text around them.
            Xml::GeneralRef(r) => {
                if let Ok(Some(ch)) = r.resolve_char_ref() {
                    text.push(ch);
                } else if let Some(v) = quick_xml::escape::resolve_predefined_entity(&r.xml10_content()) {
                    text.push_str(v);
                }
            }
            Xml::End(_) => {
                let value = std::mem::take(&mut text).trim().to_string();
                let tail: Vec<&str> = path.iter().rev().take(3).map(String::as_str).collect();
                let (last, parent, grand) = (tail.first().copied().unwrap_or(""), tail.get(1).copied().unwrap_or(""), tail.get(2).copied().unwrap_or(""));
                if let Some(r) = cur.as_mut() {
                    match (grand, parent, last) {
                        (_, "vatRateResults", "memberState") => r.member_state = value,
                        (_, "vatRateResults", "type") => r.kind = value,
                        (_, "vatRateResults", "situationOn") => r.situation_on = value.chars().take(10).collect(),
                        (_, "vatRateResults", "comment") => r.comment = Some(plain(&value)).filter(|c| !c.is_empty()),
                        (_, "rate", "type") => r.rate_kind = value,
                        (_, "rate", "value") => r.value = Decimal::from_str(&value).ok(),
                        ("cnCodes", "code", "value") => r.cn_codes.push(value),
                        ("cpaCodes", "code", "value") => r.cpa_codes.push(value),
                        (_, "category", "description") => r.category = Some(plain(&value)).filter(|c| !c.is_empty()),
                        _ => {}
                    }
                } else if parent == "error" && last == "description" {
                    fault.push(value);
                }
                if path.pop().as_deref() == Some("vatRateResults")
                    && let Some(r) = cur.take()
                {
                    out.push(r);
                }
            }
            Xml::Eof => break,
            _ => {}
        }
    }
    if out.is_empty() && !fault.is_empty() {
        return Err(format!("TEDB refused: {}", fault.join("; ")));
    }
    Ok(out)
}

/// The digits of a CN code ("0106 41 00" → "01064100").
fn cn_digits(code: &str) -> String {
    code.chars().filter(char::is_ascii_digit).collect()
}

pub struct Tedb {
    client: reqwest::Client,
}

impl Default for Tedb {
    fn default() -> Self {
        Tedb { client: reqwest::Client::builder().timeout(Duration::from_secs(120)).build().unwrap_or_default() }
    }
}

/// Keeps TEDB's rates the workspace does not have yet (by state, kind, rate, day and category).
pub async fn store(k: &Kernel, ctx: &Ctx, rates: &[Rate]) -> Result<usize> {
    let table = ferrum_kernel::ddl::q(&ferrum_meta::table_for("EuVatRate"));
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let have: Vec<(String, String, Decimal, NaiveDate, Option<String>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!("select member_state, \"kind\", rate, valid_from, category from {table} where tenant_id = $1")))
            .bind(ctx.tenant)
            .fetch_all(&mut *tx)
            .await?;
    let mut seen: HashSet<(String, String, Decimal, String, String)> =
        have.into_iter().map(|(m, kd, r, d, c)| (m, kd, r.normalize(), d.format("%Y-%m-%d").to_string(), c.unwrap_or_default())).collect();
    let mut added = 0;
    for r in rates {
        let Some(kind) = r.kept_kind() else { continue };
        let value = if kind == "Exempt" {
            Decimal::ZERO
        } else if let Some(v) = r.value {
            v
        } else {
            continue;
        };
        if r.member_state.len() != 2 || NaiveDate::parse_from_str(&r.situation_on, "%Y-%m-%d").is_err() {
            continue;
        }
        let category: String = r.category.clone().unwrap_or_default().chars().take(500).collect();
        if !seen.insert((r.member_state.clone(), kind.to_string(), value.normalize(), r.situation_on.clone(), category.clone())) {
            continue;
        }
        let doc = json!({
            "member_state": r.member_state, "kind": kind, "rate": value.to_string(), "valid_from": r.situation_on,
            "category": Some(category).filter(|c| !c.is_empty()), "comment": r.comment,
            "cn_codes": Some(r.cn_codes.join("\n")).filter(|c| !c.is_empty()), "cpa_codes": Some(r.cpa_codes.join("\n")).filter(|c| !c.is_empty()),
            "source": format!("TEDB, {}", r.situation_on),
        });
        if let Value::Object(doc) = doc {
            k.insert_tx(&sys(ctx), &mut tx, "EuVatRate", doc).await?;
            added += 1;
        }
    }
    tx.commit().await?;
    Ok(added)
}

#[async_trait]
impl EventSink for Tedb {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic != TOPIC {
            return Ok(());
        }
        let ctx = Ctx::system(ev.tenant_id, k.registry.schema(ev.tenant_id).await?);
        if !ctx.schema.packages.iter().any(|p| p == crate::PACKAGE) {
            return Ok(());
        }
        let on = ev.payload["situation_on"].as_str().filter(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok()).unwrap_or_default().to_string();
        let on = if on.is_empty() { ctx.today().format("%Y-%m-%d").to_string() } else { on };
        let reply = self
            .client
            .post(url())
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("SOAPAction", ACTION)
            .body(envelope(&on))
            .send()
            .await
            .map_err(|e| Error::internal(format!("TEDB could not be reached: {e}")))?;
        let status = reply.status();
        let body = reply.text().await.map_err(|e| Error::internal(format!("TEDB's answer broke off: {e}")))?;
        let rates = match parse(&body) {
            Ok(r) if !r.is_empty() => r,
            Ok(_) => return Err(Error::internal(format!("TEDB ({status}) sent no rates"))),
            Err(e) => return Err(Error::internal(format!("TEDB ({status}): {e}"))),
        };
        let added = store(k, &ctx, &rates).await?;
        tracing::info!(tenant = %ev.tenant_id, added, on, "EU VAT rates from TEDB");
        Ok(())
    }
}

/// Fills an item's rate in every member state from its CN code: the rate of TEDB's most specific
/// code that covers it (the latest by today), the state's standard rate where no reduced rate
/// covers it; states where codes of the same length give different rates are left to the user.
pub(crate) async fn suggest(hc: &mut HookCtx<'_>, item: &Doc) -> Result<Value> {
    let cn = cn_digits(s(item, "eu_cn_code").unwrap_or_default());
    if cn.len() < 2 {
        return Err(Error::field("eu_cn_code", "needs at least its chapter (two digits)"));
    }
    let today = hc.ctx.today();
    let table = ferrum_kernel::ddl::q(&ferrum_meta::table_for("EuVatRate"));
    let all: Vec<(String, String, Decimal, NaiveDate, Option<String>, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select member_state, \"kind\", rate, valid_from, category, cn_codes from {table} where tenant_id = $1 and valid_from <= $2 order by valid_from"
    )))
    .bind(hc.ctx.tenant)
    .bind(today)
    .fetch_all(&mut **hc.tx)
    .await?;
    // Per state: (code length, day) → rates and their categories; and its standard rate.
    let mut best: BTreeMap<String, (usize, NaiveDate, Vec<(Decimal, String)>)> = BTreeMap::new();
    let mut standard: BTreeMap<String, (NaiveDate, Decimal)> = BTreeMap::new();
    for (ms, kind, rate, day, category, codes) in &all {
        if kind == "Standard" {
            let e = standard.entry(ms.clone()).or_insert((*day, *rate));
            if *day > e.0 || (*day == e.0 && *rate > e.1) {
                *e = (*day, *rate);
            }
            continue;
        }
        let Some(len) =
            codes.as_deref().unwrap_or_default().lines().map(cn_digits).filter(|c| !c.is_empty() && cn.starts_with(c.as_str())).map(|c| c.len()).max()
        else {
            continue;
        };
        let label = format!("{kind}: {}", category.clone().unwrap_or_default());
        let e = best.entry(ms.clone()).or_insert((len, *day, Vec::new()));
        if (len, *day) > (e.0, e.1) {
            *e = (len, *day, vec![(*rate, label)]);
        } else if (len, *day) == (e.0, e.1) && !e.2.iter().any(|(r, _)| r == rate) {
            e.2.push((*rate, label));
        }
    }
    let mut suggested: BTreeMap<String, (Decimal, String)> = BTreeMap::new();
    let mut ambiguous = Vec::new();
    for ms in MEMBER_STATES.iter().map(|m| crate::vat::vies_code(m).to_string()) {
        match best.get(&ms) {
            Some((_, day, found)) if found.len() == 1 => {
                let (rate, label) = &found[0];
                suggested.insert(ms.clone(), (*rate, format!("TEDB {day}, CN {cn}: {label}").chars().take(500).collect()));
            }
            Some(_) => ambiguous.push(ms.clone()),
            None => {
                if let Some((day, rate)) = standard.get(&ms) {
                    suggested.insert(ms.clone(), (*rate, format!("TEDB {day}: standard (no reduced rate covers CN {cn})")));
                }
            }
        }
    }
    let mut kept: Vec<Value> = rows(item, "eu_rates")
        .into_iter()
        .filter(|r| s(r, "member_state").is_some_and(|m| !suggested.contains_key(m)))
        .map(|r| json!({"member_state": s(&r, "member_state"), "rate": r.get("rate"), "note": s(&r, "note")}))
        .collect();
    kept.extend(suggested.iter().map(|(ms, (rate, note))| json!({"member_state": ms, "rate": rate.to_string(), "note": note})));
    let iid = id(item, "id").ok_or_else(|| Error::internal("item without id"))?;
    let patch = json!({"eu_rates": kept});
    if let Value::Object(p) = patch {
        hc.kernel.update_tx(&sys(hc.ctx), hc.tx, "Item", iid, p, None).await?;
    }
    Ok(json!({"suggested": suggested.keys().collect::<Vec<_>>(), "ambiguous": ambiguous}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of TEDB's answers (VatRetrievalServiceType.xsd), as the service sends them.
    pub const SAMPLE: &str = r#"<env:Envelope xmlns:env="http://schemas.xmlsoap.org/soap/envelope/"><env:Header/><env:Body><ns0:retrieveVatRatesRespMsg xmlns="urn:ec.europa.eu:taxud:tedb:services:v1:IVatRetrievalService:types" xmlns:ns0="urn:ec.europa.eu:taxud:tedb:services:v1:IVatRetrievalService"><additionalInformation/>
<vatRateResults><memberState>FR</memberState><type>STANDARD</type><rate><type>DEFAULT</type><value>20.0</value></rate><situationOn>2026-07-01+02:00</situationOn><comment>&lt;p&gt;Art. 278 CGI&lt;/p&gt;</comment></vatRateResults>
<vatRateResults><memberState>FR</memberState><type>REDUCED</type><rate><type>REDUCED_RATE</type><value>5.5</value></rate><situationOn>2026-07-01+02:00</situationOn><cnCodes><code><value>4901</value><description>Printed books</description></code></cnCodes><category><identifier>BOOKS</identifier><description>Supply, including on loan by libraries, of books</description></category></vatRateResults>
<vatRateResults><memberState>ES</memberState><type>STANDARD</type><rate><type>DEFAULT</type><value>7.0</value></rate><situationOn>2026-07-01+02:00</situationOn><comment>VAT - Canary Islands - </comment></vatRateResults>
<vatRateResults><memberState>DE</memberState><type>REDUCED</type><rate><type>EXEMPTED</type></rate><situationOn>2026-07-01+02:00</situationOn></vatRateResults>
</ns0:retrieveVatRatesRespMsg></env:Body></env:Envelope>"#;

    #[test]
    fn tedb_answers_parse() {
        let rates = parse(SAMPLE).unwrap();
        assert_eq!(rates.len(), 4);
        assert_eq!((rates[0].member_state.as_str(), rates[0].kept_kind(), rates[0].value), ("FR", Some("Standard"), Some(Decimal::new(200, 1))));
        assert_eq!((rates[0].situation_on.as_str(), rates[0].comment.as_deref()), ("2026-07-01", Some("Art. 278 CGI")));
        assert_eq!(
            (rates[1].kept_kind(), rates[1].cn_codes.clone(), rates[1].category.as_deref()),
            (Some("Reduced"), vec!["4901".to_string()], Some("Supply, including on loan by libraries, of books"))
        );
        assert_eq!((rates[3].kept_kind(), rates[3].value), (Some("Exempt"), None));
        let fault = "<Envelope><Body><Fault><detail><retrieveVatRatesFaultMsg><error><code>TEDB-100</code><description>Invalid date</description></error></retrieveVatRatesFaultMsg></detail></Fault></Body></Envelope>";
        assert_eq!(parse(fault), Err("TEDB refused: Invalid date".into()));
        assert!(parse("<a><b>").is_err() || parse("<a><b>").unwrap().is_empty());
        assert_eq!(cn_digits("0106 41 00"), "01064100");
        assert!(envelope("2026-10-31").contains("<t:isoCode>EL</t:isoCode>") && envelope("2026-10-31").contains("<t:situationOn>2026-10-31</t:situationOn>"));
    }
}
