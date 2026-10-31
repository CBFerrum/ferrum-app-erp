//! Malaysia's e-invoices through LHDN's MyInvois (sdk.myinvois.hasil.gov.my): a submitted sales
//! invoice (or credit note) of a company that uses MyInvois is written as a UBL 2.1 JSON document
//! of version 1.0 (`_D`, `_A`, `_B` and `[{"_": value}]` values), with the supplier's TIN, BRN, SST
//! number and MSIC code, both parties' addresses with their state codes, each line's
//! classification and tax type (01 sales tax, 02 service tax, 06 not applicable, E exempt), and
//! queued in the outbox. The connector takes a token (OAuth client credentials, `/connect/token`),
//! submits the document (`POST /api/v1.0/documentsubmissions/`, base64 with its SHA-256), then
//! asks after it (`GET /api/v1.0/documentsubmissions/{uid}`) until LHDN has validated it, and
//! records its UUID, long ID and the validation link for the invoice's QR code
//! (`{portal}/{uuid}/share/{longId}`), or LHDN's errors. Cancelling an invoice within the 72 hours
//! after its validation cancels it at LHDN (`PUT /api/v1.0/documents/state/{uuid}/state`); after
//! that, a credit note corrects it. Consumers buy under the general public's TIN (EI00000000010),
//! buyers abroad under EI00000000020.

use crate::util::{b, d, get, id, rows, s, sys};
use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Duration, Utc};
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, EventSink, Filter, HookCtx, Kernel, Result, outbox};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration as Wait;
use uuid::Uuid;

pub const SUBMIT_TOPIC: &str = "my.myinvois.submit_requested";
pub const STATUS_TOPIC: &str = "my.myinvois.status_requested";
pub const CANCEL_TOPIC: &str = "my.myinvois.cancel_requested";
const SANDBOX: (&str, &str) = ("https://preprod-api.myinvois.hasil.gov.my", "https://preprod.myinvois.hasil.gov.my");
const PRODUCTION: (&str, &str) = ("https://api.myinvois.hasil.gov.my", "https://myinvois.hasil.gov.my");
pub const GENERAL_PUBLIC: &str = "EI00000000010";
pub const FOREIGN_BUYER: &str = "EI00000000020";

/// MyInvois's API and portal hosts for a company's mode (none when it is Off);
/// `FERRUM_MY_MYINVOIS_URL` stands in for both.
fn hosts(mode: &str) -> Option<(String, String)> {
    if mode == "Off" {
        return None;
    }
    if let Ok(u) = std::env::var("FERRUM_MY_MYINVOIS_URL")
        && !u.trim().is_empty()
    {
        let u = u.trim_end_matches('/').to_string();
        return Some((u.clone(), u));
    }
    match mode {
        "Sandbox" => Some((SANDBOX.0.into(), SANDBOX.1.into())),
        "Production" => Some((PRODUCTION.0.into(), PRODUCTION.1.into())),
        _ => None,
    }
}

/// Malaysia's states as MyInvois codes them (17: not applicable, abroad).
pub const STATES: &[(&str, &str)] = &[
    ("01", "Johor"),
    ("02", "Kedah"),
    ("03", "Kelantan"),
    ("04", "Melaka"),
    ("05", "Negeri Sembilan"),
    ("06", "Pahang"),
    ("07", "Pulau Pinang"),
    ("08", "Perak"),
    ("09", "Perlis"),
    ("10", "Selangor"),
    ("11", "Terengganu"),
    ("12", "Sabah"),
    ("13", "Sarawak"),
    ("14", "Wilayah Persekutuan Kuala Lumpur"),
    ("15", "Wilayah Persekutuan Labuan"),
    ("16", "Wilayah Persekutuan Putrajaya"),
    ("17", "Not Applicable"),
];

/// A state's code from its code or name ("14", "Kuala Lumpur", "Penang").
pub fn state_code(text: &str) -> Option<&'static str> {
    let t = text.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    let t = match t.as_str() {
        "penang" => "pulau pinang",
        "malacca" => "melaka",
        "kuala lumpur" | "kl" | "w.p. kuala lumpur" => "wilayah persekutuan kuala lumpur",
        "labuan" => "wilayah persekutuan labuan",
        "putrajaya" => "wilayah persekutuan putrajaya",
        other => other,
    };
    STATES.iter().find(|(c, n)| *c == t || n.to_lowercase() == t).map(|(c, _)| *c)
}

/// A line's MyInvois tax type from the engine's category.
pub fn tax_type(category: &str) -> &'static str {
    match category {
        "Sales Tax 10%" | "Sales Tax 5%" => "01",
        "Service Tax" | "Service Tax 6%" => "02",
        "Exempt" => "E",
        _ => "06",
    }
}

fn v(value: impl Into<Value>) -> Value {
    json!([{"_": value.into()}])
}

fn amount(x: Decimal, currency: &str) -> Value {
    let x = x.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    json!([{"_": x.to_string().parse::<f64>().unwrap_or_default(), "currencyID": currency}])
}

fn number(x: Decimal) -> Value {
    json!(x.normalize().to_string().parse::<f64>().unwrap_or_default())
}

/// A party's address as MyInvois takes it.
fn address(line: &str, city: &str, postal: Option<&str>, state: &str, country3: &str) -> Value {
    let mut a = json!({
        "CityName": v(city), "CountrySubentityCode": v(state), "AddressLine": [{"Line": v(line)}],
        "Country": [{"IdentificationCode": [{"_": country3, "listID": "ISO3166-1", "listAgencyID": "6"}]}],
    });
    if let Some(p) = postal.filter(|p| !p.is_empty()) {
        a["PostalZone"] = v(p);
    }
    json!([a])
}

fn ids(pairs: &[(&str, &str)]) -> Value {
    Value::Array(pairs.iter().map(|(scheme, value)| json!({"ID": [{"_": value, "schemeID": scheme}]})).collect())
}

/// ISO 3166-1 alpha-3 for the countries Ferrum's addresses carry most; others as written.
fn alpha3(code: &str) -> String {
    match code {
        "MY" => "MYS",
        "SG" => "SGP",
        "ID" => "IDN",
        "TH" => "THA",
        "PH" => "PHL",
        "VN" => "VNM",
        "BN" => "BRN",
        "CN" => "CHN",
        "IN" => "IND",
        "US" => "USA",
        "GB" => "GBR",
        "AU" => "AUS",
        "JP" => "JPN",
        "HK" => "HKG",
        "AE" => "ARE",
        other => return other.to_string(),
    }
    .to_string()
}

/// A submitted invoice's MyInvois document (JSON 1.0) and what it needs to check first.
pub async fn document(hc: &mut HookCtx<'_>, doc: &Doc, now: DateTime<Utc>) -> Result<Value> {
    let ctx = sys(hc.ctx);
    let company = get(hc.kernel, &ctx, hc.tx, "Company", id(doc, "company").ok_or_else(|| Error::internal("invoice without company"))?).await?;
    let customer = get(hc.kernel, &ctx, hc.tx, "Customer", id(doc, "customer").ok_or_else(|| Error::internal("invoice without customer"))?).await?;
    let mut problems = Vec::new();
    let mut need = |ok: bool, what: &str| {
        if !ok {
            problems.push(what.to_string());
        }
    };
    let field = |d: &Doc, f: &str| s(d, f).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string);
    let (tin, brn, msic, msic_name, phone) =
        (field(&company, "my_tin"), field(&company, "my_brn"), field(&company, "my_msic_code"), field(&company, "my_msic_name"), field(&company, "my_phone"));
    let (street, city, state) = (field(&company, "my_street"), field(&company, "my_city"), s(&company, "my_state").and_then(state_code));
    need(tin.is_some(), "the company's TIN");
    need(brn.is_some(), "the company's BRN (business registration number)");
    need(msic.as_deref().is_some_and(|m| m.len() == 5), "the company's MSIC code (five digits) and its description");
    need(msic_name.is_some(), "the company's MSIC description");
    need(phone.is_some(), "the company's telephone number");
    need(street.is_some() && city.is_some() && state.is_some(), "the company's address: street, city and state");
    let currency = match id(doc, "currency") {
        Some(c) => s(&get(hc.kernel, &ctx, hc.tx, "Currency", c).await?, "code").unwrap_or("MYR").to_string(),
        None => "MYR".into(),
    };
    let billing = match id(doc, "customer_address") {
        Some(a) => crate::util::find(hc.kernel, &ctx, hc.tx, "Address", vec![Filter::eq("id", a.to_string())], None, 1).await?.into_iter().next(),
        None => None,
    };
    let abroad = billing.as_ref().and_then(|a| s(a, "country_code")).is_some_and(|c| c != "MY");
    let business = s(&customer, "my_tin").is_some();
    let buyer_tin = field(&customer, "my_tin").unwrap_or_else(|| if abroad { FOREIGN_BUYER.into() } else { GENERAL_PUBLIC.into() });
    let buyer_brn = field(&customer, "my_brn").unwrap_or_else(|| "NA".into());
    let (b_line, b_city, b_postal, b_state, b_country) = match &billing {
        Some(a) => (
            s(a, "address_line1").unwrap_or("NA").to_string(),
            s(a, "city").unwrap_or("NA").to_string(),
            s(a, "postal_code").map(str::to_string),
            if abroad { "17".to_string() } else { s(a, "state").and_then(state_code).unwrap_or("17").to_string() },
            alpha3(s(a, "country_code").unwrap_or("MY")),
        ),
        None => ("NA".into(), "NA".into(), None, "17".into(), "MYS".into()),
    };
    if business && !abroad {
        need(billing.as_ref().and_then(|a| s(a, "state")).and_then(state_code).is_some(), "the customer's state (its billing address)");
    }
    if !problems.is_empty() {
        return Err(Error::invalid(format!("MyInvois would refuse the e-invoice; set {}", problems.join(", "))));
    }
    let cid = id(&company, "id").unwrap_or_default();
    let mut accounts = Vec::new();
    for r in crate::my::COUNTRY.rows {
        if let Ok(a) = crate::engine::account(hc.kernel, &ctx, hc.tx, cid, r.sales).await {
            accounts.push(a);
        }
    }
    let credit = b(doc, "is_return");
    let sign = if credit { -Decimal::ONE } else { Decimal::ONE };
    let mut lines = Vec::new();
    let mut by_type: Vec<(&'static str, Decimal, Decimal)> = Vec::new();
    for (i, row) in rows(doc, "items").iter().enumerate() {
        let (mut category, mut rate, mut reason) = ("Out of Scope".to_string(), Decimal::ZERO, None::<String>);
        for a in &accounts {
            let cat = &row["tax_categories"][a.as_str()];
            if cat.is_null() || cat["exemption_reason"].as_str().is_some_and(|r| r.starts_with(crate::engine::NOT_SUBJECT)) {
                continue;
            }
            category = cat["category"].as_str().unwrap_or("Out of Scope").to_string();
            rate = row["item_tax_rates"][a.as_str()].as_str().and_then(|x| x.parse().ok()).unwrap_or_default();
            reason = cat["exemption_reason"].as_str().map(str::to_string);
        }
        let kind = tax_type(&category);
        let net = sign * d(row, "net_amount");
        let tax = (net * rate / Decimal::ONE_HUNDRED).round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
        let qty = (sign * d(row, "qty")).abs();
        let item = match id(row, "item") {
            Some(it) => get(hc.kernel, &ctx, hc.tx, "Item", it).await?,
            None => Doc::new(),
        };
        let class = s(&item, "my_classification").unwrap_or("022");
        let mut sub = json!({"TaxableAmount": amount(net, &currency), "TaxAmount": amount(tax, &currency), "Percent": [{"_": number(rate)}],
                             "TaxCategory": [{"ID": v(kind), "TaxScheme": [{"ID": [{"_": "OTH", "schemeID": "UN/ECE 5153", "schemeAgencyID": "6"}]}]}]});
        if kind == "E" {
            sub["TaxCategory"][0]["TaxExemptionReason"] = v(reason.as_deref().unwrap_or("Exempt from sales tax"));
        }
        lines.push(json!({
            "ID": v((i + 1).to_string()), "InvoicedQuantity": [{"_": number(qty), "unitCode": "C62"}], "LineExtensionAmount": amount(net, &currency),
            "TaxTotal": [{"TaxAmount": amount(tax, &currency), "TaxSubtotal": [sub]}],
            "Item": [{"CommodityClassification": [{"ItemClassificationCode": [{"_": class, "listID": "CLASS"}]}], "Description": v(s(row, "item_name").unwrap_or("-"))}],
            "Price": [{"PriceAmount": amount(if qty.is_zero() { net } else { (net / qty).abs() }, &currency)}],
            "ItemPriceExtension": [{"Amount": amount(net, &currency)}],
        }));
        match by_type.iter_mut().find(|(k, _, _)| *k == kind) {
            Some(t) => {
                t.1 += net;
                t.2 += tax;
            }
            None => by_type.push((kind, net, tax)),
        }
    }
    let total_tax: Decimal = by_type.iter().map(|t| t.2).sum();
    let net_total: Decimal = by_type.iter().map(|t| t.1).sum();
    let subtotals: Vec<Value> = by_type
        .iter()
        .map(|(kind, net, tax)| {
            json!({"TaxableAmount": amount(*net, &currency), "TaxAmount": amount(*tax, &currency),
                   "TaxCategory": [{"ID": v(*kind), "TaxScheme": [{"ID": [{"_": "OTH", "schemeID": "UN/ECE 5153", "schemeAgencyID": "6"}]}]}]})
        })
        .collect();
    let mut invoice = json!({
        "ID": v(s(doc, "number").unwrap_or_default()),
        "IssueDate": v(now.format("%Y-%m-%d").to_string()),
        "IssueTime": v(now.format("%H:%M:%SZ").to_string()),
        "InvoiceTypeCode": [{"_": if credit { "02" } else { "01" }, "listVersionID": "1.0"}],
        "DocumentCurrencyCode": v(currency.as_str()),
        "AccountingSupplierParty": [{"Party": [{
            "IndustryClassificationCode": [{"_": msic, "name": msic_name}],
            "PartyIdentification": ids(&[("TIN", tin.as_deref().unwrap_or_default()), ("BRN", brn.as_deref().unwrap_or_default()),
                                          ("SST", s(&company, "my_sst_number").unwrap_or("NA")), ("TTX", "NA")]),
            "PostalAddress": address(street.as_deref().unwrap_or_default(), city.as_deref().unwrap_or_default(), s(&company, "my_postal_code"), state.unwrap_or("17"), "MYS"),
            "PartyLegalEntity": [{"RegistrationName": v(s(&company, "company_name").unwrap_or_default())}],
            "Contact": [{"Telephone": v(phone.as_deref().unwrap_or_default())}],
        }]}],
        "AccountingCustomerParty": [{"Party": [{
            "PostalAddress": address(&b_line, &b_city, b_postal.as_deref(), &b_state, &b_country),
            "PartyLegalEntity": [{"RegistrationName": v(s(&customer, "customer_name").unwrap_or("General Public"))}],
            "PartyIdentification": ids(&[("TIN", buyer_tin.as_str()), ("BRN", buyer_brn.as_str()), ("SST", s(&customer, "my_sst_number").unwrap_or("NA"))]),
            "Contact": [{"Telephone": v(s(&customer, "my_phone").or(s(&customer, "phone")).unwrap_or("NA"))}],
        }]}],
        "TaxTotal": [{"TaxAmount": amount(total_tax, &currency), "TaxSubtotal": subtotals}],
        "LegalMonetaryTotal": [{
            "LineExtensionAmount": amount(net_total, &currency), "TaxExclusiveAmount": amount(net_total, &currency),
            "TaxInclusiveAmount": amount(net_total + total_tax, &currency), "PayableAmount": amount(net_total + total_tax, &currency),
        }],
        "InvoiceLine": lines,
    });
    if credit && let Some(o) = id(doc, "return_against") {
        let orig = get(hc.kernel, &ctx, hc.tx, "SalesInvoice", o).await?;
        let uuid = s(&orig, "my_uuid").ok_or_else(|| Error::invalid("the credit note corrects an invoice MyInvois has not validated: send that one first"))?;
        invoice["BillingReference"] = json!([{"InvoiceDocumentReference": [{"ID": v(s(&orig, "number").unwrap_or_default()), "UUID": v(uuid)}]}]);
    }
    if currency != "MYR" {
        invoice["TaxCurrencyCode"] = v("MYR");
    }
    Ok(json!({
        "_D": "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2",
        "_A": "urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2",
        "_B": "urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2",
        "Invoice": [invoice],
    }))
}

/// The submission's body for a document: its bytes in base64 and their SHA-256 (hexadecimal).
pub fn submission(document: &Value, code: &str) -> Value {
    let bytes = serde_json::to_vec(document).unwrap_or_default();
    let hash = hex::encode(Sha256::digest(&bytes));
    json!({"documents": [{"format": "JSON", "document": base64::engine::general_purpose::STANDARD.encode(&bytes), "documentHash": hash, "codeNumber": code}]})
}

/// Submitted invoices queued for MyInvois, cancelled ones cancelled there within LHDN's window.
pub struct MyInvoisHook;

async fn uses_myinvois(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Option<Doc>> {
    let Some(cid) = id(doc, "company") else { return Ok(None) };
    let company = get(hc.kernel, &sys(hc.ctx), hc.tx, "Company", cid).await?;
    Ok((s(&company, "country_code") == Some("MY") && hosts(s(&company, "my_myinvois_mode").unwrap_or("Off")).is_some()).then_some(company))
}

async fn set_invoice(k: &Kernel, ctx: &Ctx, invoice: Uuid, values: Value) -> Result<()> {
    let Value::Object(v) = values else { return Ok(()) };
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    k.set_values_tx(&sys(ctx), &mut tx, "SalesInvoice", invoice, v).await?;
    tx.commit().await?;
    Ok(())
}

#[async_trait]
impl DocHook for MyInvoisHook {
    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.entity.def.name != "SalesInvoice" || hc.entity.field("my_einvoice_status").is_none() || uses_myinvois(hc, doc).await?.is_none() {
            return Ok(());
        }
        let body = submission(&document(hc, doc, Utc::now()).await?, s(doc, "number").unwrap_or_default());
        let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let Value::Object(v) = json!({"my_einvoice_status": "Pending", "my_einvoice_error": null}) else { return Ok(()) };
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", iid, v).await?;
        outbox::enqueue(
            hc.tx,
            hc.ctx.tenant,
            SUBMIT_TOPIC,
            Some("SalesInvoice"),
            Some(iid),
            json!({"invoice": iid, "company": s(doc, "company"), "body": body}),
        )
        .await
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        match s(doc, "my_einvoice_status") {
            Some("Pending" | "Submitted") => Err(Error::invalid("MyInvois is still validating this invoice: cancel it once it is validated")),
            Some("Valid") => {
                let validated = s(doc, "my_validated_at").and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc));
                if validated.is_none_or(|t| Utc::now() > t + Duration::hours(72)) {
                    return Err(Error::invalid("LHDN cancels an e-invoice only within 72 hours of validating it: issue a credit note instead"));
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if s(doc, "my_einvoice_status") != Some("Valid") || uses_myinvois(hc, doc).await?.is_none() {
            return Ok(());
        }
        let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let event = json!({"invoice": iid, "company": s(doc, "company"), "uuid": s(doc, "my_uuid"), "reason": format!("Invoice {} cancelled", s(doc, "number").unwrap_or_default())});
        outbox::enqueue(hc.tx, hc.ctx.tenant, CANCEL_TOPIC, Some("SalesInvoice"), Some(iid), event).await
    }
}

/// The connector.
pub struct MyInvois {
    client: reqwest::Client,
}

impl Default for MyInvois {
    fn default() -> Self {
        MyInvois { client: reqwest::Client::builder().timeout(Wait::from_secs(60)).build().unwrap_or_default() }
    }
}

impl MyInvois {
    /// A usable access token for the company: the stored one while it lasts, else a new one.
    async fn token(&self, k: &Kernel, ctx: &Ctx, api: &str, company: &Doc) -> Result<String> {
        let cid = id(company, "id").ok_or_else(|| Error::internal("company without id"))?;
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let stored = k.secret(&mut tx, ctx.tenant, "Company", cid, "my_access_token").await?;
        let secret = k.secret(&mut tx, ctx.tenant, "Company", cid, "my_client_secret").await?;
        tx.commit().await?;
        let fresh = s(company, "my_token_expires").and_then(|t| DateTime::parse_from_rfc3339(t).ok()).is_some_and(|t| t > Utc::now() + Duration::seconds(60));
        if let (Some(t), true) = (stored, fresh) {
            return Ok(t);
        }
        let (Some(client_id), Some(secret)) = (s(company, "my_client_id"), secret) else {
            return Err(Error::invalid("set the company's MyInvois client ID and client secret"));
        };
        let reply = self
            .client
            .post(format!("{api}/connect/token"))
            .form(&[("client_id", client_id), ("client_secret", secret.as_str()), ("grant_type", "client_credentials"), ("scope", "InvoicingAPI")])
            .send()
            .await
            .map_err(|e| Error::internal(format!("MyInvois could not be reached: {e}")))?;
        let status = reply.status();
        let v: Value = reply.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(Error::invalid(format!("MyInvois refused the client credentials ({status}): {}", v["error"].as_str().unwrap_or("no reason given"))));
        }
        let token = v["access_token"].as_str().ok_or_else(|| Error::invalid("MyInvois's answer has no access token"))?.to_string();
        let expires = Utc::now() + Duration::seconds(v["expires_in"].as_i64().unwrap_or(3600));
        let mut patch = Map::new();
        patch.insert("my_access_token".into(), json!(token));
        patch.insert("my_token_expires".into(), json!(expires.to_rfc3339()));
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        k.update_tx(&sys(ctx), &mut tx, "Company", cid, patch, None).await?;
        tx.commit().await?;
        Ok(token)
    }

    fn errors(v: &Value) -> String {
        let e = &v["error"];
        let details: Vec<String> =
            e["details"].as_array().map(|d| d.iter().filter_map(|x| x["message"].as_str().map(str::to_string)).collect()).unwrap_or_default();
        let head = e["message"].as_str().or(e.as_str()).unwrap_or("no reason given").to_string();
        if details.is_empty() { head } else { format!("{head}: {}", details.join("; ")) }
    }
}

#[async_trait]
impl EventSink for MyInvois {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ![SUBMIT_TOPIC, STATUS_TOPIC, CANCEL_TOPIC].contains(&ev.topic.as_str()) {
            return Ok(());
        }
        let Some(invoice) = ev.record_id else { return Ok(()) };
        let ctx = Ctx::system(ev.tenant_id, k.registry.schema(ev.tenant_id).await?);
        let Some(cid) = ev.payload["company"].as_str().and_then(|c| c.parse::<Uuid>().ok()) else { return Ok(()) };
        let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
        let company = get(k, &ctx, &mut tx, "Company", cid).await?;
        tx.commit().await?;
        let Some((api, portal)) = hosts(s(&company, "my_myinvois_mode").unwrap_or("Off")) else {
            return set_invoice(
                k,
                &ctx,
                invoice,
                json!({"my_einvoice_status": "Failed", "my_einvoice_error": "the company no longer sends e-invoices to MyInvois"}),
            )
            .await;
        };
        let token = match self.token(k, &ctx, &api, &company).await {
            Ok(t) => t,
            Err(Error::Invalid(m)) => return set_invoice(k, &ctx, invoice, json!({"my_einvoice_status": "Failed", "my_einvoice_error": m})).await,
            Err(e) => return Err(e),
        };
        let reached = |e: reqwest::Error| Error::internal(format!("MyInvois could not be reached: {e}"));
        match ev.topic.as_str() {
            SUBMIT_TOPIC => {
                let reply = self
                    .client
                    .post(format!("{api}/api/v1.0/documentsubmissions/"))
                    .bearer_auth(&token)
                    .json(&ev.payload["body"])
                    .send()
                    .await
                    .map_err(reached)?;
                let status = reply.status().as_u16();
                let v: Value = reply.json().await.unwrap_or(Value::Null);
                match status {
                    202 => {
                        if let Some(rejected) = v["rejectedDocuments"].as_array().and_then(|r| r.first()) {
                            return set_invoice(k, &ctx, invoice, json!({"my_einvoice_status": "Invalid", "my_einvoice_error": Self::errors(rejected)})).await;
                        }
                        let uid = v["submissionUid"].as_str().or(v["submissionUID"].as_str()).unwrap_or_default().to_string();
                        let uuid = v["acceptedDocuments"][0]["uuid"].as_str().map(str::to_string);
                        set_invoice(k, &ctx, invoice, json!({"my_einvoice_status": "Submitted", "my_submission_uid": uid, "my_uuid": uuid})).await?;
                        let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
                        outbox::enqueue(
                            &mut tx,
                            ev.tenant_id,
                            STATUS_TOPIC,
                            Some("SalesInvoice"),
                            Some(invoice),
                            json!({"invoice": invoice, "company": cid, "submission": uid}),
                        )
                        .await?;
                        tx.commit().await?;
                        Ok(())
                    }
                    401 => Err(Error::internal("MyInvois refused the token; trying again with a new one")),
                    422 | 429 | 500.. => Err(Error::internal(format!("MyInvois is busy ({status}); trying again"))),
                    _ => set_invoice(k, &ctx, invoice, json!({"my_einvoice_status": "Invalid", "my_einvoice_error": Self::errors(&v)})).await,
                }
            }
            STATUS_TOPIC => {
                let uid = ev.payload["submission"].as_str().unwrap_or_default();
                let reply = self.client.get(format!("{api}/api/v1.0/documentsubmissions/{uid}")).bearer_auth(&token).send().await.map_err(reached)?;
                let status = reply.status().as_u16();
                let v: Value = reply.json().await.unwrap_or(Value::Null);
                if status != 200 {
                    return Err(Error::internal(format!("MyInvois did not say how the submission went ({status}); asking again")));
                }
                let summary = &v["documentSummary"][0];
                match summary["status"].as_str().unwrap_or_default() {
                    "Valid" => {
                        let (uuid, long) = (summary["uuid"].as_str().unwrap_or_default(), summary["longId"].as_str().unwrap_or_default());
                        set_invoice(
                            k,
                            &ctx,
                            invoice,
                            json!({"my_einvoice_status": "Valid", "my_uuid": uuid, "my_long_id": long, "my_validation_url": format!("{portal}/{uuid}/share/{long}"),
                                   "my_validated_at": summary["dateTimeValidated"].as_str().map(str::to_string).unwrap_or_else(|| Utc::now().to_rfc3339()), "my_einvoice_error": null}),
                        )
                        .await
                    }
                    "Invalid" => {
                        let why = summary["documentStatusReason"].as_str().unwrap_or("LHDN found the e-invoice invalid").to_string();
                        set_invoice(k, &ctx, invoice, json!({"my_einvoice_status": "Invalid", "my_einvoice_error": why})).await
                    }
                    // Submitted, in progress: asked again later.
                    _ => Err(Error::internal("MyInvois is still validating the e-invoice; asking again")),
                }
            }
            _ => {
                let uuid = ev.payload["uuid"].as_str().unwrap_or_default();
                let reply = self
                    .client
                    .put(format!("{api}/api/v1.0/documents/state/{uuid}/state"))
                    .bearer_auth(&token)
                    .json(&json!({"status": "cancelled", "reason": ev.payload["reason"]}))
                    .send()
                    .await
                    .map_err(reached)?;
                let status = reply.status().as_u16();
                let v: Value = reply.json().await.unwrap_or(Value::Null);
                match status {
                    200 => set_invoice(k, &ctx, invoice, json!({"my_einvoice_status": "Cancelled", "my_einvoice_error": null})).await,
                    429 | 500.. => Err(Error::internal(format!("MyInvois is busy ({status}); trying again"))),
                    _ => {
                        set_invoice(
                            k,
                            &ctx,
                            invoice,
                            json!({"my_einvoice_error": format!("MyInvois did not cancel it: {}; issue a credit note", Self::errors(&v))}),
                        )
                        .await
                    }
                }
            }
        }
    }
}

pub fn register(h: &mut ferrum_kernel::HookRegistry) {
    h.register("SalesInvoice", std::sync::Arc::new(MyInvoisHook));
    h.add_event_sink(std::sync::Arc::new(MyInvois::default()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_types_and_the_submission() {
        assert_eq!((state_code("Kuala Lumpur"), state_code("Penang"), state_code("10"), state_code("Narnia")), (Some("14"), Some("07"), Some("10"), None));
        assert_eq!((tax_type("Sales Tax 10%"), tax_type("Service Tax"), tax_type("Exempt"), tax_type("Out of Scope")), ("01", "02", "E", "06"));
        let body = submission(&json!({"Invoice": [{"ID": [{"_": "INV-1"}]}]}), "INV-1");
        let docs = &body["documents"][0];
        let bytes = base64::engine::general_purpose::STANDARD.decode(docs["document"].as_str().unwrap()).unwrap();
        assert_eq!(docs["documentHash"].as_str().unwrap(), hex::encode(Sha256::digest(&bytes)), "the hash of the very bytes sent");
        assert_eq!((docs["format"].clone(), docs["codeNumber"].clone()), (json!("JSON"), json!("INV-1")));
    }
}
