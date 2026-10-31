//! VAT returns through HMRC's Making Tax Digital API (VAT (MTD) 1.0, hmrc/vat-api): a return's
//! nine boxes are worked out from the company's submitted invoices for the period; submitting it
//! (with HMRC's declaration) queues it in the outbox, and the connector files it with
//! `POST /organisations/vat/{vrn}/returns` under the company's OAuth authorisation (refreshed when
//! it lapses), recording HMRC's receipt or its refusal.
//!
//! Connecting: `Connect to HMRC` gives the address of HMRC's sign-in; HMRC sends the user back to
//! `/api/app/gb/hmrc/callback`, which takes the tokens. HMRC requires fraud prevention headers
//! (WEB_APP_VIA_SERVER) with every call: filing through `/api/app/gb/returns/{id}/file` gives the
//! browser's details with the request; a return submitted otherwise carries what the server knows.

use crate::util::{d, get, id, money, s, set, sys};
use crate::vat::{INPUT_TAX, OUTPUT_TAX, account};
use async_trait::async_trait;
use chrono::{Duration, NaiveDate, Utc};
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, EventSink, HookCtx, Kernel, Result, Site, SiteRequest, SiteResponse, Tx, outbox};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration as Wait;
use uuid::Uuid;

pub const TOPIC: &str = "gb.mtd.submit_requested";
const SANDBOX: (&str, &str) = ("https://test-api.service.hmrc.gov.uk", "https://test-www.tax.service.gov.uk");
const PRODUCTION: (&str, &str) = ("https://api.service.hmrc.gov.uk", "https://www.tax.service.gov.uk");
const ACCEPT: &str = "application/vnd.hmrc.1.0+json";

/// HMRC's API and sign-in hosts for a company's mode; `FERRUM_GB_HMRC_URL` stands in for both.
fn hosts(mode: &str) -> Option<(String, String)> {
    if let Ok(u) = std::env::var("FERRUM_GB_HMRC_URL")
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

// ───────────────────────────── the nine boxes ─────────────────────────────

fn whole_pounds(v: Decimal) -> Decimal {
    v.round_dp_with_strategy(0, RoundingStrategy::ToZero)
}

fn pence(v: Decimal) -> Decimal {
    v.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
}

/// A period's nine boxes from the company's submitted invoices (in its currency, sterling):
/// VAT on sales and on purchases the company reverse-charges itself (1), VAT reclaimed (4), the
/// net (5, never negative: HMRC reads its direction from boxes 3 and 4), and sales and purchases
/// without VAT in whole pounds (6, 7). Boxes 2, 8 and 9 are Northern Ireland's trade with the EU.
pub async fn boxes(k: &Kernel, ctx: &Ctx, tx: &mut Tx, company: Uuid, from: NaiveDate, to: NaiveDate) -> Result<[Decimal; 9]> {
    let output = account(k, ctx, tx, company, OUTPUT_TAX).await?.parse::<Uuid>().map_err(Error::internal)?;
    let input = account(k, ctx, tx, company, INPUT_TAX).await?.parse::<Uuid>().map_err(Error::internal)?;
    let tax_sum = async |tx: &mut Tx, entity: &str, acc: Uuid, reverse_only: bool| -> Result<Decimal> {
        let parent = ferrum_meta::table_for(entity);
        let taxes = ferrum_kernel::ddl::q(&ferrum_meta::child_table(&parent, "taxes"));
        let parent = ferrum_kernel::ddl::q(&parent);
        let v: Option<Decimal> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select sum(case when coalesce(t.deduct, false) then -t.base_tax_amount else t.base_tax_amount end) from {parent} i join {taxes} t on t.tenant_id = i.tenant_id and t.parent_id = i.id \
             where i.tenant_id = $1 and i.company = $2 and i.docstatus = 1 and i.posting_date between $3 and $4 and t.account = $5 and ($6 = false or coalesce(t.reverse_charge, false))"
        )))
        .bind(ctx.tenant)
        .bind(company)
        .bind(from)
        .bind(to)
        .bind(acc)
        .bind(reverse_only)
        .fetch_one(&mut **tx)
        .await?;
        Ok(v.unwrap_or_default())
    };
    let net_sum = async |tx: &mut Tx, entity: &str| -> Result<Decimal> {
        let parent = ferrum_kernel::ddl::q(&ferrum_meta::table_for(entity));
        let v: Option<Decimal> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select sum(i.base_net_total) from {parent} i where i.tenant_id = $1 and i.company = $2 and i.docstatus = 1 and i.posting_date between $3 and $4"
        )))
        .bind(ctx.tenant)
        .bind(company)
        .bind(from)
        .bind(to)
        .fetch_one(&mut **tx)
        .await?;
        Ok(v.unwrap_or_default())
    };
    let box1 = pence(tax_sum(tx, "SalesInvoice", output, false).await? + tax_sum(tx, "PurchaseInvoice", input, true).await?);
    let box4 = pence(tax_sum(tx, "PurchaseInvoice", input, false).await?);
    let box3 = box1;
    Ok([
        box1,
        Decimal::ZERO,
        box3,
        box4,
        (box3 - box4).abs(),
        whole_pounds(net_sum(tx, "SalesInvoice").await?),
        whole_pounds(net_sum(tx, "PurchaseInvoice").await?),
        Decimal::ZERO,
        Decimal::ZERO,
    ])
}

/// HMRC's body for a return (hmrc/vat-api's VATReturnDeclaration).
pub fn body(period_key: &str, b: &[Decimal; 9]) -> Value {
    let n = |v: Decimal| json!(v.to_string().parse::<f64>().unwrap_or_default());
    json!({
        "periodKey": period_key, "vatDueSales": n(b[0]), "vatDueAcquisitions": n(b[1]), "totalVatDue": n(b[2]), "vatReclaimedCurrPeriod": n(b[3]),
        "netVatDue": n(b[4]), "totalValueSalesExVAT": n(b[5]), "totalValuePurchasesExVAT": n(b[6]), "totalValueGoodsSuppliedExVAT": n(b[7]),
        "totalAcquisitionsExVAT": n(b[8]), "finalised": true,
    })
}

// ───────────────────────────── fraud prevention headers ─────────────────────────────

/// Percent-encoding for HMRC's key-value headers (unreserved characters kept).
pub fn pct(s: &str) -> String {
    s.bytes().map(|c| if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~') { (c as char).to_string() } else { format!("%{c:02X}") }).collect()
}

/// A stable device ID for a user of a workspace (HMRC: generated by the app, kept, never expiring).
fn device_id(tenant: Uuid, user: Uuid) -> String {
    let h = Sha256::digest(format!("ferrum-gb-device:{tenant}:{user}").as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&h[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    Uuid::from_bytes(b).to_string()
}

/// The WEB_APP_VIA_SERVER headers from what was captured when the return was filed (`client`:
/// the browser's details and the request's), with the vendor's own.
pub fn fraud_headers(client: &Value, tenant: Uuid) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = vec![("Gov-Client-Connection-Method".into(), "WEB_APP_VIA_SERVER".into())];
    let text = |k: &str| client.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let mut put = |name: &str, v: Option<String>| {
        if let Some(v) = v {
            h.push((name.to_string(), v));
        }
    };
    put("Gov-Client-Browser-JS-User-Agent", text("js_user_agent").or_else(|| text("user_agent")));
    put("Gov-Client-Device-ID", text("device_id"));
    put("Gov-Client-Multi-Factor", text("multi_factor"));
    put("Gov-Client-Public-IP", text("public_ip"));
    put("Gov-Client-Public-IP-Timestamp", text("public_ip_timestamp"));
    put("Gov-Client-Public-Port", text("public_port"));
    put("Gov-Client-Screens", text("screens"));
    put("Gov-Client-Timezone", text("timezone"));
    put("Gov-Client-User-IDs", text("user").map(|u| format!("ferrum={}", pct(&u))));
    put("Gov-Client-Window-Size", text("window_size"));
    put("Gov-Vendor-Forwarded", text("forwarded"));
    let license = hex::encode(Sha256::digest(format!("ferrum-gb-license:{tenant}").as_bytes()));
    put("Gov-Vendor-License-IDs", Some(format!("ferrum={license}")));
    put("Gov-Vendor-Product-Name", Some("Ferrum".into()));
    put("Gov-Vendor-Public-IP", text("vendor_ip").or_else(|| std::env::var("FERRUM_PUBLIC_IP").ok()));
    put("Gov-Vendor-Version", Some(format!("ferrum-gb={}", env!("CARGO_PKG_VERSION"))));
    h
}

/// What the server knows of the user filing a return.
fn server_client(ctx: &Ctx) -> Value {
    json!({"user": ctx.email, "device_id": device_id(ctx.tenant, ctx.user), "public_ip_timestamp": Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()})
}

// ───────────────────────────── the return document ─────────────────────────────

pub struct VatReturnHook;

impl VatReturnHook {
    async fn queue(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let rid = id(doc, "id").ok_or_else(|| Error::internal("return without id"))?;
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let company = get(hc.kernel, &sys(hc.ctx), hc.tx, "Company", cid).await?;
        let vrn = s(&company, "gb_vrn").ok_or_else(|| Error::invalid("set the company's VAT registration number before filing"))?.to_string();
        let b: [Decimal; 9] = std::array::from_fn(|i| d(doc, &format!("box{}", i + 1)));
        let client = doc.get("client_data").cloned().filter(|v| v.is_object()).unwrap_or_else(|| server_client(hc.ctx));
        let event = json!({"return": rid, "company": cid, "vrn": vrn, "body": body(s(doc, "period_key").unwrap_or_default(), &b), "client": client});
        outbox::enqueue(hc.tx, hc.ctx.tenant, TOPIC, Some("GbVatReturn"), Some(rid), event).await
    }
}

#[async_trait]
impl DocHook for VatReturnHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 {
            return Ok(());
        }
        let (Some(cid), Some(from), Some(to)) = (id(doc, "company"), s(doc, "from_date"), s(doc, "to_date")) else { return Ok(()) };
        let (Ok(from), Ok(to)) = (NaiveDate::parse_from_str(from, "%Y-%m-%d"), NaiveDate::parse_from_str(to, "%Y-%m-%d")) else { return Ok(()) };
        let b = boxes(hc.kernel, &sys(hc.ctx), hc.tx, cid, from, to).await?;
        for (i, v) in b.iter().enumerate() {
            set(doc, &format!("box{}", i + 1), money(*v));
        }
        Ok(())
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let company = get(hc.kernel, &sys(hc.ctx), hc.tx, "Company", cid).await?;
        if s(&company, "gb_vrn").is_none() {
            return Err(Error::invalid("set the company's VAT registration number before filing"));
        }
        if s(&company, "gb_hmrc_mode").unwrap_or("Off") != "Off" {
            set(doc, "hmrc_status", "Pending");
            set(doc, "hmrc_error", Value::Null);
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if s(doc, "hmrc_status") == Some("Pending") {
            Self::queue(hc, doc).await?;
        }
        Ok(())
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if matches!(s(doc, "hmrc_status"), Some("Filed" | "Pending")) {
            return Err(Error::invalid(
                "HMRC keeps a filed return: correct its figures in a later return (or, past HMRC's limits, by an error correction notification)",
            ));
        }
        Ok(())
    }
}

// ───────────────────────────── connecting ─────────────────────────────

/// Starts HMRC's sign-in for a company: the address to open, with a state the callback checks.
pub(crate) async fn connect(hc: &mut HookCtx<'_>, company: &Doc) -> Result<Value> {
    let cid = id(company, "id").ok_or_else(|| Error::internal("company without id"))?;
    let (_, www) =
        hosts(s(company, "gb_hmrc_mode").unwrap_or("Off")).ok_or_else(|| Error::invalid("choose Sandbox or Production under File VAT Returns with HMRC"))?;
    let client_id = s(company, "gb_hmrc_client_id").ok_or_else(|| Error::field("gb_hmrc_client_id", "is required to connect"))?;
    let redirect = s(company, "gb_hmrc_redirect_uri").ok_or_else(|| Error::field("gb_hmrc_redirect_uri", "is required to connect"))?;
    let nonce = hex::encode(Sha256::digest(format!("{}:{}:{}", Uuid::now_v7(), cid, Utc::now().timestamp_nanos_opt().unwrap_or_default()).as_bytes()));
    let state = format!("{cid}.{nonce}");
    let Value::Object(patch) = json!({"gb_hmrc_state": state}) else { return Err(Error::internal("state")) };
    hc.kernel.update_tx(&sys(hc.ctx), hc.tx, "Company", cid, patch, None).await?;
    let url = format!(
        "{www}/oauth/authorize?response_type=code&client_id={}&scope={}&state={}&redirect_uri={}",
        pct(client_id),
        pct("read:vat write:vat"),
        pct(&state),
        pct(redirect)
    );
    Ok(json!({"url": url, "message": "Open the address, sign in to HMRC and grant access; HMRC returns you to Ferrum"}))
}

/// Stores tokens from HMRC's token endpoint answer.
async fn store_tokens(k: &Kernel, ctx: &Ctx, company: Uuid, answer: &Value, connected: bool) -> Result<()> {
    let access = answer["access_token"].as_str().ok_or_else(|| Error::invalid("HMRC's answer has no access token"))?;
    let expires = Utc::now() + Duration::seconds(answer["expires_in"].as_i64().unwrap_or(14_400));
    let mut patch = Map::new();
    patch.insert("gb_hmrc_access_token".into(), json!(access));
    if let Some(r) = answer["refresh_token"].as_str() {
        patch.insert("gb_hmrc_refresh_token".into(), json!(r));
    }
    patch.insert("gb_hmrc_token_expires".into(), json!(expires.to_rfc3339()));
    if connected {
        patch.insert("gb_hmrc_connected_at".into(), json!(Utc::now().to_rfc3339()));
        patch.insert("gb_hmrc_state".into(), Value::Null);
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    k.update_tx(&sys(ctx), &mut tx, "Company", company, patch, None).await?;
    tx.commit().await?;
    Ok(())
}

async fn token_request(client: &reqwest::Client, api: &str, form: &[(&str, &str)]) -> Result<Value> {
    let reply = client.post(format!("{api}/oauth/token")).form(form).send().await.map_err(|e| Error::internal(format!("HMRC could not be reached: {e}")))?;
    let status = reply.status();
    let v: Value = reply.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(Error::invalid(format!(
            "HMRC refused the authorisation ({status}): {}",
            v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("no reason given")
        )));
    }
    Ok(v)
}

/// The package's HTTP endpoints.
pub struct HmrcSite;

fn html(status: u16, title: &str, text: &str) -> SiteResponse {
    SiteResponse::text(status, "text/html; charset=utf-8", format!("<!doctype html><meta charset=utf-8><title>{title}</title><h1>{title}</h1><p>{text}</p>"))
}

#[async_trait]
impl Site for HmrcSite {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        let Some(user) = req.user.clone() else { return Err(Error::Forbidden("sign in to Ferrum first".into())) };
        let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        match (req.method.as_str(), parts.as_slice()) {
            ("GET", ["hmrc", "callback"]) => {
                let q = |k: &str| req.query.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
                if !q("error").is_empty() {
                    return Ok(html(400, "HMRC did not connect", &format!("HMRC answered: {}", q("error_description"))));
                }
                let state = q("state");
                let cid: Uuid = state.split('.').next().and_then(|c| c.parse().ok()).ok_or_else(|| Error::invalid("the sign-in's state is not Ferrum's"))?;
                let mut tx = k.db.tenant_tx(req.tenant).await?;
                // The user must be able to change the company, and the state must be the one issued.
                let company = k.get_tx(&user, &mut tx, "Company", cid, false).await?;
                let expected = k.secret(&mut tx, req.tenant, "Company", cid, "gb_hmrc_state").await?;
                let secret = k.secret(&mut tx, req.tenant, "Company", cid, "gb_hmrc_client_secret").await?;
                tx.commit().await?;
                if expected.as_deref() != Some(state.as_str()) {
                    return Ok(html(
                        400,
                        "HMRC did not connect",
                        "This sign-in was not started here, or was already used: start it again with Connect to HMRC.",
                    ));
                }
                let (api, _) = hosts(s(&company, "gb_hmrc_mode").unwrap_or("Off")).ok_or_else(|| Error::invalid("the company files no returns with HMRC"))?;
                let (client_id, redirect) = (s(&company, "gb_hmrc_client_id").unwrap_or_default(), s(&company, "gb_hmrc_redirect_uri").unwrap_or_default());
                let secret = secret.ok_or_else(|| Error::invalid("set the HMRC application's client secret on the company"))?;
                let code = q("code");
                let client = reqwest::Client::builder().timeout(Wait::from_secs(30)).build().map_err(Error::internal)?;
                let answer = token_request(
                    &client,
                    &api,
                    &[("grant_type", "authorization_code"), ("client_id", client_id), ("client_secret", &secret), ("redirect_uri", redirect), ("code", &code)],
                )
                .await?;
                store_tokens(k, &user, cid, &answer, true).await?;
                Ok(html(200, "Connected to HMRC", "Ferrum can now file this company's VAT returns. You can close this window."))
            }
            ("POST", ["returns", rid, "file"]) => {
                // Filing from the browser: its details for HMRC's fraud prevention headers.
                let rid: Uuid = rid.parse().map_err(|_| Error::NotFound("VAT return".into()))?;
                let sent: Value = serde_json::from_slice(&req.body).unwrap_or(json!({}));
                let header = |k: &str| req.headers.get(k).and_then(Value::as_str).map(str::to_string);
                let mut client = server_client(&user);
                for key in ["js_user_agent", "screens", "window_size", "timezone", "device_id", "multi_factor", "public_port"] {
                    if let Some(v) = sent.get(key).and_then(Value::as_str).filter(|v| !v.trim().is_empty()) {
                        client[key] = json!(v);
                    }
                }
                client["user_agent"] = json!(header("user-agent"));
                client["public_ip"] = json!(req.client_ip);
                let Value::Object(patch) = json!({"client_data": client}) else { return Err(Error::internal("client")) };
                let mut tx = k.db.tenant_tx(req.tenant).await?;
                k.set_values_tx(&sys(&user), &mut tx, "GbVatReturn", rid, patch).await?;
                tx.commit().await?;
                let filed = k.submit(&user, "GbVatReturn", rid, None).await?;
                Ok(SiteResponse::json(200, &json!({"status": filed.get("hmrc_status")})))
            }
            _ => Err(Error::NotFound(format!("gb {}", req.path))),
        }
    }
}

// ───────────────────────────── the connector ─────────────────────────────

pub struct Mtd {
    client: reqwest::Client,
}

impl Default for Mtd {
    fn default() -> Self {
        Mtd { client: reqwest::Client::builder().timeout(Wait::from_secs(60)).build().unwrap_or_default() }
    }
}

impl Mtd {
    async fn record(k: &Kernel, ctx: &Ctx, rid: Uuid, values: Value) -> Result<()> {
        let Value::Object(v) = values else { return Ok(()) };
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        k.set_values_tx(&sys(ctx), &mut tx, "GbVatReturn", rid, v).await?;
        tx.commit().await?;
        Ok(())
    }

    /// A usable access token: the stored one while it lasts, else a refreshed one.
    async fn token(&self, k: &Kernel, ctx: &Ctx, api: &str, company: &Doc, force: bool) -> Result<String> {
        let cid = id(company, "id").ok_or_else(|| Error::internal("company without id"))?;
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let access = k.secret(&mut tx, ctx.tenant, "Company", cid, "gb_hmrc_access_token").await?;
        let refresh = k.secret(&mut tx, ctx.tenant, "Company", cid, "gb_hmrc_refresh_token").await?;
        let secret = k.secret(&mut tx, ctx.tenant, "Company", cid, "gb_hmrc_client_secret").await?;
        tx.commit().await?;
        let fresh = s(company, "gb_hmrc_token_expires")
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| t > Utc::now() + Duration::seconds(60));
        if let (Some(a), true, false) = (&access, fresh, force) {
            return Ok(a.clone());
        }
        let (Some(refresh), Some(secret)) = (refresh, secret) else { return Err(Error::invalid("connect the company to HMRC first (Connect to HMRC)")) };
        let client_id = s(company, "gb_hmrc_client_id").unwrap_or_default();
        let answer = token_request(
            &self.client,
            api,
            &[("grant_type", "refresh_token"), ("client_id", client_id), ("client_secret", &secret), ("refresh_token", &refresh)],
        )
        .await?;
        store_tokens(k, ctx, cid, &answer, false).await?;
        Ok(answer["access_token"].as_str().unwrap_or_default().to_string())
    }
}

#[async_trait]
impl EventSink for Mtd {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic != TOPIC {
            return Ok(());
        }
        let Some(rid) = ev.record_id else { return Ok(()) };
        let ctx = Ctx::system(ev.tenant_id, k.registry.schema(ev.tenant_id).await?);
        let Some(cid) = ev.payload["company"].as_str().and_then(|c| c.parse::<Uuid>().ok()) else { return Ok(()) };
        let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
        let company = get(k, &ctx, &mut tx, "Company", cid).await?;
        tx.commit().await?;
        let Some((api, _)) = hosts(s(&company, "gb_hmrc_mode").unwrap_or("Off")) else {
            return Self::record(k, &ctx, rid, json!({"hmrc_status": "Failed", "hmrc_error": "the company files no returns with HMRC (its mode is Off)"}))
                .await;
        };
        let vrn = ev.payload["vrn"].as_str().unwrap_or_default();
        let headers = fraud_headers(&ev.payload["client"], ev.tenant_id);
        let mut forced = false;
        loop {
            let token = match self.token(k, &ctx, &api, &company, forced).await {
                Ok(t) => t,
                Err(Error::Invalid(m)) => return Self::record(k, &ctx, rid, json!({"hmrc_status": "Failed", "hmrc_error": m})).await,
                Err(e) => return Err(e),
            };
            let mut call =
                self.client.post(format!("{api}/organisations/vat/{vrn}/returns")).header("Accept", ACCEPT).bearer_auth(&token).json(&ev.payload["body"]);
            for (name, value) in &headers {
                call = call.header(name.as_str(), value.as_str());
            }
            let reply = call.send().await.map_err(|e| Error::internal(format!("HMRC could not be reached: {e}")))?;
            let status = reply.status().as_u16();
            let receipt = reply.headers().get("Receipt-ID").and_then(|v| v.to_str().ok()).map(str::to_string);
            let v: Value = reply.json().await.unwrap_or(Value::Null);
            match status {
                201 => {
                    return Self::record(
                        k,
                        &ctx,
                        rid,
                        json!({"hmrc_status": "Filed", "processing_date": v["processingDate"], "form_bundle_number": v["formBundleNumber"],
                               "charge_ref_number": v["chargeRefNumber"], "payment_indicator": v["paymentIndicator"], "receipt_id": receipt, "hmrc_error": null}),
                    )
                    .await;
                }
                401 if !forced => forced = true,
                429 | 500.. => return Err(Error::internal(format!("HMRC is busy ({status}); trying again"))),
                _ => {
                    let why = format!("{} {}", v["code"].as_str().unwrap_or(&status.to_string()), v["message"].as_str().unwrap_or("")).trim().to_string();
                    return Self::record(k, &ctx, rid, json!({"hmrc_status": "Failed", "hmrc_error": format!("HMRC refused the return: {why}")})).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmrcs_body_and_headers() {
        let b = [
            Decimal::new(12500, 2),
            Decimal::ZERO,
            Decimal::new(12500, 2),
            Decimal::new(14000, 2),
            Decimal::new(1500, 2),
            Decimal::new(1350, 0),
            Decimal::new(700, 0),
            Decimal::ZERO,
            Decimal::ZERO,
        ];
        let v = body("26A1", &b);
        assert_eq!(
            (v["periodKey"].clone(), v["vatDueSales"].clone(), v["netVatDue"].clone(), v["finalised"].clone()),
            (json!("26A1"), json!(125.0), json!(15.0), json!(true))
        );
        assert_eq!(whole_pounds(Decimal::new(135099, 2)), Decimal::new(1350, 0), "boxes 6 to 9 are whole pounds");
        assert_eq!(pct("a b@c.d/é"), "a%20b%40c.d%2F%C3%A9");
        let h = fraud_headers(
            &json!({"user": "anna@x.test", "screens": "width=1920&height=1080&scaling-factor=1&colour-depth=24", "public_ip": "198.51.100.7"}),
            Uuid::nil(),
        );
        let get = |n: &str| h.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
        assert_eq!(get("Gov-Client-Connection-Method"), Some("WEB_APP_VIA_SERVER"));
        assert_eq!(get("Gov-Client-User-IDs"), Some("ferrum=anna%40x.test"));
        assert_eq!(get("Gov-Client-Public-IP"), Some("198.51.100.7"));
        assert!(get("Gov-Vendor-Version").unwrap().starts_with("ferrum-gb="));
        assert_eq!(device_id(Uuid::nil(), Uuid::nil()), device_id(Uuid::nil(), Uuid::nil()), "the same device ID every time");
        assert_eq!(device_id(Uuid::nil(), Uuid::nil()).len(), 36);
    }
}
