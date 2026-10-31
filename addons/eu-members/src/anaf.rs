//! Romania's connection to ANAF's e-Factura: the company signs in once with its qualified
//! certificate through ANAF's OAuth (`logincert.anaf.ro/anaf-oauth2/v1`: Connect to ANAF gives
//! the address; ANAF returns the user to `/api/app/ro/anaf/callback`, which takes the tokens, kept
//! sealed and refreshed when they lapse). A queued CIUS-RO invoice is uploaded
//! (`POST {api}/upload?standard=UBL&cif=…`, test or production), ANAF's upload index recorded, and
//! ANAF asked after it (`GET {api}/stareMesaj?id_incarcare=…`) until it says ok, with the download
//! ID of the signed invoice, or nok. With the connection Off, invoices wait for their answer to be
//! recorded by hand.

use crate::util::{get, id, s, sys};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, EventSink, HookCtx, Kernel, Result, Site, SiteRequest, SiteResponse, outbox};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::time::Duration as Wait;
use uuid::Uuid;

pub const STATUS_TOPIC: &str = "ro.efactura.status_requested";
const AUTH: &str = "https://logincert.anaf.ro/anaf-oauth2/v1";
const API_TEST: &str = "https://api.anaf.ro/test/FCTEL/rest";
const API_PRODUCTION: &str = "https://api.anaf.ro/prod/FCTEL/rest";

/// ANAF's sign-in and e-Factura hosts for a company's mode (none when it is Off);
/// `FERRUM_RO_ANAF_URL` stands in for both (`{url}/anaf-oauth2/v1`, `{url}/FCTEL/rest`).
fn hosts(mode: &str) -> Option<(String, String)> {
    if mode == "Off" {
        return None;
    }
    if let Ok(u) = std::env::var("FERRUM_RO_ANAF_URL")
        && !u.trim().is_empty()
    {
        let u = u.trim_end_matches('/');
        return Some((format!("{u}/anaf-oauth2/v1"), format!("{u}/FCTEL/rest")));
    }
    match mode {
        "Test" => Some((AUTH.into(), API_TEST.into())),
        "Production" => Some((AUTH.into(), API_PRODUCTION.into())),
        _ => None,
    }
}

fn pct(s: &str) -> String {
    s.bytes().map(|c| if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~') { (c as char).to_string() } else { format!("%{c:02X}") }).collect()
}

/// An attribute of ANAF's XML answers (`index_incarcare="…"`).
pub fn attr(xml: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let start = xml.find(&key)? + key.len();
    let end = xml[start..].find('"')? + start;
    Some(xml[start..end].to_string())
}

/// The errors in ANAF's answers (`<Errors errorMessage="…"/>`).
pub fn errors(xml: &str) -> Vec<String> {
    xml.match_indices("errorMessage=\"")
        .filter_map(|(i, k)| {
            let start = i + k.len();
            xml[start..].find('"').map(|e| xml[start..start + e].to_string())
        })
        .collect()
}

/// Starts ANAF's sign-in for a company: the address to open, with a state the callback checks.
pub async fn connect(hc: &mut HookCtx<'_>, company: &Doc) -> Result<Value> {
    let cid = id(company, "id").ok_or_else(|| Error::internal("company without id"))?;
    let (auth, _) = hosts(s(company, "ro_anaf_mode").unwrap_or("Off")).ok_or_else(|| Error::invalid("choose Test or Production for the ANAF connection"))?;
    let client_id = s(company, "ro_anaf_client_id").ok_or_else(|| Error::field("ro_anaf_client_id", "is required to connect"))?;
    let redirect = s(company, "ro_anaf_redirect_uri").ok_or_else(|| Error::field("ro_anaf_redirect_uri", "is required to connect"))?;
    let nonce = hex::encode(Sha256::digest(format!("{}:{cid}:{}", Uuid::now_v7(), Utc::now().timestamp_nanos_opt().unwrap_or_default()).as_bytes()));
    let state = format!("{cid}.{nonce}");
    let Value::Object(patch) = json!({"ro_anaf_state": state}) else { return Err(Error::internal("state")) };
    hc.kernel.update_tx(&sys(hc.ctx), hc.tx, "Company", cid, patch, None).await?;
    let url =
        format!("{auth}/authorize?response_type=code&client_id={}&redirect_uri={}&state={}&token_content_type=jwt", pct(client_id), pct(redirect), pct(&state));
    Ok(json!({"url": url, "message": "Open the address, sign in with the company's qualified certificate and allow access; ANAF returns you to Ferrum"}))
}

async fn token_request(client: &reqwest::Client, auth: &str, form: &[(&str, &str)]) -> Result<Value> {
    let reply = client.post(format!("{auth}/token")).form(form).send().await.map_err(|e| Error::internal(format!("ANAF could not be reached: {e}")))?;
    let status = reply.status();
    let v: Value = reply.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(Error::invalid(format!(
            "ANAF refused the authorisation ({status}): {}",
            v["error_description"].as_str().or(v["error"].as_str()).unwrap_or("no reason given")
        )));
    }
    Ok(v)
}

async fn store_tokens(k: &Kernel, ctx: &Ctx, company: Uuid, answer: &Value, connected: bool) -> Result<()> {
    let access = answer["access_token"].as_str().ok_or_else(|| Error::invalid("ANAF's answer has no access token"))?;
    let expires = Utc::now() + Duration::seconds(answer["expires_in"].as_i64().unwrap_or(7_776_000));
    let mut patch = Map::new();
    patch.insert("ro_anaf_access_token".into(), json!(access));
    if let Some(r) = answer["refresh_token"].as_str() {
        patch.insert("ro_anaf_refresh_token".into(), json!(r));
    }
    patch.insert("ro_anaf_token_expires".into(), json!(expires.to_rfc3339()));
    if connected {
        patch.insert("ro_anaf_connected_at".into(), json!(Utc::now().to_rfc3339()));
        patch.insert("ro_anaf_state".into(), Value::Null);
    }
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    k.update_tx(&sys(ctx), &mut tx, "Company", company, patch, None).await?;
    tx.commit().await?;
    Ok(())
}

/// The package's HTTP endpoint: ANAF's sign-in returns here.
pub struct AnafSite;

fn html(status: u16, title: &str, text: &str) -> SiteResponse {
    SiteResponse::text(status, "text/html; charset=utf-8", format!("<!doctype html><meta charset=utf-8><title>{title}</title><h1>{title}</h1><p>{text}</p>"))
}

#[async_trait]
impl Site for AnafSite {
    async fn handle(&self, k: &Kernel, req: SiteRequest) -> Result<SiteResponse> {
        let Some(user) = req.user.clone() else { return Err(Error::Forbidden("sign in to Ferrum first".into())) };
        let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        if (req.method.as_str(), parts.as_slice()) != ("GET", &["anaf", "callback"][..]) {
            return Err(Error::NotFound(format!("ro {}", req.path)));
        }
        let q = |k: &str| req.query.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        if !q("error").is_empty() {
            return Ok(html(400, "ANAF did not connect", &format!("ANAF answered: {}", q("error_description"))));
        }
        let state = q("state");
        let cid: Uuid = state.split('.').next().and_then(|c| c.parse().ok()).ok_or_else(|| Error::invalid("the sign-in's state is not Ferrum's"))?;
        let mut tx = k.db.tenant_tx(req.tenant).await?;
        // The user must be able to change the company, and the state must be the one issued.
        let company = k.get_tx(&user, &mut tx, "Company", cid, false).await?;
        let expected = k.secret(&mut tx, req.tenant, "Company", cid, "ro_anaf_state").await?;
        let secret = k.secret(&mut tx, req.tenant, "Company", cid, "ro_anaf_client_secret").await?;
        tx.commit().await?;
        if expected.as_deref() != Some(state.as_str()) {
            return Ok(html(400, "ANAF did not connect", "This sign-in was not started here, or was already used: start it again with Connect to ANAF."));
        }
        let (auth, _) = hosts(s(&company, "ro_anaf_mode").unwrap_or("Off")).ok_or_else(|| Error::invalid("the company has no ANAF connection"))?;
        let secret = secret.ok_or_else(|| Error::invalid("set the ANAF application's client secret on the company"))?;
        let (client_id, redirect) = (s(&company, "ro_anaf_client_id").unwrap_or_default(), s(&company, "ro_anaf_redirect_uri").unwrap_or_default());
        let client = reqwest::Client::builder().timeout(Wait::from_secs(30)).build().map_err(Error::internal)?;
        let code = q("code");
        let answer = token_request(
            &client,
            &auth,
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", client_id),
                ("client_secret", &secret),
                ("redirect_uri", redirect),
                ("token_content_type", "jwt"),
            ],
        )
        .await?;
        store_tokens(k, &user, cid, &answer, true).await?;
        Ok(html(200, "Connected to ANAF", "Ferrum can now upload this company's e-invoices to e-Factura. You can close this window."))
    }
}

/// The company's Connect to ANAF action.
pub struct AnafHook;

#[async_trait]
impl DocHook for AnafHook {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if (hc.entity.def.name.as_str(), name) != ("Company", "ro_connect_anaf") {
            return Ok(None);
        }
        connect(hc, doc).await.map(Some)
    }
}

/// The connector.
pub struct Anaf {
    client: reqwest::Client,
}

impl Default for Anaf {
    fn default() -> Self {
        Anaf { client: reqwest::Client::builder().timeout(Wait::from_secs(60)).build().unwrap_or_default() }
    }
}

impl Anaf {
    async fn record(k: &Kernel, ctx: &Ctx, invoice: Uuid, values: Value) -> Result<()> {
        let Value::Object(v) = values else { return Ok(()) };
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        k.set_values_tx(&sys(ctx), &mut tx, "SalesInvoice", invoice, v).await?;
        tx.commit().await?;
        Ok(())
    }

    /// A usable access token: the stored one while it lasts, else a refreshed one.
    async fn token(&self, k: &Kernel, ctx: &Ctx, auth: &str, company: &Doc, force: bool) -> Result<String> {
        let cid = id(company, "id").ok_or_else(|| Error::internal("company without id"))?;
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let access = k.secret(&mut tx, ctx.tenant, "Company", cid, "ro_anaf_access_token").await?;
        let refresh = k.secret(&mut tx, ctx.tenant, "Company", cid, "ro_anaf_refresh_token").await?;
        let secret = k.secret(&mut tx, ctx.tenant, "Company", cid, "ro_anaf_client_secret").await?;
        tx.commit().await?;
        let fresh = s(company, "ro_anaf_token_expires")
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| t > Utc::now() + Duration::seconds(60));
        if let (Some(a), true, false) = (&access, fresh, force) {
            return Ok(a.clone());
        }
        let (Some(refresh), Some(secret)) = (refresh, secret) else { return Err(Error::invalid("connect the company to ANAF first (Connect to ANAF)")) };
        let client_id = s(company, "ro_anaf_client_id").unwrap_or_default();
        let answer = token_request(
            &self.client,
            auth,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh),
                ("client_id", client_id),
                ("client_secret", &secret),
                ("token_content_type", "jwt"),
            ],
        )
        .await?;
        store_tokens(k, ctx, cid, &answer, false).await?;
        Ok(answer["access_token"].as_str().unwrap_or_default().to_string())
    }
}

#[async_trait]
impl EventSink for Anaf {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic != crate::ro::UPLOAD_TOPIC && ev.topic != STATUS_TOPIC {
            return Ok(());
        }
        let Some(invoice) = ev.record_id else { return Ok(()) };
        let ctx = Ctx::system(ev.tenant_id, k.registry.schema(ev.tenant_id).await?);
        let Some(cid) = ev.payload["company"].as_str().and_then(|c| c.parse::<Uuid>().ok()) else { return Ok(()) };
        let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
        let company = get(k, &ctx, &mut tx, "Company", cid).await?;
        tx.commit().await?;
        // Without a connection, the invoice waits for its answer to be recorded by hand.
        let Some((auth, api)) = hosts(s(&company, "ro_anaf_mode").unwrap_or("Off")) else { return Ok(()) };
        let mut forced = false;
        loop {
            let token = match self.token(k, &ctx, &auth, &company, forced).await {
                Ok(t) => t,
                Err(Error::Invalid(m)) => return Self::record(k, &ctx, invoice, json!({"ro_efactura_error": m})).await,
                Err(e) => return Err(e),
            };
            let reached = |e: reqwest::Error| Error::internal(format!("ANAF could not be reached: {e}"));
            let (status, body) = if ev.topic == crate::ro::UPLOAD_TOPIC {
                let cif = ev.payload["cif"].as_str().unwrap_or_default();
                let reply = self
                    .client
                    .post(format!("{api}/upload?standard=UBL&cif={}", pct(cif)))
                    .bearer_auth(&token)
                    .header("Content-Type", "text/plain")
                    .body(ev.payload["xml"].as_str().unwrap_or_default().to_string())
                    .send()
                    .await
                    .map_err(reached)?;
                (reply.status().as_u16(), reply.text().await.unwrap_or_default())
            } else {
                let index = ev.payload["index"].as_str().unwrap_or_default();
                let reply = self.client.get(format!("{api}/stareMesaj?id_incarcare={}", pct(index))).bearer_auth(&token).send().await.map_err(reached)?;
                (reply.status().as_u16(), reply.text().await.unwrap_or_default())
            };
            match status {
                401 if !forced => forced = true,
                429 | 500.. => return Err(Error::internal(format!("ANAF is busy ({status}); trying again"))),
                200 if ev.topic == crate::ro::UPLOAD_TOPIC => {
                    let index = attr(&body, "index_incarcare").filter(|_| attr(&body, "ExecutionStatus").as_deref() == Some("0"));
                    let Some(index) = index else {
                        let why = errors(&body).join("; ");
                        return Self::record(k, &ctx, invoice, json!({"ro_efactura_status": "Rejected", "ro_efactura_error": if why.is_empty() { "ANAF refused the upload".to_string() } else { why }})).await;
                    };
                    Self::record(k, &ctx, invoice, json!({"ro_efactura_status": "Uploaded", "ro_upload_index": index, "ro_efactura_error": null})).await?;
                    let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
                    outbox::enqueue(
                        &mut tx,
                        ev.tenant_id,
                        STATUS_TOPIC,
                        Some("SalesInvoice"),
                        Some(invoice),
                        json!({"invoice": invoice, "company": cid, "index": index}),
                    )
                    .await?;
                    tx.commit().await?;
                    return Ok(());
                }
                200 => {
                    return match attr(&body, "stare").as_deref() {
                        Some("ok") => {
                            Self::record(
                                k,
                                &ctx,
                                invoice,
                                json!({"ro_efactura_status": "Validated", "ro_download_id": attr(&body, "id_descarcare"), "ro_efactura_error": null}),
                            )
                            .await
                        }
                        Some("nok") => {
                            let why = errors(&body).join("; ");
                            Self::record(
                                k,
                                &ctx,
                                invoice,
                                json!({"ro_efactura_status": "Rejected", "ro_download_id": attr(&body, "id_descarcare"),
                                       "ro_efactura_error": if why.is_empty() { "ANAF found errors: download its answer for them".to_string() } else { why }}),
                            )
                            .await
                        }
                        Some("XML cu erori nepreluat de sistem") => {
                            Self::record(
                                k,
                                &ctx,
                                invoice,
                                json!({"ro_efactura_status": "Rejected", "ro_efactura_error": "ANAF did not take the XML (it has errors)"}),
                            )
                            .await
                        }
                        // in prelucrare: still being processed; asked again later.
                        _ => Err(Error::internal("ANAF is still processing the invoice; asking again")),
                    };
                }
                _ => {
                    let why = errors(&body).join("; ");
                    return Self::record(k, &ctx, invoice, json!({"ro_efactura_error": format!("ANAF answered {status}: {}", if why.is_empty() { body.chars().take(300).collect() } else { why })})).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anafs_answers() {
        let upload = r#"<?xml version="1.0" encoding="UTF-8"?><header xmlns="mfp:anaf:dgti:spv:respUploadFisier:v1" dateResponse="202611011200" ExecutionStatus="0" index_incarcare="5004321"/>"#;
        assert_eq!((attr(upload, "index_incarcare").as_deref(), attr(upload, "ExecutionStatus").as_deref()), (Some("5004321"), Some("0")));
        let refused = r#"<header ExecutionStatus="1"><Errors errorMessage="Fisierul transmis nu este valid."/><Errors errorMessage="CIF invalid"/></header>"#;
        assert_eq!(errors(refused), vec!["Fisierul transmis nu este valid.", "CIF invalid"]);
        assert_eq!(attr(refused, "index_incarcare"), None);
    }
}
