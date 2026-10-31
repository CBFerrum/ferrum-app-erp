//! Sales reported to Avalara AvaTax (REST v2): a submitted invoice of a company that reports to
//! AvaTax is queued in the outbox, and the connector records it as a committed transaction
//! (`POST /api/v2/transactions/createoradjust`, so a retry adjusts rather than duplicates) with
//! the tax the invoice charged as a tax-amount override, for AvaTax's returns. AvaTax's own
//! figure comes back with it: an invoice whose tax differs is marked for review. Cancelling the
//! invoice voids the transaction (`.../transactions/{code}/void`, `DocVoided`).

use crate::tax::{SALES_TAX, account, tax_code, zip5};
use crate::util::{d, get, id, rows, s, set, sys};
use async_trait::async_trait;
use ferrum_kernel::outbox::Event;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, EventSink, HookCtx, Kernel, Result, outbox};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::time::Duration;
use uuid::Uuid;

pub const COMMIT_TOPIC: &str = "us.avatax.commit_requested";
pub const VOID_TOPIC: &str = "us.avatax.void_requested";
const SANDBOX: &str = "https://sandbox-rest.avatax.com";
const PRODUCTION: &str = "https://rest.avatax.com";

/// AvaTax's host for a company's mode (none when it is Off); `FERRUM_US_AVATAX_URL` stands in for both.
fn host(mode: &str) -> Option<String> {
    if mode == "Off" {
        return None;
    }
    if let Ok(u) = std::env::var("FERRUM_US_AVATAX_URL")
        && !u.trim().is_empty()
    {
        return Some(u.trim_end_matches('/').to_string());
    }
    match mode {
        "Sandbox" => Some(SANDBOX.into()),
        "Production" => Some(PRODUCTION.into()),
        _ => None,
    }
}

/// A path segment as AvaTax takes it: its own escapes for characters URLs reserve.
pub fn segment(v: &str) -> String {
    v.chars()
        .map(|c| match c {
            '/' => "_-ava2f-_".to_string(),
            '+' => "_-ava2b-_".to_string(),
            '?' => "_-ava3f-_".to_string(),
            '%' => "_-ava25-_".to_string(),
            '#' => "_-ava23-_".to_string(),
            ' ' => "%20".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// AvaTax's entity use code for an exemption reason.
pub fn entity_use_code(reason: &str) -> &'static str {
    match reason {
        "Federal Government" => "A",
        "State or Local Government" => "B",
        "Charitable" => "E",
        "Religious" => "F",
        "Resale" => "G",
        "Agricultural" => "H",
        "Industrial Production" => "I",
        "Direct Pay" => "J",
        "Educational" => "M",
        _ => "L",
    }
}

fn num(v: Decimal) -> Value {
    json!(v.normalize().to_string().parse::<f64>().unwrap_or_default())
}

/// The transaction AvaTax records for a submitted invoice.
async fn model(hc: &mut HookCtx<'_>, doc: &Doc, company: &Doc) -> Result<Value> {
    let ctx = sys(hc.ctx);
    let cid = id(company, "id").ok_or_else(|| Error::internal("company without id"))?;
    let sales_tax = account(hc.kernel, &ctx, hc.tx, cid, SALES_TAX).await?;
    let tax: Decimal = rows(doc, "taxes").iter().filter(|t| s(t, "account") == Some(sales_tax.as_str())).map(|t| d(t, "tax_amount")).sum();
    let currency = match id(doc, "currency") {
        Some(c) => s(&get(hc.kernel, &ctx, hc.tx, "Currency", c).await?, "code").map(str::to_string),
        None => None,
    };
    let to = match ferrum_app_erp::localization::ship_to(hc.kernel, &ctx, hc.tx, doc).await? {
        Some(a) => json!({"line1": s(&a, "address_line1"), "city": s(&a, "city"), "region": s(&a, "state"), "postalCode": s(&a, "postal_code"),
                          "country": s(&a, "country_code").unwrap_or("US")}),
        None => json!({"region": s(company, "us_state"), "postalCode": s(company, "us_zip").and_then(zip5), "country": "US"}),
    };
    let from = json!({"region": s(company, "us_state"), "postalCode": s(company, "us_zip").and_then(zip5), "country": "US"});
    let mut lines = Vec::new();
    for (i, line) in rows(doc, "items").iter().enumerate() {
        let item = match id(line, "item") {
            Some(it) => get(hc.kernel, &ctx, hc.tx, "Item", it).await?,
            None => Doc::new(),
        };
        lines.push(json!({
            "number": (i + 1).to_string(), "quantity": num(d(line, "qty").abs()), "amount": num(d(line, "net_amount")), "taxCode": tax_code(&item),
            "itemCode": s(&item, "item_code"), "description": s(line, "item_name").or(s(&item, "item_name")),
        }));
    }
    let certificate = match id(doc, "us_exemption_certificate") {
        Some(c) => Some(get(hc.kernel, &ctx, hc.tx, "UsExemptionCertificate", c).await?),
        None => None,
    };
    let mut create = json!({
        "type": if crate::util::b(doc, "is_return") { "ReturnInvoice" } else { "SalesInvoice" },
        "code": s(doc, "number"),
        "companyCode": s(company, "us_avatax_company_code"),
        "date": s(doc, "posting_date"),
        "customerCode": s(doc, "customer"),
        "commit": true,
        "currencyCode": currency,
        "purchaseOrderNo": s(doc, "po_no"),
        "addresses": {"shipFrom": from, "shipTo": to},
        "lines": lines,
        "taxOverride": {"type": "TaxAmount", "taxAmount": num(tax), "reason": "The tax charged on the invoice"},
        "description": s(doc, "customer_name"),
    });
    if let Some(c) = &certificate {
        create["exemptionNo"] = json!(s(c, "certificate_number"));
        create["entityUseCode"] = json!(entity_use_code(s(c, "reason").unwrap_or_default()));
    }
    Ok(json!({"createTransactionModel": create, "adjustmentReason": "Other", "adjustmentDescription": "Recorded again by Ferrum"}))
}

/// Queues submitted invoices for AvaTax and cancelled ones for voiding.
pub struct InvoiceHook;

async fn reporting_company(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Option<Doc>> {
    let Some(cid) = id(doc, "company") else { return Ok(None) };
    let company = get(hc.kernel, &sys(hc.ctx), hc.tx, "Company", cid).await?;
    Ok((s(&company, "country_code") == Some("US") && host(s(&company, "us_avatax_mode").unwrap_or("Off")).is_some()).then_some(company))
}

#[async_trait]
impl DocHook for InvoiceHook {
    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if hc.entity.field("us_avatax_status").is_none() {
            return Ok(());
        }
        if let Some(company) = reporting_company(hc, doc).await? {
            if s(&company, "us_avatax_company_code").is_none() || s(&company, "us_avatax_account_id").is_none() {
                return Err(Error::invalid("the company reports to AvaTax: set its AvaTax account ID and company code (or switch reporting off)"));
            }
            set(doc, "us_avatax_status", "Pending");
            set(doc, "us_avatax_error", Value::Null);
        }
        Ok(())
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if s(doc, "us_avatax_status") != Some("Pending") {
            return Ok(());
        }
        let Some(company) = reporting_company(hc, doc).await? else { return Ok(()) };
        let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let body = model(hc, doc, &company).await?;
        let tax = body["createTransactionModel"]["taxOverride"]["taxAmount"].clone();
        let event = json!({"invoice": iid, "company": s(doc, "company"), "body": body, "tax": tax});
        outbox::enqueue(hc.tx, hc.ctx.tenant, COMMIT_TOPIC, Some("SalesInvoice"), Some(iid), event).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !matches!(s(doc, "us_avatax_status"), Some("Pending" | "Committed" | "Differs")) {
            return Ok(());
        }
        let Some(company) = reporting_company(hc, doc).await? else { return Ok(()) };
        let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let event = json!({
            "invoice": iid, "company": s(doc, "company"), "company_code": s(&company, "us_avatax_company_code"), "code": s(doc, "number"),
            "document_type": if crate::util::b(doc, "is_return") { "ReturnInvoice" } else { "SalesInvoice" },
        });
        outbox::enqueue(hc.tx, hc.ctx.tenant, VOID_TOPIC, Some("SalesInvoice"), Some(iid), event).await
    }
}

/// The connector.
pub struct AvaTax {
    client: reqwest::Client,
}

impl Default for AvaTax {
    fn default() -> Self {
        AvaTax { client: reqwest::Client::builder().timeout(Duration::from_secs(60)).build().unwrap_or_default() }
    }
}

impl AvaTax {
    async fn record(k: &Kernel, ctx: &Ctx, invoice: Uuid, values: Value) -> Result<()> {
        let Value::Object(v) = values else { return Ok(()) };
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        k.set_values_tx(&sys(ctx), &mut tx, "SalesInvoice", invoice, v).await?;
        tx.commit().await?;
        Ok(())
    }

    fn why(v: &Value, status: u16) -> String {
        let e = &v["error"];
        let detail = e["details"].as_array().and_then(|d| d.first()).and_then(|d| d["description"].as_str().or(d["message"].as_str()));
        format!("{} {}", e["code"].as_str().unwrap_or(&status.to_string()), detail.or(e["message"].as_str()).unwrap_or("no reason given")).trim().to_string()
    }
}

#[async_trait]
impl EventSink for AvaTax {
    async fn on_event(&self, k: &Kernel, ev: &Event) -> Result<()> {
        if ev.topic != COMMIT_TOPIC && ev.topic != VOID_TOPIC {
            return Ok(());
        }
        let Some(invoice) = ev.record_id else { return Ok(()) };
        let ctx = Ctx::system(ev.tenant_id, k.registry.schema(ev.tenant_id).await?);
        let Some(cid) = ev.payload["company"].as_str().and_then(|c| c.parse::<Uuid>().ok()) else { return Ok(()) };
        let mut tx = k.db.tenant_tx(ev.tenant_id).await?;
        let company = get(k, &ctx, &mut tx, "Company", cid).await?;
        let key = k.secret(&mut tx, ev.tenant_id, "Company", cid, "us_avatax_license_key").await?;
        let current = get(k, &ctx, &mut tx, "SalesInvoice", invoice).await?;
        tx.commit().await?;
        let commit = ev.topic == COMMIT_TOPIC;
        let status = s(&current, "us_avatax_status");
        if status == Some("Voided") {
            return Ok(());
        }
        // Cancelled before it was recorded: never recorded (its void finds it voided).
        if commit && current.get("docstatus").and_then(Value::as_i64) == Some(2) {
            return Self::record(k, &ctx, invoice, json!({"us_avatax_status": "Voided", "us_avatax_error": null})).await;
        }
        let failed = |why: &str| {
            if commit {
                json!({"us_avatax_status": "Failed", "us_avatax_error": why})
            } else {
                json!({"us_avatax_error": format!("{why}: void the transaction in AvaTax")})
            }
        };
        let Some(base) = host(s(&company, "us_avatax_mode").unwrap_or("Off")) else {
            return Self::record(k, &ctx, invoice, failed("the company no longer reports to AvaTax")).await;
        };
        let (Some(account_id), Some(key)) = (s(&company, "us_avatax_account_id"), key) else {
            return Self::record(k, &ctx, invoice, failed("set the company's AvaTax account ID and license key")).await;
        };
        let client_id = format!("Ferrum; {}; Rust; 2; ferrum-us", env!("CARGO_PKG_VERSION"));
        let call = |url: String| {
            self.client.post(url).basic_auth(account_id, Some(&key)).header("X-Avalara-Client", client_id.as_str()).header("Accept", "application/json")
        };
        if commit {
            let reply = call(format!("{base}/api/v2/transactions/createoradjust"))
                .json(&ev.payload["body"])
                .send()
                .await
                .map_err(|e| Error::internal(format!("AvaTax could not be reached: {e}")))?;
            let code = reply.status().as_u16();
            let v: Value = reply.json().await.unwrap_or(Value::Null);
            return match code {
                200 | 201 => {
                    let ours = ev.payload["tax"].as_f64().unwrap_or_default();
                    let theirs = v["totalTaxCalculated"].as_f64().or(v["totalTax"].as_f64()).unwrap_or(ours);
                    let differs = (theirs - ours).abs() > 0.005;
                    let note = differs.then(|| format!("AvaTax works out {theirs:.2} of tax; the invoice charged {ours:.2}"));
                    Self::record(
                        k,
                        &ctx,
                        invoice,
                        json!({"us_avatax_status": if differs { "Differs" } else { "Committed" }, "us_avatax_tax": format!("{theirs:.2}"), "us_avatax_error": note}),
                    )
                    .await
                }
                401 | 403 => Self::record(k, &ctx, invoice, failed(&format!("AvaTax refused the credentials: {}", Self::why(&v, code)))).await,
                429 | 500.. => Err(Error::internal(format!("AvaTax is busy ({code}); trying again"))),
                _ => Self::record(k, &ctx, invoice, failed(&format!("AvaTax refused the invoice: {}", Self::why(&v, code)))).await,
            };
        }
        let number = ev.payload["code"].as_str().unwrap_or_default();
        let company_code = ev.payload["company_code"].as_str().unwrap_or_default();
        let kind = ev.payload["document_type"].as_str().unwrap_or("SalesInvoice");
        let reply = call(format!("{base}/api/v2/companies/{}/transactions/{}/void?documentType={kind}", segment(company_code), segment(number)))
            .json(&json!({"code": "DocVoided"}))
            .send()
            .await
            .map_err(|e| Error::internal(format!("AvaTax could not be reached: {e}")))?;
        let code = reply.status().as_u16();
        let v: Value = reply.json().await.unwrap_or(Value::Null);
        match code {
            200 | 201 => Self::record(k, &ctx, invoice, json!({"us_avatax_status": "Voided", "us_avatax_error": null})).await,
            // Never recorded there (it failed): nothing to void. Not yet recorded (its commit is
            // still being retried): later.
            404 if status == Some("Failed") => Self::record(k, &ctx, invoice, json!({"us_avatax_status": "Voided"})).await,
            404 | 429 | 500.. => Err(Error::internal(format!("AvaTax could not void yet ({code}); trying again"))),
            _ => Self::record(k, &ctx, invoice, failed(&format!("AvaTax did not void it: {}", Self::why(&v, code)))).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avatax_codes() {
        assert_eq!(segment("SINV/2026 01#a"), "SINV_-ava2f-_2026%2001_-ava23-_a");
        assert_eq!((entity_use_code("Resale"), entity_use_code("Charitable"), entity_use_code("Other")), ("G", "E", "L"));
        assert_eq!(num(Decimal::new(97500, 4)), json!(9.75));
    }
}
