//! Peppol PINT e-invoices for the countries with a PINT specialization (docs.peppol.eu, 2026):
//! Singapore (PINT SG, InvoiceNow, `urn:peppol:pint:billing-1@sg-1`), Australia and New Zealand
//! (PINT A-NZ, `@aunz-1`), Japan (JP PINT, `@jp-1`) and the United Arab Emirates (PINT AE, `@ae-1`),
//! all on the business process `urn:peppol:bis:billing`. A submitted invoice is written as UBL 2.1
//! with the EU add-on's writer (EN 16931 and PINT share the syntax), with the country's tax scheme
//! (GST in Singapore, Australia and New Zealand; VAT in Japan and the UAE), its tax category codes
//! (Singapore's SR, ZR, ESN33, OS; Japan's S and AA for the reduced rate; the UAE's Z for exports)
//! and its participants' electronic addresses (UEN 0195, ABN 0151, NZBN 0088, the qualified invoice
//! issuer's number 0221, the UAE's TIN 0235: the first ten digits of the TRN), checked against the
//! PINT rules a file written here could break, and queued for the company's Peppol access point
//! (the same hand-over as the EU's), whose answer is recorded.

use crate::engine::{Country, NOT_SUBJECT, account};
use crate::util::{d, get, id, rows, s, sys};
use async_trait::async_trait;
use ferrum_eu::ubl::{Invoice, Line, Party, Subtotal, endpoint, to_xml, unit_code};
use ferrum_kernel::{Doc, DocHook, Error, Filter, HookCtx, Result, outbox};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::{Value, json};

pub const PROFILE: &str = "urn:peppol:bis:billing";

/// A country's PINT specialization.
pub struct Pint {
    pub customization: &'static str,
    /// The tax scheme of the parties' numbers and the categories.
    pub scheme: &'static str,
    /// The electronic address scheme of the country's participants, and the identifier a
    /// registration number gives (None: none).
    pub endpoint_scheme: &'static str,
    pub endpoint_of: fn(&str) -> Option<String>,
    /// A line's category code from the engine's category.
    pub code: fn(&str) -> &'static str,
    /// The UAE's transaction type code (ProfileExecutionID).
    pub execution_id: Option<&'static str>,
    /// Tax amounts in whole units (Japan's).
    pub whole_tax: bool,
}

fn digits_only(v: &str) -> String {
    v.chars().filter(char::is_ascii_digit).collect()
}

pub static SG: Pint = Pint {
    customization: "urn:peppol:pint:billing-1@sg-1",
    scheme: "GST",
    endpoint_scheme: "0195",
    endpoint_of: |n| Some(n.replace(['-', ' '], "")),
    code: |c| match c {
        "Standard" => "SR",
        "Zero Rated" | "Export" => "ZR",
        "Exempt" => "ESN33",
        _ => "OS",
    },
    execution_id: None,
    whole_tax: false,
};

pub static AU: Pint = Pint {
    customization: "urn:peppol:pint:billing-1@aunz-1",
    scheme: "GST",
    endpoint_scheme: "0151",
    endpoint_of: |n| Some(digits_only(n)).filter(|d| d.len() == 11),
    code: anz_code,
    execution_id: None,
    whole_tax: false,
};

pub static NZ: Pint = Pint {
    customization: "urn:peppol:pint:billing-1@aunz-1",
    scheme: "GST",
    // The NZBN is a GLN: the EAN location code scheme.
    endpoint_scheme: "0088",
    endpoint_of: |_| None,
    code: anz_code,
    execution_id: None,
    whole_tax: false,
};

fn anz_code(c: &str) -> &'static str {
    match c {
        "Standard" | "Taxable" => "S",
        "Zero Rated" => "Z",
        "Export" => "G",
        "Exempt" => "E",
        _ => "O",
    }
}

pub static JP: Pint = Pint {
    customization: "urn:peppol:pint:billing-1@jp-1",
    scheme: "VAT",
    endpoint_scheme: "0221",
    endpoint_of: |n| Some(n.to_string()),
    code: |c| match c {
        "Standard" => "S",
        "Reduced" => "AA",
        "Export" | "Zero Rated" => "G",
        "Exempt" => "E",
        _ => "O",
    },
    execution_id: None,
    whole_tax: true,
};

pub static AE: Pint = Pint {
    customization: "urn:peppol:pint:billing-1@ae-1",
    scheme: "VAT",
    endpoint_scheme: "0235",
    endpoint_of: |n| Some(digits_only(n)).filter(|d| d.len() == 15).map(|d| d[..10].to_string()),
    code: |c| match c {
        "Standard" => "S",
        "Zero Rated" | "Export" => "Z",
        "Exempt" => "E",
        _ => "O",
    },
    execution_id: Some("00000000"),
    whole_tax: false,
};

fn r2(v: Decimal) -> Decimal {
    let mut x = v.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    x.rescale(2);
    x
}

/// The PINT rules a file written here could break: the problems, empty when it complies.
pub fn validate(inv: &Invoice, p: &Pint) -> Vec<String> {
    let mut out = Vec::new();
    let mut rule = |ok: bool, id: &str, what: &str| {
        if !ok {
            out.push(format!("{id}: {what}"));
        }
    };
    rule(inv.seller.endpoint.is_some(), "IBR-062", "the seller's electronic address (its Peppol ID)");
    rule(inv.buyer.endpoint.is_some(), "IBR-063", "the buyer's electronic address (its Peppol ID)");
    rule(inv.seller.vat.is_some(), "IBR-CO-26", "the seller's tax registration number");
    rule(!inv.lines.is_empty(), "IBR-016", "the invoice has lines");
    rule(inv.lines.iter().all(|l| !l.category.is_empty()), "IBR-SR-58", "every line has its tax category");
    rule(inv.subtotals.iter().all(|t| t.category != "S" || t.percent > Decimal::ZERO), "IBR-S-05", "a standard-rated category's rate is above zero");
    if p.whole_tax {
        rule(inv.subtotals.iter().all(|t| t.tax.fract().is_zero()), "JP-BR", "each category's tax is in whole yen");
    }
    out
}

/// A submitted sales invoice as the country's PINT invoice.
pub async fn invoice(hc: &mut HookCtx<'_>, doc: &Doc, c: &Country, p: &Pint) -> Result<Invoice> {
    let ctx = sys(hc.ctx);
    let code = c.package;
    let field = |f: &str| format!("{code}_{f}");
    let company = get(hc.kernel, &ctx, hc.tx, "Company", id(doc, "company").ok_or_else(|| Error::internal("invoice without company"))?).await?;
    let customer = get(hc.kernel, &ctx, hc.tx, "Customer", id(doc, "customer").ok_or_else(|| Error::internal("invoice without customer"))?).await?;
    let cid = id(&company, "id").unwrap_or_default();
    let mut accounts = Vec::new();
    for r in c.rows {
        if let Ok(a) = account(hc.kernel, &ctx, hc.tx, cid, r.sales).await {
            accounts.push(a);
        }
    }
    let credit_note = crate::util::b(doc, "is_return");
    let sign = if credit_note { -Decimal::ONE } else { Decimal::ONE };
    let address_of = |party: &mut Party, a: &Doc| {
        party.street = Some([s(a, "address_line1"), s(a, "address_line2")].into_iter().flatten().collect::<Vec<_>>().join(", ")).filter(|x| !x.is_empty());
        party.city = s(a, "city").map(str::to_string);
        party.postal = s(a, "postal_code").map(str::to_string);
        party.country = s(a, "country_code").map(str::to_string);
    };
    let derived = |own: Option<&str>, number: Option<&str>| -> Option<(String, String)> {
        endpoint(own).or_else(|| number.and_then(p.endpoint_of).map(|e| (p.endpoint_scheme.to_string(), e)))
    };
    let seller_number = s(&company, c.number_field);
    let mut seller = Party {
        name: s(&company, "company_name").unwrap_or_default().to_string(),
        vat: seller_number.map(str::to_string),
        street: s(&company, &field("street")).map(str::to_string),
        city: s(&company, &field("city")).map(str::to_string),
        postal: s(&company, &field("postal_code")).map(str::to_string),
        country: Some(c.country.to_string()),
        ..Default::default()
    };
    seller.endpoint = derived(s(&company, &field("peppol_id")), seller_number);
    let buyer_number = s(&customer, c.number_field);
    let mut buyer = Party { name: s(&customer, "customer_name").unwrap_or_default().to_string(), vat: buyer_number.map(str::to_string), ..Default::default() };
    if let Some(a) = id(doc, "customer_address")
        && let Some(addr) = crate::util::find(hc.kernel, &ctx, hc.tx, "Address", vec![Filter::eq("id", a.to_string())], None, 1).await?.first()
    {
        address_of(&mut buyer, addr);
    }
    buyer.endpoint = derived(s(&customer, &field("peppol_id")), buyer_number);
    // Each line on the row its category is on.
    let mut lines = Vec::new();
    for (i, row) in rows(doc, "items").iter().enumerate() {
        let mut category = "Out of Scope".to_string();
        let mut rate = Decimal::ZERO;
        for a in &accounts {
            let cat = &row["tax_categories"][a.as_str()];
            if cat.is_null() || cat["exemption_reason"].as_str().is_some_and(|r| r.starts_with(NOT_SUBJECT)) {
                continue;
            }
            category = cat["category"].as_str().unwrap_or("Out of Scope").to_string();
            rate = row["item_tax_rates"][a.as_str()].as_str().and_then(|v| v.parse().ok()).unwrap_or_default();
        }
        let code = (p.code)(&category);
        let uom = match id(row, "uom") {
            Some(u) => s(&get(hc.kernel, &ctx, hc.tx, "Uom", u).await?, "uom_name").unwrap_or("").to_string(),
            None => String::new(),
        };
        let net = r2(sign * d(row, "net_amount"));
        let qty = sign * d(row, "qty");
        lines.push(Line {
            id: (i + 1).to_string(),
            name: s(row, "item_name").unwrap_or("-").to_string(),
            quantity: qty,
            unit: unit_code(&uom).to_string(),
            price: if qty.is_zero() { net.abs() } else { (net / qty).abs() },
            net,
            category: code,
            percent: if matches!(code, "S" | "SR" | "AA") { rate } else { Decimal::ZERO },
        });
    }
    let mut subtotals: Vec<Subtotal> = Vec::new();
    for g in rows(doc, "tax_breakdown").iter().filter(|g| s(g, "account").is_some_and(|a| accounts.iter().any(|x| x == a))) {
        if s(g, "exemption_reason").is_some_and(|r| r.starts_with(NOT_SUBJECT)) {
            continue;
        }
        let code = (p.code)(s(g, "category").unwrap_or("Out of Scope"));
        let percent = if matches!(code, "S" | "SR" | "AA") { d(g, "rate") } else { Decimal::ZERO };
        let (taxable, tax) = (r2(sign * d(g, "taxable_amount")), r2(sign * d(g, "tax_amount")));
        match subtotals.iter_mut().find(|t| t.category == code && t.percent == percent) {
            Some(t) => {
                t.taxable += taxable;
                t.tax += tax;
            }
            None => subtotals.push(Subtotal {
                category: code,
                percent,
                taxable,
                tax,
                reason_code: None,
                reason: (!matches!(code, "S" | "SR" | "AA")).then(|| s(g, "exemption_reason").unwrap_or(category_text(code)).to_string()),
            }),
        }
    }
    let currency = match id(doc, "currency") {
        Some(cur) => s(&get(hc.kernel, &ctx, hc.tx, "Currency", cur).await?, "code").unwrap_or("").to_string(),
        None => String::new(),
    };
    let original = match id(doc, "return_against") {
        Some(o) => {
            let orig = get(hc.kernel, &ctx, hc.tx, "SalesInvoice", o).await?;
            Some((s(&orig, "number").unwrap_or("").to_string(), s(&orig, "posting_date").unwrap_or("").to_string()))
        }
        None => None,
    };
    let line_total: Decimal = lines.iter().map(|l| l.net).sum();
    let tax_total: Decimal = subtotals.iter().map(|t| t.tax).sum();
    let inv = Invoice {
        number: s(doc, "number").unwrap_or("").to_string(),
        issue_date: s(doc, "posting_date").unwrap_or("").to_string(),
        due_date: s(doc, "due_date").map(str::to_string),
        credit_note,
        original,
        currency,
        buyer_reference: s(doc, "po_no").unwrap_or_else(|| s(&customer, "customer_name").unwrap_or("-")).to_string(),
        order_reference: s(doc, "po_no").map(str::to_string),
        customization: Some(p.customization.into()),
        profile: Some(PROFILE.into()),
        execution_id: p.execution_id.map(str::to_string),
        tax_scheme: Some(p.scheme.into()),
        payment: None,
        notes: Vec::new(),
        seller,
        buyer,
        delivery_country: None,
        delivery_date: s(doc, "posting_date").map(str::to_string),
        lines,
        allowances: Vec::new(),
        subtotals,
        line_total,
        allowance_total: Decimal::ZERO,
        tax_exclusive: line_total,
        tax_total,
        tax_inclusive: line_total + tax_total,
        rounding: r2(sign * d(doc, "rounding_adjustment")),
        payable: line_total + tax_total + r2(sign * d(doc, "rounding_adjustment")),
    };
    let problems = validate(&inv, p);
    if !problems.is_empty() {
        return Err(Error::invalid(format!("the PINT e-invoice would not comply: {}", problems.join("; "))));
    }
    Ok(inv)
}

fn category_text(code: &str) -> &'static str {
    match code {
        "ZR" | "Z" | "G" => "Zero-rated or exported",
        "ESN33" | "ES33" | "E" => "Exempt",
        _ => "Not subject to the tax",
    }
}

/// PINT e-invoices on a country's submitted invoices: download, send through Peppol (on submit
/// when the company says so), record the access point's answer.
pub struct PintHook(pub &'static Country, pub &'static Pint);

impl PintHook {
    fn field(&self, f: &str) -> String {
        format!("{}_{f}", self.0.package)
    }

    async fn save(&self, hc: &mut HookCtx<'_>, doc: &Doc, values: Value) -> Result<()> {
        let inv = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let Value::Object(v) = values else { return Ok(()) };
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", inv, v).await
    }

    async fn send(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Value> {
        let status = self.field("peppol_status");
        if matches!(s(doc, &status), Some("Sent" | "Pending")) {
            return Err(Error::invalid(format!("the invoice is already {}", s(doc, &status).unwrap_or_default().to_lowercase())));
        }
        let inv = invoice(hc, doc, self.0, self.1).await?;
        let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let (scheme, ident) = inv.buyer.endpoint.clone().unwrap_or_default();
        let (s_scheme, s_ident) = inv.seller.endpoint.clone().unwrap_or_default();
        let event = json!({
            "invoice": iid, "number": s(doc, "number"), "company": s(doc, "company"), "sender": format!("{s_scheme}:{s_ident}"), "receiver": format!("{scheme}:{ident}"),
            "document_type": if inv.credit_note { "credit_note" } else { "invoice" }, "profile": inv.customization, "ubl": to_xml(&inv),
        });
        let mut values = serde_json::Map::new();
        values.insert(status, json!("Pending"));
        values.insert(self.field("peppol_error"), Value::Null);
        self.save(hc, doc, Value::Object(values)).await?;
        outbox::enqueue(hc.tx, hc.ctx.tenant, ferrum_eu::ubl::SEND_TOPIC, Some("SalesInvoice"), Some(iid), event).await?;
        Ok(json!({"status": "Pending"}))
    }
}

#[async_trait]
impl DocHook for PintHook {
    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if hc.entity.def.name != "SalesInvoice" || hc.entity.field(&self.field("peppol_status")).is_none() {
            return Ok(());
        }
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let company = get(hc.kernel, &sys(hc.ctx), hc.tx, "Company", cid).await?;
        if s(&company, "country_code") != Some(self.0.country) || !crate::util::b(&company, &self.field("peppol_on_submit")) {
            return Ok(());
        }
        let customer = match id(doc, "customer") {
            Some(c) => get(hc.kernel, &sys(hc.ctx), hc.tx, "Customer", c).await?,
            None => return Ok(()),
        };
        // To customers on Peppol only (a Peppol ID, or a registration number that gives one).
        let reachable = s(&customer, &self.field("peppol_id")).is_some() || s(&customer, self.0.number_field).and_then(self.1.endpoint_of).is_some();
        if reachable {
            self.send(hc, doc).await?;
        }
        Ok(())
    }

    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if matches!(s(doc, &self.field("peppol_status")), Some("Sent" | "Pending")) {
            return Err(Error::invalid("the invoice went to the customer through Peppol: issue a credit note instead of cancelling it"));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let code = self.0.package;
        if hc.entity.field(&self.field("peppol_status")).is_none() || !name.starts_with(&format!("{code}_")) {
            return Ok(None);
        }
        let action = &name[code.len() + 1..];
        match action {
            "pint" => {
                let inv = invoice(hc, doc, self.0, self.1).await?;
                let file = format!("{}.xml", inv.number.replace(['/', '\\'], "-"));
                Ok(Some(json!({"file": {"name": file, "type": "application/xml", "content": to_xml(&inv)}})))
            }
            "send_peppol" => self.send(hc, doc).await.map(Some),
            "record_peppol" => {
                if s(doc, &self.field("peppol_status")) != Some("Pending") {
                    return Err(Error::invalid("only an invoice waiting for its access point is recorded"));
                }
                let message = params.get("message_id").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty());
                let mut values = serde_json::Map::new();
                match message {
                    Some(m) => {
                        values.insert(self.field("peppol_status"), json!("Sent"));
                        values.insert(self.field("peppol_message_id"), json!(m));
                        values.insert(self.field("peppol_error"), Value::Null);
                    }
                    None => {
                        let why = params
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("the access point refused the invoice")
                            .chars()
                            .take(2000)
                            .collect::<String>();
                        values.insert(self.field("peppol_status"), json!("Failed"));
                        values.insert(self.field("peppol_error"), json!(why));
                    }
                }
                let status = values.get(&self.field("peppol_status")).cloned();
                self.save(hc, doc, Value::Object(values)).await?;
                Ok(Some(json!({"status": status})))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_addresses() {
        assert_eq!(((SG.code)("Standard"), (SG.code)("Export"), (SG.code)("Exempt"), (SG.code)("Out of Scope")), ("SR", "ZR", "ESN33", "OS"));
        assert_eq!(((JP.code)("Reduced"), (JP.code)("Export"), (AE.code)("Export"), (AU.code)("Export")), ("AA", "G", "Z", "G"));
        assert_eq!((AE.endpoint_of)("100123456789003").as_deref(), Some("1001234567"), "the TIN: the TRN's first ten digits");
        assert_eq!((AU.endpoint_of)("51 824 753 556").as_deref(), Some("51824753556"));
        assert_eq!((NZ.endpoint_of)("49-091-850"), None, "New Zealand's Peppol ID is the NZBN, set on its own");
    }
}
