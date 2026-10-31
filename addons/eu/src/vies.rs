//! VIES: whether a customer's VAT number is registered in its member state, asked of the
//! Commission's VIES (its REST API, `POST /check-vat-number`) through the outbox, so no save waits
//! on it. With a requester (one of the workspace's companies' VAT numbers) VIES answers with a
//! consultation number, which proves the check and goes on the invoices that rely on it.

use crate::util::{find, s, sys};
use crate::vat::{iso_code, vies_code};
use async_trait::async_trait;
use chrono::Utc;
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, Doc, Error, EventSink, Filter, HookCtx, Kernel, Result, outbox};
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

pub const TOPIC: &str = "eu.vies.requested";
const DEFAULT_URL: &str = "https://ec.europa.eu/taxation_customs/vies/rest-api";
/// VIES's errors that pass: a member state's service or VIES itself is busy or down.
const PASSING: &[&str] = &["MS_UNAVAILABLE", "SERVICE_UNAVAILABLE", "TIMEOUT", "MS_MAX_CONCURRENT_REQ", "GLOBAL_MAX_CONCURRENT_REQ", "SERVER_BUSY"];

/// VIES's address: the Commission's, or `FERRUM_EU_VIES_URL` (a proxy, a test double).
fn base_url() -> String {
    std::env::var("FERRUM_EU_VIES_URL").ok().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| DEFAULT_URL.into()).trim_end_matches('/').to_string()
}

/// Queues a check of the customer's VAT number, on behalf of `company` (else the first of the
/// workspace's companies with an EU VAT number).
pub(crate) async fn request(hc: &mut HookCtx<'_>, customer: Uuid, company: Option<Uuid>) -> Result<()> {
    outbox::enqueue(hc.tx, hc.ctx.tenant, TOPIC, Some("Customer"), Some(customer), json!({"customer": customer, "company": company})).await
}

/// A VIES text, without its "---" for "not given".
fn text(v: &Value) -> Value {
    match v.as_str().map(str::trim) {
        Some(t) if !t.is_empty() && t != "---" => json!(t),
        _ => Value::Null,
    }
}

pub struct Vies {
    client: reqwest::Client,
}

impl Default for Vies {
    fn default() -> Self {
        Vies { client: reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default() }
    }
}

impl Vies {
    async fn record(k: &Kernel, ctx: &Ctx, customer: Uuid, values: Value) -> Result<()> {
        let Value::Object(v) = values else { return Ok(()) };
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        k.set_values_tx(&sys(ctx), &mut tx, "Customer", customer, v).await?;
        tx.commit().await?;
        Ok(())
    }
}

#[async_trait]
impl EventSink for Vies {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic != TOPIC {
            return Ok(());
        }
        let Some(customer) = ev.record_id else { return Ok(()) };
        let ctx = Ctx::system(ev.tenant_id, k.registry.schema(ev.tenant_id).await?);
        if !ctx.schema.packages.iter().any(|p| p == crate::PACKAGE) {
            return Ok(());
        }
        let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
        let found = find(k, &ctx, &mut tx, "Customer", vec![Filter::eq("id", customer.to_string())], None, 1).await?;
        let Some(cust) = found.into_iter().next() else { return Ok(()) };
        let Some(vat) = s(&cust, "eu_vat_number").map(str::to_string) else { return Ok(()) };
        let requester: Option<Doc> = match ev.payload["company"].as_str().and_then(|c| c.parse::<Uuid>().ok()) {
            Some(c) => find(k, &ctx, &mut tx, "Company", vec![Filter::eq("id", c.to_string())], None, 1).await?.into_iter().next(),
            None => None,
        };
        let requester = match requester.filter(|c| s(c, "eu_vat_number").is_some()) {
            Some(c) => Some(c),
            None => find(k, &ctx, &mut tx, "Company", vec![], Some("created_at asc"), 100).await?.into_iter().find(|c| s(c, "eu_vat_number").is_some()),
        };
        tx.commit().await?;
        let mut body = json!({"countryCode": vies_code(iso_code(&vat[..2])), "vatNumber": &vat[2..]});
        if let Some(own) = requester.as_ref().and_then(|c| s(c, "eu_vat_number")) {
            body["requesterMemberStateCode"] = json!(&own[..2]);
            body["requesterNumber"] = json!(&own[2..]);
        }
        let sent = self.client.post(format!("{}/check-vat-number", base_url())).json(&body).send().await;
        let reply = match sent {
            Ok(r) => r,
            Err(e) => {
                Self::record(k, &ctx, customer, json!({"eu_vies_error": format!("VIES could not be reached ({e}); trying again")})).await?;
                return Err(Error::internal(format!("VIES: {e}")));
            }
        };
        let status = reply.status();
        let v: Value = reply.json().await.unwrap_or(Value::Null);
        if status.is_success() && v.get("valid").is_some() {
            let valid = v["valid"].as_bool().unwrap_or(false);
            return Self::record(
                k,
                &ctx,
                customer,
                json!({
                    "eu_vies_valid": valid, "eu_vies_name": text(&v["name"]), "eu_vies_address": text(&v["address"]),
                    "eu_vies_checked_at": Utc::now().to_rfc3339(), "eu_vies_consultation": text(&v["requestIdentifier"]),
                    "eu_vies_error": if valid { Value::Null } else { json!(format!("VIES: {vat} is not a valid VAT number")) },
                }),
            )
            .await;
        }
        let first = &v["errorWrappers"][0];
        let code = first["error"].as_str().unwrap_or_default().to_string();
        let message = first["message"].as_str().unwrap_or_default().to_string();
        let said = format!("VIES ({status}): {} {}", code, message).trim().to_string();
        if status.is_server_error() || PASSING.contains(&code.as_str()) || code.is_empty() {
            // Busy or down: noted on the customer and asked again later (the outbox's back-off).
            Self::record(k, &ctx, customer, json!({"eu_vies_error": format!("{said}; trying again")})).await?;
            return Err(Error::internal(said));
        }
        Self::record(k, &ctx, customer, json!({"eu_vies_valid": false, "eu_vies_checked_at": Utc::now().to_rfc3339(), "eu_vies_error": said})).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vies_texts_drop_their_placeholders() {
        assert_eq!(text(&json!("---")), Value::Null);
        assert_eq!(text(&json!("  ACME GMBH ")), json!("ACME GMBH"));
        assert_eq!(text(&Value::Null), Value::Null);
    }
}
