//! Carriers behind one interface (OP-17). A [carrier connection](CarrierHook) is configuration: four
//! described HTTP calls (rates, label, tracking, cancellation), each a method, headers and a body or form
//! as templates over the shipment, and JSON pointers saying where the answer holds the rates, the label
//! file, the tracking number and the error, and which statuses mean "later". Its keys are secret fields,
//! sealed by the platform. A test connection answers from fixtures. No carrier's name or format is here.
//!
//! The runner is modelled on the platform's message channels (`ferrum_kernel::messages`, its
//! `deliver_message`): the platform offers described calls only for text and chat channels, so this is
//! the smallest copy that an app-facing "described call on a connection" in the kernel would replace.
//!
//! A label is bought once per shipment: the shipment's row is locked first, a second press (or a retry)
//! finds the label there and answers it; every attempt sends the same idempotency key (`key`, the
//! shipment's id) for carriers that keep one.

use crate::ledger::{self, Entry, Line};
use crate::util::{company, d, get, id, money, rows, s, sys};
use async_trait::async_trait;
use base64::Engine;
use ferrum_domain::flow::{create_for_user, created};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, Filter, HookCtx, Kernel, Result, Site, SiteRequest, SiteResponse, Tx};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;
use uuid::Uuid;

/// Statuses meaning "later" when a call names none.
const TEMPORARY: [u16; 7] = [408, 425, 429, 500, 502, 503, 504];
/// What tracking may say, after a connection's `statuses` map the carrier's words.
const TRACKED: [&str; 3] = ["In Transit", "Delivered", "Exception"];

/// One described call and where its answer says what.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// A JSON template.
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub form: BTreeMap<String, String>,
    #[serde(default)]
    pub error_at: Option<String>,
    #[serde(default)]
    pub temporary: Option<Vec<u16>>,
    // Rates: the list, then each rate's parts relative to it.
    #[serde(default)]
    pub rates_at: Option<String>,
    #[serde(default)]
    pub service_at: Option<String>,
    #[serde(default)]
    pub amount_at: Option<String>,
    #[serde(default)]
    pub currency_at: Option<String>,
    #[serde(default)]
    pub days_at: Option<String>,
    #[serde(default)]
    pub rate_id_at: Option<String>,
    // A label.
    #[serde(default)]
    pub tracking_at: Option<String>,
    #[serde(default)]
    pub tracking_url_at: Option<String>,
    /// The label file in base64.
    #[serde(default)]
    pub label_at: Option<String>,
    /// Or where to fetch it.
    #[serde(default)]
    pub label_url_at: Option<String>,
    /// `pdf` (the default), `zpl`, `png`.
    #[serde(default)]
    pub label_type: Option<String>,
    #[serde(default)]
    pub cost_at: Option<String>,
    #[serde(default)]
    pub id_at: Option<String>,
    // Tracking.
    #[serde(default)]
    pub status_at: Option<String>,
    #[serde(default)]
    pub statuses: BTreeMap<String, String>,
}

/// How a carrier's own calls to us are checked and read.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inbound {
    /// `hmac` (HMAC-SHA256 of the body under the inbound secret, hex, in `header`) or `token` (the secret
    /// itself in `header`, or the `token` query parameter).
    pub auth: String,
    #[serde(default)]
    pub header: Option<String>,
    pub tracking_at: String,
    pub status_at: String,
    #[serde(default)]
    pub statuses: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Description {
    pub rate: Call,
    pub label: Call,
    #[serde(default)]
    pub track: Option<Call>,
    #[serde(default)]
    pub cancel: Option<Call>,
    /// A template giving the link people follow, when the label's answer has none.
    #[serde(default)]
    pub tracking_link: Option<String>,
    #[serde(default)]
    pub inbound: Option<Inbound>,
}

fn environment() -> minijinja::Environment<'static> {
    let mut env = minijinja::Environment::new();
    env.set_auto_escape_callback(|_| minijinja::AutoEscape::None);
    env.set_fuel(Some(500_000));
    env.add_filter("json", |v: minijinja::Value| serde_json::to_string(&v).unwrap_or_default());
    env
}

fn render(src: &str, vars: &Value) -> Result<String> {
    environment().render_str(src, minijinja::Value::from_serialize(vars)).map_err(|e| Error::invalid(format!("a carrier call's template: {e}")))
}

/// Where a JSON pointer leads, as text.
fn at(v: &Value, pointer: Option<&str>) -> Option<String> {
    match v.pointer(pointer?)? {
        Value::String(s) => Some(s.clone()).filter(|s| !s.is_empty()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

impl Description {
    /// What is wrong with it, said plainly.
    pub fn problem(&self, test: bool) -> Option<String> {
        let env = environment();
        let calls = [("rate", Some(&self.rate)), ("label", Some(&self.label)), ("track", self.track.as_ref()), ("cancel", self.cancel.as_ref())];
        for (name, c) in calls {
            let Some(c) = c else { continue };
            if !test && !c.url.starts_with("https://") && !c.url.starts_with("http://") {
                return Some(format!("{name}.url must be an https address"));
            }
            if c.method.as_deref().is_some_and(|m| !["POST", "PUT", "GET", "DELETE", "PATCH"].contains(&m)) {
                return Some(format!("{name}.method is POST, PUT, PATCH, GET or DELETE"));
            }
            if c.body.is_some() && !c.form.is_empty() {
                return Some(format!("{name} has a body or a form, not both"));
            }
            for t in std::iter::once(&c.url).chain(c.headers.values()).chain(c.body.iter()).chain(c.form.values()) {
                if let Err(e) = env.template_from_str(t) {
                    return Some(format!("{name}: '{t}': {e}"));
                }
            }
            let pointers = [
                &c.error_at,
                &c.rates_at,
                &c.service_at,
                &c.amount_at,
                &c.currency_at,
                &c.days_at,
                &c.rate_id_at,
                &c.tracking_at,
                &c.tracking_url_at,
                &c.label_at,
                &c.label_url_at,
                &c.cost_at,
                &c.id_at,
                &c.status_at,
            ];
            if let Some(p) = pointers.into_iter().flatten().find(|p| !p.starts_with('/')) {
                return Some(format!("{name}: {p} is not a JSON pointer (/a/0/b)"));
            }
            if let Some(s) = c.statuses.values().find(|s| !TRACKED.contains(&s.as_str())) {
                return Some(format!("{name}.statuses map the carrier's words to In Transit, Delivered or Exception, not {s}"));
            }
        }
        if self.rate.rates_at.is_none() || self.rate.service_at.is_none() || self.rate.amount_at.is_none() {
            return Some("rate needs rates_at, service_at and amount_at".into());
        }
        if self.label.tracking_at.is_none() || (self.label.label_at.is_none() && self.label.label_url_at.is_none()) {
            return Some("label needs tracking_at, and label_at or label_url_at".into());
        }
        if let Some(t) = &self.track
            && t.status_at.is_none()
        {
            return Some("track needs status_at".into());
        }
        if let Some(i) = &self.inbound {
            if !matches!(i.auth.as_str(), "hmac" | "token") || (i.auth == "hmac" && i.header.is_none()) {
                return Some("inbound.auth is hmac (with a header) or token".into());
            }
            if let Some(s) = i.statuses.values().find(|s| !TRACKED.contains(&s.as_str())) {
                return Some(format!("inbound.statuses map the carrier's words to In Transit, Delivered or Exception, not {s}"));
            }
        }
        if let Some(t) = &self.tracking_link
            && let Err(e) = env.template_from_str(t)
        {
            return Some(format!("tracking_link: {e}"));
        }
        None
    }
}

/// A connection as the calls need it.
struct Conn {
    doc: Doc,
    desc: Description,
    secrets: Value,
}

async fn connection(k: &Kernel, ctx: &Ctx, tx: &mut Tx, conn: Uuid) -> Result<Conn> {
    let doc = get(k, ctx, tx, "CarrierConnection", conn).await?;
    if !crate::util::b(&doc, "enabled") {
        return Err(Error::invalid(format!("the carrier connection {} is turned off", s(&doc, "connection_name").unwrap_or_default())));
    }
    let desc: Description = serde_json::from_value(doc.get("description").cloned().unwrap_or(Value::Null))
        .map_err(|e| Error::invalid(format!("the carrier connection's calls: {e}")))?;
    let mut secrets = serde_json::Map::new();
    for f in ["api_key", "api_secret"] {
        if let Some(v) = k.secret(tx, ctx.tenant, "CarrierConnection", conn, f).await? {
            secrets.insert(f.into(), json!(v));
        }
    }
    Ok(Conn { doc, desc, secrets: Value::Object(secrets) })
}

/// Makes one described call and answers the carrier's answer (a 2xx). A status the call calls temporary is
/// `Unavailable` (try again later); any other refusal says what the carrier said.
async fn call(k: &Kernel, conn: &Conn, op: &str, c: &Call, vars: &Value) -> Result<Value> {
    let r = |t: &str| render(t, vars);
    let url = r(&c.url)?;
    let mut headers = Vec::new();
    for (h, v) in &c.headers {
        headers.push((h.clone(), r(v)?));
    }
    let body = c.body.as_deref().map(r).transpose()?;
    let body: Option<Value> =
        body.map(|b| serde_json::from_str(&b).map_err(|e| Error::invalid(format!("the {op} call's body is not JSON: {e}")))).transpose()?;
    let form = c.form.iter().map(|(k, v)| Ok((k.clone(), r(v)?))).collect::<Result<Vec<_>>>()?;
    let (status, answer) = if crate::util::b(&conn.doc, "test") {
        let fixture = conn.doc.get("fixtures").and_then(|f| f.get(op)).cloned();
        (200, fixture.ok_or_else(|| Error::invalid(format!("the test carrier has no fixture for {op}")))?)
    } else {
        let (client, parsed) = k.vetted_client(&url, k.settings.webhooks_allow_private, Duration::from_secs(30), "Ferrum-Carriers/1").await?;
        let method = reqwest::Method::from_bytes(c.method.as_deref().unwrap_or("POST").as_bytes()).map_err(Error::internal)?;
        let mut req = client.request(method, parsed);
        for (h, v) in &headers {
            req = req.header(h.as_str(), v.as_str());
        }
        if let Some(b) = &body {
            req = req.json(b);
        } else if !form.is_empty() {
            req = req.form(&form);
        }
        let res = req.send().await.map_err(|e| Error::Unavailable(format!("the carrier did not answer: {e}")))?;
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        (status, serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text)))
    };
    if (200..300).contains(&status) {
        return Ok(answer);
    }
    let said = at(&answer, c.error_at.as_deref()).unwrap_or_else(|| answer.to_string().chars().take(300).collect());
    if c.temporary.as_deref().unwrap_or(&TEMPORARY).contains(&status) {
        return Err(Error::Unavailable(format!("the carrier answered {status}, try again later: {said}")));
    }
    Err(Error::invalid(format!("the carrier refused ({status}): {said}")))
}

/// What the calls read: the shipment and its parcels, the addresses, the chosen rate, the label's numbers,
/// the connection's units and secrets, and `key` (the same on every attempt for this shipment).
async fn vars(k: &Kernel, ctx: &Ctx, tx: &mut Tx, conn: &Conn, ship: &Doc) -> Result<Value> {
    let sctx = sys(ctx);
    let mut address = Vec::new();
    for a in [id(ship, "ship_to"), id(&conn.doc, "from_address")] {
        address.push(match a {
            Some(a) => Value::Object(k.get_tx(&sctx, tx, "Address", a, false).await?),
            None => Value::Null,
        });
    }
    let from = address.pop().unwrap_or_default();
    let to = address.pop().unwrap_or_default();
    let weight: Decimal = rows(ship, "packages").iter().map(|p| d(p, "weight")).sum();
    Ok(json!({
        "shipment": ship, "to": to, "from": from, "key": s(ship, "id"),
        "rate": {"id": ship.get("rate_id"), "service": ship.get("service"), "amount": ship.get("rate_amount"), "currency": ship.get("rate_currency")},
        "tracking_number": ship.get("tracking_number"), "provider_id": ship.get("provider_id"),
        "weight_unit": conn.doc.get("weight_unit"), "dimension_unit": conn.doc.get("dimension_unit"), "total_weight": weight.normalize().to_string(),
        "secret": conn.secrets,
    }))
}

fn set_fields(v: Value) -> Doc {
    v.as_object().cloned().unwrap_or_default()
}

async fn save(hc: &mut HookCtx<'_>, ship: Uuid, v: Value) -> Result<()> {
    hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "CarrierShipment", ship, set_fields(v)).await
}

pub struct CarrierHook;

#[async_trait]
impl DocHook for CarrierHook {
    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let desc: Description = serde_json::from_value(doc.get("description").cloned().unwrap_or(Value::Null))
            .map_err(|e| Error::field("description", format!("is not a carrier description: {e}")))?;
        if let Some(p) = desc.problem(crate::util::b(doc, "test")) {
            return Err(Error::field("description", p));
        }
        Ok(())
    }
}

pub struct ShipmentHook;

#[async_trait]
impl DocHook for ShipmentHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        // A carrier of another company is refused by the link's company scope; changing parcels drops the rates.
        if s(doc, "tracking_number").is_none() && doc.get("status").and_then(Value::as_str).is_none_or(|s| s == "Rated") {
            let old = match id(doc, "id") {
                Some(i) => hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "CarrierShipment", i, false).await.ok(),
                None => None,
            };
            let same = old
                .as_ref()
                .is_some_and(|o| o.get("packages") == doc.get("packages") && o.get("ship_to") == doc.get("ship_to") && o.get("carrier") == doc.get("carrier"));
            if !same {
                for (f, v) in
                    [("status", json!("Draft")), ("rates", Value::Null), ("service", Value::Null), ("rate_id", Value::Null), ("rate_amount", Value::Null)]
                {
                    doc.insert(f.into(), v);
                }
            }
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let sid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let out = match name {
            "get_rates" => rates(hc, sid).await?,
            "choose_rate" => {
                let service = params.get("service").and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
                choose(hc, sid, service.ok_or_else(|| Error::field("service", "is required"))?).await?
            }
            "buy_label" => buy_label(hc, sid).await?,
            "track" => track(hc.kernel, hc.ctx, hc.tx, sid).await?,
            "cancel_label" => cancel(hc, sid).await?,
            _ => return Ok(None),
        };
        Ok(Some(out))
    }
}

/// The shipment, locked for the rest of the transaction, and its connection.
async fn locked(k: &Kernel, ctx: &Ctx, tx: &mut Tx, sid: Uuid) -> Result<(Doc, Conn)> {
    let ship = k.get_tx(&sys(ctx), tx, "CarrierShipment", sid, true).await?;
    let conn = connection(k, ctx, tx, id(&ship, "carrier").ok_or_else(|| Error::field("carrier", "is required"))?).await?;
    Ok((ship, conn))
}

async fn rates(hc: &mut HookCtx<'_>, sid: Uuid) -> Result<Value> {
    let (ship, conn) = locked(hc.kernel, hc.ctx, hc.tx, sid).await?;
    if s(&ship, "tracking_number").is_some() {
        return Err(Error::invalid("a label is bought: cancel it to ship another way"));
    }
    if id(&ship, "ship_to").is_none() {
        return Err(Error::field("ship_to", "is required to ask for rates"));
    }
    let v = vars(hc.kernel, hc.ctx, hc.tx, &conn, &ship).await?;
    let c = &conn.desc.rate;
    let answer = call(hc.kernel, &conn, "rate", c, &v).await?;
    let list = answer.pointer(c.rates_at.as_deref().unwrap_or_default()).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut offered = Vec::new();
    for r in &list {
        let (Some(service), Some(amount)) = (at(r, c.service_at.as_deref()), at(r, c.amount_at.as_deref())) else { continue };
        let Ok(amount) = amount.trim().parse::<Decimal>() else { continue };
        let days = at(r, c.days_at.as_deref()).and_then(|d| d.parse::<f64>().ok()).map(|d| d.round() as i64);
        offered.push(json!({"service": service, "amount": money(amount), "currency": at(r, c.currency_at.as_deref()), "days": days, "id": at(r, c.rate_id_at.as_deref())}));
    }
    if offered.is_empty() {
        return Err(Error::invalid("the carrier offered no rate for these parcels"));
    }
    save(
        hc,
        sid,
        json!({"rates": offered, "status": "Rated", "service": null, "rate_id": null, "rate_amount": null, "rate_currency": null, "transit_days": null}),
    )
    .await?;
    Ok(json!({"rates": offered}))
}

async fn choose(hc: &mut HookCtx<'_>, sid: Uuid, service: &str) -> Result<Value> {
    let ship = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "CarrierShipment", sid, true).await?;
    if s(&ship, "tracking_number").is_some() {
        return Err(Error::invalid("a label is bought already"));
    }
    let offered = ship.get("rates").and_then(Value::as_array).cloned().unwrap_or_default();
    let r = offered
        .iter()
        .find(|r| r["service"].as_str() == Some(service))
        .ok_or_else(|| Error::field("service", format!("{service} is not among the rates offered: get rates again")))?;
    save(hc, sid, json!({"service": r["service"], "rate_id": r["id"], "rate_amount": r["amount"], "rate_currency": r["currency"], "transit_days": r["days"]}))
        .await?;
    Ok(r.clone())
}

async fn label_file(k: &Kernel, conn: &Conn, c: &Call, answer: &Value) -> Result<Vec<u8>> {
    if let Some(b64) = at(answer, c.label_at.as_deref()) {
        return base64::engine::general_purpose::STANDARD.decode(b64.trim()).map_err(|_| Error::invalid("the carrier's label is not base64"));
    }
    let url = at(answer, c.label_url_at.as_deref()).ok_or_else(|| Error::invalid("the carrier's answer holds no label"))?;
    if crate::util::b(&conn.doc, "test") {
        return Ok(url.into_bytes());
    }
    let (client, parsed) = k.vetted_client(&url, k.settings.webhooks_allow_private, Duration::from_secs(30), "Ferrum-Carriers/1").await?;
    let res = client.get(parsed).send().await.map_err(|e| Error::Unavailable(format!("the label could not be fetched: {e}")))?;
    if !res.status().is_success() {
        return Err(Error::Unavailable(format!("the label could not be fetched: {}", res.status())));
    }
    Ok(res.bytes().await.map_err(|e| Error::Unavailable(e.to_string()))?.to_vec())
}

async fn buy_label(hc: &mut HookCtx<'_>, sid: Uuid) -> Result<Value> {
    let (ship, conn) = locked(hc.kernel, hc.ctx, hc.tx, sid).await?;
    // Once: a second press, or a retry after the first committed, finds the label it bought.
    if let Some(t) = s(&ship, "tracking_number") {
        return Ok(json!({"tracking_number": t, "tracking_url": ship.get("tracking_url"), "file": ship.get("label_file"), "already": true}));
    }
    if s(&ship, "service").is_none() {
        return Err(Error::invalid("choose a rate first"));
    }
    let co = company(hc.kernel, hc.ctx, hc.tx, id(&ship, "company").unwrap_or_default()).await?;
    let freight = s(&conn.doc, "freight").unwrap_or("Not Booked").to_string();
    if freight != "Not Booked"
        && let Some(cur) = s(&ship, "rate_currency").filter(|c| !c.eq_ignore_ascii_case(&co.currency_code))
    {
        return Err(Error::invalid(format!("the rate is in {cur} and the books in {}: book this label's cost by hand", co.currency_code)));
    }
    let v = vars(hc.kernel, hc.ctx, hc.tx, &conn, &ship).await?;
    let c = &conn.desc.label;
    let answer = call(hc.kernel, &conn, "label", c, &v).await?;
    let tracking = at(&answer, c.tracking_at.as_deref()).ok_or_else(|| Error::invalid("the carrier's answer holds no tracking number"))?;
    let bytes = label_file(hc.kernel, &conn, c, &answer).await?;
    let kind = c.label_type.as_deref().unwrap_or("pdf").to_lowercase();
    let ctype = match kind.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        _ => "text/plain",
    };
    let file = hc.kernel.store_file(hc.ctx, None, &format!("label-{tracking}.{kind}"), ctype, &bytes).await?;
    let file_id: Uuid = file["id"].as_str().and_then(|f| f.parse().ok()).ok_or_else(|| Error::internal("a stored file without its id"))?;
    hc.kernel.attach_file_tx(&sys(hc.ctx), hc.tx, file_id, "CarrierShipment", sid).await?;
    let link = match (at(&answer, c.tracking_url_at.as_deref()), &conn.desc.tracking_link) {
        (Some(u), _) => Some(u),
        (None, Some(t)) => Some(render(t, &json!({"tracking_number": tracking, "shipment": ship}))?),
        _ => None,
    };
    let cost = match at(&answer, c.cost_at.as_deref()) {
        Some(c) => c.trim().parse::<Decimal>().map_err(|_| Error::invalid(format!("the carrier's cost {c} is not a number")))?,
        None => d(&ship, "rate_amount"),
    };
    let cost = crate::util::round(cost, co.precision);
    let booked = freight != "Not Booked" && cost > Decimal::ZERO;
    if booked {
        let expense = id(&conn.doc, "freight_account").ok_or_else(|| Error::invalid("the connection books freight: set its freight account"))?;
        let owed = id(&conn.doc, "freight_payable_account").ok_or_else(|| Error::invalid("the connection books freight: set its freight owed account"))?;
        let mut entry = Entry::new(co.id, hc.ctx.today(), "CarrierShipment", sid, s(&ship, "number").map(str::to_string));
        entry.lines.push(Line::new(expense, cost, &co.currency_code).cost_center(co.account("default_cost_center")));
        entry.lines.push(Line::new(owed, -cost, &co.currency_code));
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
    }
    save(
        hc,
        sid,
        json!({"tracking_number": tracking, "tracking_url": link, "provider_id": at(&answer, c.id_at.as_deref()), "label_file": file_id.to_string(),
               "cost": money(cost), "freight_booked": booked, "status": "Label Bought"}),
    )
    .await?;
    Ok(json!({"tracking_number": tracking, "tracking_url": link, "file": file_id, "cost": money(cost), "already": false}))
}

/// Moves a shipment to what the carrier says, never out of Delivered or Cancelled.
async fn record_status(k: &Kernel, ctx: &Ctx, tx: &mut Tx, ship: &Doc, said: &str, mapped: Option<&str>) -> Result<Value> {
    let sid = id(ship, "id").unwrap_or_default();
    let mut v = json!({"tracking_status": said, "last_tracked_at": chrono::Utc::now().to_rfc3339()});
    let now = s(ship, "status").unwrap_or("Draft");
    if let Some(m) = mapped
        && !matches!(now, "Delivered" | "Cancelled")
    {
        v["status"] = json!(m);
        if m == "Delivered" {
            v["delivered_at"] = json!(chrono::Utc::now().to_rfc3339());
        }
    }
    k.set_values_tx(&sys(ctx), tx, "CarrierShipment", sid, set_fields(v.clone())).await?;
    Ok(v)
}

/// Asks the carrier where a labelled shipment is.
pub async fn track(k: &Kernel, ctx: &Ctx, tx: &mut Tx, sid: Uuid) -> Result<Value> {
    let (ship, conn) = locked(k, ctx, tx, sid).await?;
    if s(&ship, "tracking_number").is_none() {
        return Err(Error::invalid("no label is bought yet"));
    }
    let c = conn.desc.track.clone().ok_or_else(|| Error::invalid("this carrier's connection does not describe tracking"))?;
    let v = vars(k, ctx, tx, &conn, &ship).await?;
    let answer = call(k, &conn, "track", &c, &v).await?;
    let said = at(&answer, c.status_at.as_deref()).unwrap_or_default();
    let mapped = c.statuses.get(&said).map(String::as_str);
    record_status(k, ctx, tx, &ship, &said, mapped).await
}

async fn cancel(hc: &mut HookCtx<'_>, sid: Uuid) -> Result<Value> {
    let (ship, conn) = locked(hc.kernel, hc.ctx, hc.tx, sid).await?;
    match s(&ship, "status") {
        Some("Cancelled") => return Ok(json!({"status": "Cancelled", "already": true})),
        Some("Delivered") => return Err(Error::invalid("the goods are delivered: the label cannot be cancelled")),
        _ if s(&ship, "tracking_number").is_none() => return Err(Error::invalid("no label is bought")),
        _ => {}
    }
    if hc.ctx.schema.entity("SalesInvoiceItem").is_some_and(|e| e.field("carrier_shipment").is_some()) {
        let charged: Option<String> = sqlx::query_scalar(
            "select coalesce(i.number, i.id::text) from e_sales_invoice__items l join e_sales_invoice i on i.tenant_id = l.tenant_id and i.id = l.parent_id \
             where l.tenant_id = $1 and l.carrier_shipment = $2 and i.docstatus < 2 limit 1",
        )
        .bind(hc.ctx.tenant)
        .bind(sid)
        .fetch_optional(&mut **hc.tx)
        .await?;
        if let Some(inv) = charged {
            return Err(Error::invalid(format!("{inv} charges this label to the customer: cancel or credit it first")));
        }
    }
    let c = conn.desc.cancel.clone().ok_or_else(|| Error::invalid("this carrier's connection does not describe cancelling: ask the carrier"))?;
    let v = vars(hc.kernel, hc.ctx, hc.tx, &conn, &ship).await?;
    call(hc.kernel, &conn, "cancel", &c, &v).await?;
    if crate::util::b(&ship, "freight_booked") {
        let co = company(hc.kernel, hc.ctx, hc.tx, id(&ship, "company").unwrap_or_default()).await?;
        ledger::reverse(hc.tx, hc.ctx, "CarrierShipment", sid, hc.ctx.today(), co.precision).await?;
    }
    save(hc, sid, json!({"status": "Cancelled", "freight_booked": false})).await?;
    Ok(json!({"status": "Cancelled", "already": false}))
}

/// `make_carrier_shipment` on a delivery note or a purchase return: a draft shipment of its goods.
pub async fn make_from(hc: &mut HookCtx<'_>, entity: &str, doc: &Doc, params: &Value) -> Result<Value> {
    let text =
        |k: &str| params.get(k).and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_number().map(|n| n.to_string()))).filter(|v| !v.trim().is_empty());
    let num = |k: &str| -> Result<Option<Decimal>> {
        text(k).map(|v| v.trim().parse::<Decimal>().map_err(|_| Error::field(k.to_string(), "is not a number"))).transpose()
    };
    let co = id(doc, "company").ok_or_else(|| Error::internal("a document without its company"))?;
    let carrier = match text("carrier") {
        Some(c) => c.parse::<Uuid>().map_err(|_| Error::field("carrier", "is not a carrier connection"))?,
        None => {
            let found = hc
                .kernel
                .find_tx(&sys(hc.ctx), hc.tx, "CarrierConnection", &[Filter::eq("company", co.to_string()), Filter::eq("enabled", true)], None, 2)
                .await?;
            match found.as_slice() {
                [one] => id(one, "id").unwrap_or_default(),
                [] => return Err(Error::field("carrier", "the company has no enabled carrier connection")),
                _ => return Err(Error::field("carrier", "the company has several: choose one")),
            }
        }
    };
    let weight = num("weight")?.ok_or_else(|| Error::field("weight", "is required"))?;
    let (link, ship_to) = match entity {
        "DeliveryNote" => ("delivery_note", id(doc, "ship_to").or(id(doc, "customer_address"))),
        _ => {
            // A return copies no address: the receipt it returns says where the supplier is.
            let orig = match (id(doc, "supplier_address"), id(doc, "return_against")) {
                (None, Some(o)) => id(&get(hc.kernel, hc.ctx, hc.tx, entity, o).await?, "supplier_address"),
                (a, _) => a,
            };
            ("purchase_receipt", orig)
        }
    };
    let package = json!({"weight": money(weight), "length": num("length")?.map(money), "width": num("width")?.map(money), "height": num("height")?.map(money)});
    let input = json!({"company": co.to_string(), "carrier": carrier.to_string(), "reference": s(doc, "number"), link: s(doc, "id"),
                       "ship_to": ship_to.map(|a| a.to_string()), "packages": [package]});
    let made = create_for_user(hc, "CarrierShipment", set_fields(input)).await?;
    Ok(created("CarrierShipment", &made))
}

/// The label costs a delivery's invoice charges: one line of the connection's freight item for each
/// shipment of the delivery under Charged to Customer that no invoice still standing charges yet.
pub async fn freight_lines(hc: &mut HookCtx<'_>, delivery: Uuid, conversion_rate: Decimal) -> Result<Vec<Value>> {
    if hc.ctx.schema.entity("CarrierShipment").is_none_or(|e| e.field("delivery_note").is_none()) {
        return Ok(vec![]);
    }
    let rows: Vec<(Uuid, Decimal, Uuid)> = sqlx::query_as(
        "select s.id, s.cost, c.freight_item from e_carrier_shipment s join e_carrier_connection c on c.tenant_id = s.tenant_id and c.id = s.carrier \
         where s.tenant_id = $1 and s.delivery_note = $2 and s.freight_booked and s.status <> 'Cancelled' and c.freight = 'Charged to Customer' \
         and c.freight_item is not null and not exists (select 1 from e_sales_invoice__items l join e_sales_invoice i on i.tenant_id = l.tenant_id \
         and i.id = l.parent_id where l.tenant_id = s.tenant_id and l.carrier_shipment = s.id and i.docstatus < 2) order by s.created_at",
    )
    .bind(hc.ctx.tenant)
    .bind(delivery)
    .fetch_all(&mut **hc.tx)
    .await?;
    let rate = if conversion_rate.is_zero() { Decimal::ONE } else { conversion_rate };
    Ok(rows
        .into_iter()
        .map(|(sid, cost, item)| json!({"item": item.to_string(), "qty": "1", "price_list_rate": money((cost / rate).round_dp(9)), "carrier_shipment": sid.to_string()}))
        .collect())
}

/// The upkeep's tracking: labelled shipments whose connection tracks every so many hours and that were
/// last tracked longer ago, at most `limit` of them, each in its own transaction.
pub async fn upkeep(k: &Kernel, ctx: &Ctx, limit: i64) -> Result<usize> {
    if ctx.schema.entity("CarrierShipment").is_none() {
        return Ok(0);
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let due: Vec<Uuid> = sqlx::query_scalar(
        "select s.id from e_carrier_shipment s join e_carrier_connection c on c.tenant_id = s.tenant_id and c.id = s.carrier \
         where s.tenant_id = $1 and s.status in ('Label Bought', 'In Transit', 'Exception') and s.tracking_number is not null and c.enabled \
         and coalesce(c.track_every_hours, 0) > 0 and (s.last_tracked_at is null or s.last_tracked_at < now() - make_interval(hours => c.track_every_hours::int)) \
         order by s.last_tracked_at nulls first limit $2",
    )
    .bind(ctx.tenant)
    .bind(limit)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut done = 0;
    for sid in due {
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        match track(k, ctx, &mut tx, sid).await {
            Ok(_) => {
                tx.commit().await?;
                done += 1;
            }
            Err(e) => tracing::warn!(tenant = %ctx.tenant, shipment = %sid, "tracking: {e}"),
        }
    }
    Ok(done)
}

/// A carrier's tracking call to `/api/site/{workspace}/inventory/carriers/{connection}`, checked as the
/// connection's `inbound` says. Unknown, unsigned or wrongly signed calls are refused and counted.
pub struct CarrierSite;

fn equal(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[async_trait]
impl Site for CarrierSite {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        let segs: Vec<&str> = req.path.split('/').filter(|p| !p.is_empty()).collect();
        let ("POST", ["carriers", conn]) = (req.method.as_str(), segs.as_slice()) else {
            return Err(Error::NotFound(format!("inventory endpoint {} {}", req.method, req.path)));
        };
        let conn: Uuid = conn.parse().map_err(|_| Error::NotFound("carrier connection".into()))?;
        let schema = k.registry.schema(req.tenant).await?;
        if schema.entity("CarrierShipment").is_none() {
            return Err(Error::NotFound("carrier connection".into()));
        }
        let ctx = Ctx::system(req.tenant, schema);
        let mut tx = k.db.tenant_tx(req.tenant).await?;
        let doc = k.get_tx(&ctx, &mut tx, "CarrierConnection", conn, false).await.map_err(|_| Error::NotFound("carrier connection".into()))?;
        let desc: Option<Description> = serde_json::from_value(doc.get("description").cloned().unwrap_or(Value::Null)).ok();
        let Some(inbound) = desc.and_then(|d| d.inbound) else { return Err(Error::NotFound("carrier connection".into())) };
        let secret = k.secret(&mut tx, req.tenant, "CarrierConnection", conn, "webhook_secret").await?;
        let header = |name: &str| req.headers.iter().find(|(h, _)| h.eq_ignore_ascii_case(name)).and_then(|(_, v)| v.as_str()).map(str::trim);
        let signed = match (&secret, inbound.auth.as_str()) {
            (None, _) => false,
            (Some(sec), "hmac") => {
                use hmac::Mac;
                let given = inbound.header.as_deref().and_then(header).map(|h| h.strip_prefix("sha256=").unwrap_or(h).to_lowercase());
                let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(sec.as_bytes()).map_err(Error::internal)?;
                mac.update(&req.body);
                given.is_some_and(|g| equal(g.as_bytes(), hex::encode(mac.finalize().into_bytes()).as_bytes()))
            }
            (Some(sec), _) => {
                let given = inbound.header.as_deref().and_then(header).or(req.query.get("token").and_then(Value::as_str));
                given.is_some_and(|g| equal(g.as_bytes(), sec.as_bytes()))
            }
        };
        if !signed {
            sqlx::query("update e_carrier_connection set refused_calls = coalesce(refused_calls, 0) + 1 where tenant_id = $1 and id = $2")
                .bind(req.tenant)
                .bind(conn)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(SiteResponse::json(401, &json!({"error": "not signed as this connection's inbound secret says"})));
        }
        let body: Value = serde_json::from_slice(&req.body).map_err(|e| Error::invalid(format!("the request body: {e}")))?;
        let (Some(tracking), Some(said)) = (at(&body, Some(&inbound.tracking_at)), at(&body, Some(&inbound.status_at))) else {
            return Err(Error::invalid("the call holds no tracking number or status where the connection says"));
        };
        let ships = k
            .find_tx(&ctx, &mut tx, "CarrierShipment", &[Filter::eq("carrier", conn.to_string()), Filter::eq("tracking_number", tracking.clone())], None, 10)
            .await?;
        let mapped = inbound.statuses.get(&said).map(String::as_str);
        for ship in &ships {
            let ship = k.get_tx(&ctx, &mut tx, "CarrierShipment", id(ship, "id").unwrap_or_default(), true).await?;
            record_status(k, &ctx, &mut tx, &ship, &said, mapped).await?;
        }
        tx.commit().await?;
        Ok(SiteResponse::json(200, &json!({"shipments": ships.len()})))
    }

    fn max_body(&self) -> usize {
        256 << 10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(v: Value) -> Description {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_description_says_what_is_wrong() {
        let ok = json!({"rate": {"url": "https://c.example/rates", "rates_at": "/rates", "service_at": "/service", "amount_at": "/amount"},
                        "label": {"url": "https://c.example/labels", "tracking_at": "/tracking", "label_at": "/label"}});
        assert_eq!(desc(ok.clone()).problem(false), None);
        let mut bad = ok.clone();
        bad["rate"]["amount_at"] = json!("amount");
        assert!(desc(bad).problem(false).unwrap().contains("not a JSON pointer"));
        let mut bad = ok.clone();
        bad["label"]["label_at"] = Value::Null;
        assert!(desc(bad).problem(false).unwrap().contains("label_at or label_url_at"));
        let mut bad = ok.clone();
        bad["track"] = json!({"url": "https://c.example/t", "status_at": "/s", "statuses": {"X": "Lost"}});
        assert!(desc(bad).problem(false).unwrap().contains("not Lost"));
        let mut bad = ok;
        bad["rate"]["body"] = json!("{{ shipment.");
        assert!(desc(bad).problem(false).is_some());
    }

    /// The worked examples in docs/carriers.md are descriptions ERP accepts.
    #[test]
    fn the_documented_examples_are_valid() {
        let doc = include_str!("../docs/carriers.md");
        let blocks: Vec<&str> = doc.split("```json").skip(1).map(|b| b.split("```").next().unwrap_or_default()).collect();
        assert_eq!(blocks.len(), 2);
        for b in blocks {
            let d: Description = serde_json::from_str(b).unwrap_or_else(|e| panic!("{e}: {b}"));
            assert_eq!(d.problem(false), None);
        }
    }

    #[test]
    fn templates_write_json_and_pointers_read_it() {
        let v = json!({"to": {"city": "Springfield \"North\""}, "shipment": {"packages": [{"weight": "1.5"}]}});
        let out = render("{\"city\": {{ to.city|json }}, \"w\": {{ shipment.packages[0].weight }}}", &v).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["city"], "Springfield \"North\"");
        assert_eq!(at(&json!({"a": [{"b": 2.5}]}), Some("/a/0/b")).as_deref(), Some("2.5"));
        assert_eq!(at(&json!({"a": null}), Some("/a")), None);
    }
}
