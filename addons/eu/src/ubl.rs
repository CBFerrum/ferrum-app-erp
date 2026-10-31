//! EN 16931 e-invoices in UBL 2.1: a submitted sales invoice as Peppol BIS Billing 3.0 (when the
//! company and the customer have Peppol IDs) or as plain EN 16931, checked against the standard's
//! business rules before it is written, so a file that leaves Ferrum validates. Sending goes
//! through the outbox (`eu.peppol.send_requested`) to the company's access point connector, which
//! records the answer with the invoice's Peppol actions.

use crate::util::{d, get, id, rows, s, sys};
use crate::vat::{INPUT_TAX, OSS_VAT, OUTPUT_TAX, iso_code, vat_country};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, Error, Filter, HookCtx, Result, outbox};
use rust_decimal::{Decimal, RoundingStrategy};
use serde_json::{Value, json};
use std::fmt::Write;

pub const SEND_TOPIC: &str = "eu.peppol.send_requested";
const PEPPOL_CUSTOMIZATION: &str = "urn:cen.eu:en16931:2017#compliant#urn:fdc:peppol.eu:2017:poacc:billing:3.0";
const PEPPOL_PROFILE: &str = "urn:fdc:peppol.eu:2017:poacc:billing:01:1.0";
const EN16931: &str = "urn:cen.eu:en16931:2017";

/// EN 16931's VAT category code (UNCL5305) for Ferrum's category.
pub fn category_code(category: Option<&str>, rate: Decimal) -> &'static str {
    match category {
        Some("Zero Rated") => "Z",
        Some("Exempt") => "E",
        Some("Reverse Charge") => "AE",
        Some("Intra-Community") => "K",
        Some("Export") => "G",
        Some("Out of Scope") => "O",
        Some(_) => "S",
        None if rate > Decimal::ZERO => "S",
        None => "Z",
    }
}

#[derive(Debug, Clone, Default)]
pub struct Party {
    pub name: String,
    pub vat: Option<String>,
    pub street: Option<String>,
    pub city: Option<String>,
    pub postal: Option<String>,
    pub country: Option<String>,
    /// Peppol endpoint: (scheme, identifier).
    pub endpoint: Option<(String, String)>,
    /// Who to ask about the invoice: (name, telephone, email).
    pub contact: Option<(String, String, String)>,
    /// The national tax registration number (BT-32), where there is no VAT number.
    pub tax_number: Option<String>,
    /// The country subdivision (BT-39, BT-54): a county or region, as national profiles code it.
    pub subdivision: Option<String>,
    /// The legal registration identifier (BT-30, BT-47) and its scheme (France's SIREN: 0002).
    pub legal_id: Option<(String, String)>,
}

/// How to pay: a UNCL4461 code (58: SEPA credit transfer) to an account.
#[derive(Debug, Clone, Default)]
pub struct Payment {
    pub code: String,
    pub iban: Option<String>,
    pub bic: Option<String>,
    pub account_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub id: String,
    pub name: String,
    pub quantity: Decimal,
    pub unit: String,
    pub price: Decimal,
    pub net: Decimal,
    pub category: &'static str,
    pub percent: Decimal,
}

#[derive(Debug, Clone)]
pub struct Subtotal {
    pub category: &'static str,
    pub percent: Decimal,
    pub taxable: Decimal,
    pub tax: Decimal,
    pub reason_code: Option<String>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Allowance {
    pub category: &'static str,
    pub percent: Decimal,
    pub amount: Decimal,
}

#[derive(Debug, Clone)]
pub struct Invoice {
    pub number: String,
    pub issue_date: String,
    pub due_date: Option<String>,
    pub credit_note: bool,
    pub original: Option<(String, String)>,
    pub currency: String,
    pub buyer_reference: String,
    pub order_reference: Option<String>,
    /// A national profile's specification identifier instead of EN 16931's or Peppol's.
    pub customization: Option<String>,
    pub payment: Option<Payment>,
    pub notes: Vec<String>,
    pub seller: Party,
    pub buyer: Party,
    pub delivery_country: Option<String>,
    pub delivery_date: Option<String>,
    pub lines: Vec<Line>,
    pub allowances: Vec<Allowance>,
    pub subtotals: Vec<Subtotal>,
    pub line_total: Decimal,
    pub allowance_total: Decimal,
    pub tax_exclusive: Decimal,
    pub tax_total: Decimal,
    pub tax_inclusive: Decimal,
    pub rounding: Decimal,
    pub payable: Decimal,
}

impl Invoice {
    pub fn peppol(&self) -> bool {
        self.seller.endpoint.is_some() && self.buyer.endpoint.is_some()
    }
}

fn r2(v: Decimal) -> Decimal {
    let mut x = v.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero);
    x.rescale(2);
    x
}

/// A Peppol participant ID ("0088:4000001000005") as (scheme, identifier).
pub fn endpoint(text: Option<&str>) -> Option<(String, String)> {
    let (scheme, ident) = text?.trim().split_once(':')?;
    (scheme.len() == 4 && scheme.bytes().all(|c| c.is_ascii_digit()) && !ident.trim().is_empty()).then(|| (scheme.to_string(), ident.trim().to_string()))
}

/// UN/ECE Recommendation 20 for Ferrum's usual units; one ("C62") for the rest.
pub fn unit_code(uom: &str) -> &'static str {
    match uom.trim().to_ascii_lowercase().as_str() {
        "nos" | "unit" | "units" | "piece" | "pieces" | "pcs" => "H87",
        "kg" | "kilogram" => "KGM",
        "gram" | "g" => "GRM",
        "meter" | "metre" | "m" => "MTR",
        "litre" | "liter" | "l" => "LTR",
        "hour" | "hours" | "h" => "HUR",
        "day" | "days" => "DAY",
        "box" => "XBX",
        "set" => "SET",
        _ => "C62",
    }
}

/// EN 16931's business rules (CEN/TS 16931-3-2 numbering) that the files Ferrum writes can break:
/// the problems, empty when it complies.
pub fn validate(inv: &Invoice) -> Vec<String> {
    let mut p: Vec<String> = Vec::new();
    let mut rule = |ok: bool, id: &str, what: &str| {
        if !ok {
            p.push(format!("{id}: {what}"));
        }
    };
    rule(!inv.number.trim().is_empty(), "BR-02", "the invoice has a number");
    rule(inv.currency.len() == 3, "BR-05", "the invoice has a currency (ISO 4217)");
    rule(!inv.seller.name.trim().is_empty(), "BR-06", "the seller has a name");
    rule(!inv.buyer.name.trim().is_empty(), "BR-07", "the buyer has a name");
    rule(inv.seller.country.is_some(), "BR-09", "the seller's address has a country");
    rule(inv.buyer.country.is_some(), "BR-11", "the buyer's address has a country");
    rule(!inv.lines.is_empty(), "BR-16", "the invoice has a line");
    for l in &inv.lines {
        rule(!l.name.trim().is_empty(), "BR-25", &format!("line {} has an item name", l.id));
        rule(l.price >= Decimal::ZERO, "BR-27", &format!("line {}'s net price is not negative", l.id));
        rule(
            (l.net - l.quantity * l.price).abs() <= Decimal::new(2, 2),
            "PEPPOL-EN16931-R120",
            &format!("line {}'s net amount is its quantity times its price", l.id),
        );
    }
    let sum_lines: Decimal = inv.lines.iter().map(|l| l.net).sum();
    rule(sum_lines == inv.line_total, "BR-CO-10", "the sum of line net amounts is the lines' total");
    let sum_allow: Decimal = inv.allowances.iter().map(|a| a.amount).sum();
    rule(sum_allow == inv.allowance_total, "BR-CO-11", "the sum of allowances is the allowances' total");
    rule(inv.tax_exclusive == inv.line_total - inv.allowance_total, "BR-CO-13", "the total without VAT is the lines less the allowances");
    let sum_tax: Decimal = inv.subtotals.iter().map(|t| t.tax).sum();
    rule(inv.tax_total == sum_tax, "BR-CO-14", "the VAT total is the sum of the breakdown's VAT");
    rule(inv.tax_inclusive == inv.tax_exclusive + inv.tax_total, "BR-CO-15", "the total with VAT is the total without VAT plus the VAT");
    rule(inv.payable == inv.tax_inclusive + inv.rounding, "BR-CO-16", "the amount due is the total with VAT and its rounding");
    rule(!inv.subtotals.is_empty(), "BR-CO-18", "the invoice has a VAT breakdown");
    for t in &inv.subtotals {
        let lines: Decimal = inv.lines.iter().filter(|l| l.category == t.category && l.percent == t.percent).map(|l| l.net).sum();
        let allow: Decimal = inv.allowances.iter().filter(|a| a.category == t.category && a.percent == t.percent).map(|a| a.amount).sum();
        rule(
            t.taxable == lines - allow,
            &format!("BR-{}-08", t.category),
            &format!("the {} {}% breakdown's taxable amount is its lines' less their allowances", t.category, t.percent),
        );
        // Within one unit, as the standard's own check allows (tax rounded line by line).
        let expect = r2(t.taxable * t.percent / Decimal::ONE_HUNDRED);
        rule((t.tax - expect).abs() < Decimal::ONE, "BR-CO-17", &format!("the {} {}% VAT is its taxable amount times its rate", t.category, t.percent));
        let zero = matches!(t.category, "Z" | "E" | "AE" | "K" | "G" | "O");
        if zero {
            rule(t.percent.is_zero() && t.tax.is_zero(), &format!("BR-{}-09", t.category), &format!("a {} breakdown has no VAT", t.category));
        } else {
            rule(t.percent > Decimal::ZERO, "BR-S-05", "a standard-rated breakdown has a rate above zero");
        }
        let reason = t.reason_code.is_some() || t.reason.is_some();
        match t.category {
            "E" | "AE" | "K" | "G" | "O" => rule(reason, &format!("BR-{}-10", t.category), &format!("a {} breakdown says why there is no VAT", t.category)),
            "S" | "Z" => rule(!reason, &format!("BR-{}-10", t.category), &format!("a {} breakdown has no exemption reason", t.category)),
            _ => {}
        }
    }
    let has = |c: &str| inv.subtotals.iter().any(|t| t.category == c) || inv.lines.iter().any(|l| l.category == c);
    for c in ["S", "Z", "E", "AE", "K", "G"] {
        if has(c) {
            rule(
                inv.seller.vat.is_some() || inv.seller.tax_number.is_some(),
                &format!("BR-{c}-02"),
                "the seller's VAT number (or tax number) is on an invoice with VAT categories other than not subject to VAT",
            );
        }
    }
    if has("AE") || has("K") {
        rule(inv.buyer.vat.is_some(), "BR-AE-02/BR-K-02", "the buyer's VAT number is on a reverse-charge or intra-Community invoice");
    }
    if has("K") {
        rule(inv.delivery_date.is_some(), "BR-IC-11", "an intra-Community supply says when it was delivered");
        rule(inv.delivery_country.is_some(), "BR-IC-12", "an intra-Community supply says the country it was delivered to");
    }
    if has("O") {
        rule(inv.subtotals.iter().all(|t| t.category == "O"), "BR-O-11", "an invoice not subject to VAT has no other VAT breakdown");
        rule(inv.seller.vat.is_none() && inv.buyer.vat.is_none(), "BR-O-02/BR-O-03", "an invoice not subject to VAT carries no VAT numbers");
    }
    if inv.peppol() {
        rule(!inv.buyer_reference.is_empty() || inv.order_reference.is_some(), "PEPPOL-EN16931-R003", "a buyer reference or an order reference is given");
    }
    p
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn amt(v: Decimal) -> String {
    r2(v).to_string()
}

fn num(v: Decimal) -> String {
    v.normalize().to_string()
}

fn party(out: &mut String, tag: &str, p: &Party) {
    let _ = write!(out, "<cac:{tag}><cac:Party>");
    if let Some((scheme, ident)) = &p.endpoint {
        let _ = write!(out, "<cbc:EndpointID schemeID=\"{}\">{}</cbc:EndpointID>", esc(scheme), esc(ident));
    }
    let _ = write!(out, "<cac:PartyName><cbc:Name>{}</cbc:Name></cac:PartyName><cac:PostalAddress>", esc(&p.name));
    if let Some(v) = &p.street {
        let _ = write!(out, "<cbc:StreetName>{}</cbc:StreetName>", esc(v));
    }
    if let Some(v) = &p.city {
        let _ = write!(out, "<cbc:CityName>{}</cbc:CityName>", esc(v));
    }
    if let Some(v) = &p.postal {
        let _ = write!(out, "<cbc:PostalZone>{}</cbc:PostalZone>", esc(v));
    }
    if let Some(v) = &p.subdivision {
        let _ = write!(out, "<cbc:CountrySubentity>{}</cbc:CountrySubentity>", esc(v));
    }
    let _ = write!(
        out,
        "<cac:Country><cbc:IdentificationCode>{}</cbc:IdentificationCode></cac:Country></cac:PostalAddress>",
        esc(p.country.as_deref().unwrap_or(""))
    );
    if let Some(vat) = &p.vat {
        let _ = write!(
            out,
            "<cac:PartyTaxScheme><cbc:CompanyID>{}</cbc:CompanyID><cac:TaxScheme><cbc:ID>VAT</cbc:ID></cac:TaxScheme></cac:PartyTaxScheme>",
            esc(vat)
        );
    }
    if let Some(n) = &p.tax_number {
        let _ =
            write!(out, "<cac:PartyTaxScheme><cbc:CompanyID>{}</cbc:CompanyID><cac:TaxScheme><cbc:ID>FC</cbc:ID></cac:TaxScheme></cac:PartyTaxScheme>", esc(n));
    }
    let _ = write!(out, "<cac:PartyLegalEntity><cbc:RegistrationName>{}</cbc:RegistrationName>", esc(&p.name));
    if let Some((scheme, ident)) = &p.legal_id {
        let _ = write!(out, "<cbc:CompanyID schemeID=\"{}\">{}</cbc:CompanyID>", esc(scheme), esc(ident));
    }
    out.push_str("</cac:PartyLegalEntity>");
    if let Some((name, phone, email)) = &p.contact {
        let _ = write!(
            out,
            "<cac:Contact><cbc:Name>{}</cbc:Name><cbc:Telephone>{}</cbc:Telephone><cbc:ElectronicMail>{}</cbc:ElectronicMail></cac:Contact>",
            esc(name),
            esc(phone),
            esc(email)
        );
    }
    let _ = write!(out, "</cac:Party></cac:{tag}>");
}

fn tax_category(out: &mut String, tag: &str, category: &str, percent: Decimal, reason: Option<(&Option<String>, &Option<String>)>) {
    let _ = write!(out, "<cac:{tag}><cbc:ID>{category}</cbc:ID>");
    if category != "O" {
        let _ = write!(out, "<cbc:Percent>{}</cbc:Percent>", num(percent));
    }
    if let Some((code, text)) = reason {
        if let Some(c) = code {
            let _ = write!(out, "<cbc:TaxExemptionReasonCode>{}</cbc:TaxExemptionReasonCode>", esc(c));
        }
        if let Some(t) = text {
            let _ = write!(out, "<cbc:TaxExemptionReason>{}</cbc:TaxExemptionReason>", esc(t));
        }
    }
    let _ = write!(out, "<cac:TaxScheme><cbc:ID>VAT</cbc:ID></cac:TaxScheme></cac:{tag}>");
}

/// The invoice as UBL 2.1 (an Invoice, or a CreditNote for a credit note).
pub fn to_xml(inv: &Invoice) -> String {
    let (root, ns, line_tag, qty_tag, type_tag, type_code) = if inv.credit_note {
        ("CreditNote", "urn:oasis:names:specification:ubl:schema:xsd:CreditNote-2", "CreditNoteLine", "CreditedQuantity", "CreditNoteTypeCode", "381")
    } else {
        ("Invoice", "urn:oasis:names:specification:ubl:schema:xsd:Invoice-2", "InvoiceLine", "InvoicedQuantity", "InvoiceTypeCode", "380")
    };
    let cur = esc(&inv.currency);
    let mut o = String::with_capacity(8192);
    let _ = write!(
        o,
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><{root} xmlns=\"{ns}\" \
         xmlns:cac=\"urn:oasis:names:specification:ubl:schema:xsd:CommonAggregateComponents-2\" \
         xmlns:cbc=\"urn:oasis:names:specification:ubl:schema:xsd:CommonBasicComponents-2\">"
    );
    let customization = inv.customization.as_deref().unwrap_or(if inv.peppol() { PEPPOL_CUSTOMIZATION } else { EN16931 });
    let _ = write!(o, "<cbc:CustomizationID>{}</cbc:CustomizationID>", esc(customization));
    if inv.peppol() {
        let _ = write!(o, "<cbc:ProfileID>{PEPPOL_PROFILE}</cbc:ProfileID>");
    }
    let _ = write!(o, "<cbc:ID>{}</cbc:ID><cbc:IssueDate>{}</cbc:IssueDate>", esc(&inv.number), esc(&inv.issue_date));
    if let (Some(due), false) = (&inv.due_date, inv.credit_note) {
        let _ = write!(o, "<cbc:DueDate>{}</cbc:DueDate>", esc(due));
    }
    let _ = write!(o, "<cbc:{type_tag}>{type_code}</cbc:{type_tag}>");
    for n in &inv.notes {
        let _ = write!(o, "<cbc:Note>{}</cbc:Note>", esc(n));
    }
    let _ = write!(o, "<cbc:DocumentCurrencyCode>{cur}</cbc:DocumentCurrencyCode>");
    if !inv.buyer_reference.is_empty() {
        let _ = write!(o, "<cbc:BuyerReference>{}</cbc:BuyerReference>", esc(&inv.buyer_reference));
    }
    if let Some(r) = &inv.order_reference {
        let _ = write!(o, "<cac:OrderReference><cbc:ID>{}</cbc:ID></cac:OrderReference>", esc(r));
    }
    if let Some((number, day)) = &inv.original {
        let _ = write!(
            o,
            "<cac:BillingReference><cac:InvoiceDocumentReference><cbc:ID>{}</cbc:ID><cbc:IssueDate>{}</cbc:IssueDate></cac:InvoiceDocumentReference></cac:BillingReference>",
            esc(number),
            esc(day)
        );
    }
    party(&mut o, "AccountingSupplierParty", &inv.seller);
    party(&mut o, "AccountingCustomerParty", &inv.buyer);
    if inv.delivery_country.is_some() || inv.delivery_date.is_some() {
        o.push_str("<cac:Delivery>");
        if let Some(day) = &inv.delivery_date {
            let _ = write!(o, "<cbc:ActualDeliveryDate>{}</cbc:ActualDeliveryDate>", esc(day));
        }
        if let Some(c) = &inv.delivery_country {
            let _ = write!(
                o,
                "<cac:DeliveryLocation><cac:Address><cac:Country><cbc:IdentificationCode>{}</cbc:IdentificationCode></cac:Country></cac:Address></cac:DeliveryLocation>",
                esc(c)
            );
        }
        o.push_str("</cac:Delivery>");
    }
    if let Some(p) = &inv.payment {
        let _ = write!(o, "<cac:PaymentMeans><cbc:PaymentMeansCode>{}</cbc:PaymentMeansCode>", esc(&p.code));
        if let Some(iban) = &p.iban {
            let _ = write!(o, "<cac:PayeeFinancialAccount><cbc:ID>{}</cbc:ID>", esc(iban));
            if let Some(n) = &p.account_name {
                let _ = write!(o, "<cbc:Name>{}</cbc:Name>", esc(n));
            }
            if let Some(bic) = &p.bic {
                let _ = write!(o, "<cac:FinancialInstitutionBranch><cbc:ID>{}</cbc:ID></cac:FinancialInstitutionBranch>", esc(bic));
            }
            o.push_str("</cac:PayeeFinancialAccount>");
        }
        o.push_str("</cac:PaymentMeans>");
    }
    for a in &inv.allowances {
        let _ = write!(
            o,
            "<cac:AllowanceCharge><cbc:ChargeIndicator>false</cbc:ChargeIndicator><cbc:AllowanceChargeReason>Discount</cbc:AllowanceChargeReason>\
             <cbc:Amount currencyID=\"{cur}\">{}</cbc:Amount>",
            amt(a.amount)
        );
        tax_category(&mut o, "TaxCategory", a.category, a.percent, None);
        o.push_str("</cac:AllowanceCharge>");
    }
    let _ = write!(o, "<cac:TaxTotal><cbc:TaxAmount currencyID=\"{cur}\">{}</cbc:TaxAmount>", amt(inv.tax_total));
    for t in &inv.subtotals {
        let _ = write!(
            o,
            "<cac:TaxSubtotal><cbc:TaxableAmount currencyID=\"{cur}\">{}</cbc:TaxableAmount><cbc:TaxAmount currencyID=\"{cur}\">{}</cbc:TaxAmount>",
            amt(t.taxable),
            amt(t.tax)
        );
        tax_category(&mut o, "TaxCategory", t.category, t.percent, Some((&t.reason_code, &t.reason)));
        o.push_str("</cac:TaxSubtotal>");
    }
    o.push_str("</cac:TaxTotal>");
    let _ = write!(
        o,
        "<cac:LegalMonetaryTotal><cbc:LineExtensionAmount currencyID=\"{cur}\">{}</cbc:LineExtensionAmount>\
         <cbc:TaxExclusiveAmount currencyID=\"{cur}\">{}</cbc:TaxExclusiveAmount><cbc:TaxInclusiveAmount currencyID=\"{cur}\">{}</cbc:TaxInclusiveAmount>",
        amt(inv.line_total),
        amt(inv.tax_exclusive),
        amt(inv.tax_inclusive)
    );
    if !inv.allowance_total.is_zero() {
        let _ = write!(o, "<cbc:AllowanceTotalAmount currencyID=\"{cur}\">{}</cbc:AllowanceTotalAmount>", amt(inv.allowance_total));
    }
    if !inv.rounding.is_zero() {
        let _ = write!(o, "<cbc:PayableRoundingAmount currencyID=\"{cur}\">{}</cbc:PayableRoundingAmount>", amt(inv.rounding));
    }
    let _ = write!(o, "<cbc:PayableAmount currencyID=\"{cur}\">{}</cbc:PayableAmount></cac:LegalMonetaryTotal>", amt(inv.payable));
    for l in &inv.lines {
        let _ = write!(
            o,
            "<cac:{line_tag}><cbc:ID>{}</cbc:ID><cbc:{qty_tag} unitCode=\"{}\">{}</cbc:{qty_tag}><cbc:LineExtensionAmount currencyID=\"{cur}\">{}</cbc:LineExtensionAmount>\
             <cac:Item><cbc:Name>{}</cbc:Name>",
            esc(&l.id),
            esc(&l.unit),
            num(l.quantity),
            amt(l.net),
            esc(&l.name)
        );
        tax_category(&mut o, "ClassifiedTaxCategory", l.category, l.percent, None);
        let _ = write!(o, "</cac:Item><cac:Price><cbc:PriceAmount currencyID=\"{cur}\">{}</cbc:PriceAmount></cac:Price></cac:{line_tag}>", num(l.price));
    }
    let _ = write!(o, "</{root}>");
    o
}

// ───────────────────────────── from a sales invoice ─────────────────────────────

async fn address(hc: &mut HookCtx<'_>, address: Option<uuid::Uuid>) -> Result<Option<Doc>> {
    let Some(a) = address else { return Ok(None) };
    Ok(crate::util::find(hc.kernel, hc.ctx, hc.tx, "Address", vec![Filter::eq("id", a.to_string())], None, 1).await?.into_iter().next())
}

fn from_address(p: &mut Party, a: &Doc) {
    p.street = Some([s(a, "address_line1"), s(a, "address_line2")].into_iter().flatten().collect::<Vec<_>>().join(", ")).filter(|x| !x.is_empty());
    p.city = s(a, "city").map(str::to_string);
    p.postal = s(a, "postal_code").map(str::to_string);
    p.country = s(a, "country_code").map(str::to_string);
    p.subdivision = s(a, "state").map(str::to_string);
}

/// The EN 16931 invoice of a submitted sales invoice, from what was stored when it was submitted.
pub async fn invoice(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Invoice> {
    let ctx = sys(hc.ctx);
    let company = get(hc.kernel, &ctx, hc.tx, "Company", id(doc, "company").ok_or_else(|| Error::internal("invoice without company"))?).await?;
    let customer = get(hc.kernel, &ctx, hc.tx, "Customer", id(doc, "customer").ok_or_else(|| Error::internal("invoice without customer"))?).await?;
    let mut problems = Vec::new();
    // The VAT rows are the EU's (the company's VAT accounts); other taxes have no place in EN 16931.
    let cid = id(&company, "id").unwrap_or_default();
    let mut vat_accounts = Vec::new();
    for n in [OUTPUT_TAX, INPUT_TAX, OSS_VAT] {
        let a = crate::util::find(hc.kernel, &ctx, hc.tx, "Account", vec![Filter::eq("company", cid.to_string()), Filter::eq("account_number", n)], None, 1)
            .await?;
        vat_accounts.extend(a.first().and_then(|a| s(a, "id")).map(str::to_string));
    }
    let taxes = rows(doc, "taxes");
    let vat_rows: Vec<&Doc> = taxes.iter().filter(|t| s(t, "account").is_some_and(|a| vat_accounts.iter().any(|v| v == a))).collect();
    if vat_rows.len() != taxes.len() {
        problems.push("only VAT can be in an EN 16931 invoice: its other tax rows (charges, levies) belong on lines".to_string());
    }
    if vat_rows.len() > 1 {
        problems.push("an EN 16931 invoice has one VAT: the invoice has several VAT rows".to_string());
    }
    if taxes.iter().any(|t| t.get("included_in_price").and_then(Value::as_bool).unwrap_or(false)) {
        problems.push("prices including VAT: an EN 16931 invoice states net prices (use prices without VAT)".to_string());
    }
    let vat_row = vat_rows.first().copied();
    let vat_account = vat_row.and_then(|r| s(r, "account")).map(str::to_string);
    let row_rate = vat_row.map(|r| d(r, "rate")).unwrap_or_default();
    let credit_note = doc.get("is_return").and_then(Value::as_bool).unwrap_or(false);
    let sign = if credit_note { -Decimal::ONE } else { Decimal::ONE };
    let mut lines = Vec::new();
    let mut allowances: Vec<Allowance> = Vec::new();
    for (i, r) in rows(doc, "items").iter().enumerate() {
        let (rate, category) = match &vat_account {
            Some(acc) => {
                let rate = r.get("item_tax_rates").and_then(|m| m.get(acc)).and_then(Value::as_str).and_then(|x| x.parse().ok()).unwrap_or(row_rate);
                let cat = r.get("tax_categories").and_then(|m| m.get(acc)).and_then(|c| c["category"].as_str()).map(str::to_string);
                (rate, category_code(cat.as_deref(), rate))
            }
            None => (Decimal::ZERO, "O"),
        };
        let percent = if category == "S" { rate } else { Decimal::ZERO };
        let amount = sign * d(r, "amount");
        let net = sign * d(r, "net_amount");
        let qty = sign * d(r, "qty");
        let uom = match id(r, "uom") {
            Some(u) => s(&get(hc.kernel, &ctx, hc.tx, "Uom", u).await?, "uom_name").unwrap_or("").to_string(),
            None => String::new(),
        };
        lines.push(Line {
            id: (i + 1).to_string(),
            name: s(r, "item_name").unwrap_or("").to_string(),
            quantity: qty,
            unit: unit_code(&uom).to_string(),
            price: d(r, "rate").abs(),
            net: r2(amount),
            category,
            percent,
        });
        let allowance = r2(amount - net);
        if !allowance.is_zero() {
            match allowances.iter_mut().find(|a| a.category == category && a.percent == percent) {
                Some(a) => a.amount += allowance,
                None => allowances.push(Allowance { category, percent, amount: allowance }),
            }
        }
    }
    let subtotals: Vec<Subtotal> = rows(doc, "tax_breakdown")
        .iter()
        .filter(|g| vat_account.is_some() && s(g, "account") == vat_account.as_deref())
        .map(|g| {
            let rate = d(g, "rate");
            let category = category_code(s(g, "category"), rate);
            let reason = s(g, "exemption_reason").map(str::to_string);
            let (reason_code, reason) = match (category, reason) {
                ("S" | "Z", _) => (None, None),
                (_, Some(r)) if r.starts_with("VATEX-") => (Some(r), None),
                (_, Some(r)) => (None, Some(r)),
                (c, None) => (Some(format!("VATEX-EU-{}", if c == "K" { "IC" } else { c })), None),
            };
            Subtotal {
                category,
                percent: if category == "S" { rate } else { Decimal::ZERO },
                taxable: r2(sign * d(g, "taxable_amount")),
                tax: r2(sign * d(g, "tax_amount")),
                reason_code,
                reason,
            }
        })
        .collect();
    let mut seller =
        Party { name: s(&company, "company_name").unwrap_or("").to_string(), vat: s(&company, "eu_vat_number").map(str::to_string), ..Default::default() };
    seller.country = s(&company, "country_code").map(str::to_string);
    seller.endpoint = endpoint(s(&company, "eu_peppol_id"));
    seller.tax_number = s(&company, "eu_tax_number").filter(|_| seller.vat.is_none()).map(str::to_string);
    seller.street = s(&company, "eu_street").map(str::to_string);
    seller.city = s(&company, "eu_city").map(str::to_string);
    seller.postal = s(&company, "eu_postal_code").map(str::to_string);
    if let (Some(n), Some(p), Some(e)) = (s(&company, "eu_contact_name"), s(&company, "eu_contact_phone"), s(&company, "eu_contact_email")) {
        seller.contact = Some((n.to_string(), p.to_string(), e.to_string()));
    }
    // Paid by SEPA credit transfer to the company's IBAN, when it has one.
    let payment = s(&company, "eu_iban").map(|iban| Payment {
        code: "58".into(),
        iban: Some(iban.to_string()),
        bic: s(&company, "eu_bic").map(str::to_string),
        account_name: Some(seller.name.clone()),
    });
    // The customer's VAT number as it was when the invoice was submitted.
    let vat = s(doc, "eu_customer_vat").or_else(|| s(&customer, "eu_vat_number")).map(str::to_string);
    let mut buyer = Party { name: s(&customer, "customer_name").unwrap_or("").to_string(), vat, ..Default::default() };
    buyer.endpoint = endpoint(s(&customer, "eu_peppol_id"));
    if let Some(a) = address(hc, id(doc, "customer_address")).await? {
        from_address(&mut buyer, &a);
    }
    if buyer.country.is_none() {
        buyer.country = buyer.vat.as_deref().and_then(vat_country);
    }
    let ship = address(hc, id(doc, "ship_to")).await?;
    let delivery_country =
        ship.as_ref().and_then(|a| s(a, "country_code")).map(str::to_string).or_else(|| s(doc, "eu_destination").map(|c| iso_code(c).to_string()));
    let original = match id(doc, "return_against") {
        Some(o) => {
            let orig = get(hc.kernel, &ctx, hc.tx, "SalesInvoice", o).await?;
            Some((s(&orig, "number").unwrap_or("").to_string(), s(&orig, "posting_date").unwrap_or("").to_string()))
        }
        None => None,
    };
    let currency = match id(doc, "currency") {
        Some(c) => s(&get(hc.kernel, &ctx, hc.tx, "Currency", c).await?, "code").unwrap_or("").to_string(),
        None => String::new(),
    };
    let line_total: Decimal = lines.iter().map(|l| l.net).sum();
    let allowance_total = r2(sign * d(doc, "discount_amount"));
    let tax_total: Decimal = subtotals.iter().map(|t| t.tax).sum();
    let tax_exclusive = r2(sign * d(doc, "net_total"));
    let tax_inclusive = r2(sign * d(doc, "grand_total"));
    let rounding = r2(sign * d(doc, "rounding_adjustment"));
    let notes: Vec<String> = s(doc, "eu_legal_text").map(|t| t.lines().map(str::to_string).collect()).unwrap_or_default();
    let po = s(doc, "po_no").map(str::to_string);
    let inv = Invoice {
        number: s(doc, "number").unwrap_or("").to_string(),
        issue_date: s(doc, "posting_date").unwrap_or("").to_string(),
        due_date: s(doc, "due_date").map(str::to_string),
        credit_note,
        original,
        currency,
        buyer_reference: po.clone().unwrap_or_else(|| s(&customer, "customer_name").unwrap_or("").to_string()),
        order_reference: po,
        customization: None,
        payment,
        notes,
        seller,
        buyer,
        delivery_country,
        delivery_date: s(doc, "posting_date").map(str::to_string),
        lines,
        allowances,
        subtotals,
        line_total,
        allowance_total,
        tax_exclusive,
        tax_total,
        tax_inclusive,
        rounding,
        payable: r2(sign * d(doc, "rounded_total")),
    };
    problems.extend(validate(&inv));
    if !problems.is_empty() {
        return Err(Error::invalid(format!("the e-invoice would not comply with EN 16931: {}", problems.join("; "))));
    }
    Ok(inv)
}

/// Queues a submitted invoice for the company's Peppol access point (`inv`: the invoice as a
/// national profile writes it, else EN 16931's).
pub async fn send_peppol(hc: &mut HookCtx<'_>, doc: &Doc, inv: Option<Invoice>) -> Result<Value> {
    if matches!(s(doc, "eu_peppol_status"), Some("Sent" | "Pending")) {
        return Err(Error::invalid(format!("the invoice is already {}", s(doc, "eu_peppol_status").unwrap_or_default().to_lowercase())));
    }
    let inv = match inv {
        Some(i) => i,
        None => invoice(hc, doc).await?,
    };
    if !inv.peppol() {
        return Err(Error::invalid("the company and the customer both need a Peppol ID (scheme:identifier) to send through Peppol"));
    }
    let xml = to_xml(&inv);
    let iid = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
    let (scheme, ident) = inv.buyer.endpoint.clone().unwrap_or_default();
    let (s_scheme, s_ident) = inv.seller.endpoint.clone().unwrap_or_default();
    let event = json!({
        "invoice": iid, "number": s(doc, "number"), "company": s(doc, "company"),
        "sender": format!("{s_scheme}:{s_ident}"), "receiver": format!("{scheme}:{ident}"),
        "document_type": if inv.credit_note { "credit_note" } else { "invoice" }, "ubl": xml,
    });
    EInvoiceHook::save(hc, doc, json!({"eu_peppol_status": "Pending", "eu_peppol_error": null, "eu_peppol_message_id": null})).await?;
    outbox::enqueue(hc.tx, hc.ctx.tenant, SEND_TOPIC, Some("SalesInvoice"), Some(iid), event).await?;
    Ok(json!({"status": "Pending"}))
}

/// A sales invoice's e-invoice actions.
pub struct EInvoiceHook;

impl EInvoiceHook {
    async fn save(hc: &mut HookCtx<'_>, doc: &Doc, values: Value) -> Result<()> {
        let inv = id(doc, "id").ok_or_else(|| Error::internal("invoice without id"))?;
        let Value::Object(v) = values else { return Ok(()) };
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "SalesInvoice", inv, v).await
    }
}

fn param<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty() && v.chars().count() <= 2000)
        .ok_or_else(|| Error::field(key, "is required"))
}

#[async_trait]
impl DocHook for EInvoiceHook {
    async fn before_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if matches!(s(doc, "eu_peppol_status"), Some("Sent" | "Pending")) {
            return Err(Error::invalid("the invoice went to the customer through Peppol: issue a credit note instead of cancelling it"));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if hc.entity.field("eu_peppol_status").is_none() {
            return Ok(None);
        }
        let number = s(doc, "number").unwrap_or("invoice").replace(['/', '\\'], "-");
        match name {
            "eu_ubl" => {
                let inv = invoice(hc, doc).await?;
                let xml = to_xml(&inv);
                Ok(Some(json!({"peppol": inv.peppol(), "file": {"name": format!("{number}.xml"), "type": "application/xml", "content": xml}})))
            }
            "eu_send_peppol" => send_peppol(hc, doc, None).await.map(Some),
            "eu_record_peppol" => {
                if s(doc, "eu_peppol_status") != Some("Pending") {
                    return Err(Error::invalid("only an invoice waiting for its access point is recorded as sent"));
                }
                let message = param(params, "message_id")?.to_string();
                Self::save(hc, doc, json!({"eu_peppol_status": "Sent", "eu_peppol_message_id": message, "eu_peppol_error": null})).await?;
                Ok(Some(json!({"status": "Sent"})))
            }
            "eu_record_peppol_failure" => {
                if s(doc, "eu_peppol_status") != Some("Pending") {
                    return Err(Error::invalid("only an invoice waiting for its access point is recorded as failed"));
                }
                let message = param(params, "message")?.to_string();
                Self::save(hc, doc, json!({"eu_peppol_status": "Failed", "eu_peppol_error": message})).await?;
                Ok(Some(json!({"status": "Failed"})))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(v: &str) -> Decimal {
        v.parse().unwrap()
    }

    fn sample() -> Invoice {
        let party = |name: &str, vat: &str, country: &str, ep: &str| Party {
            name: name.into(),
            vat: Some(vat.into()),
            street: Some("Hauptstr. 1".into()),
            city: Some("Berlin".into()),
            postal: Some("10115".into()),
            country: Some(country.into()),
            endpoint: endpoint(Some(ep)),
            contact: None,
            tax_number: None,
            subdivision: None,
            legal_id: None,
        };
        Invoice {
            number: "SINV-26-00001".into(),
            issue_date: "2026-10-31".into(),
            due_date: Some("2026-11-30".into()),
            credit_note: false,
            original: None,
            currency: "EUR".into(),
            buyer_reference: "PO-7".into(),
            order_reference: Some("PO-7".into()),
            customization: None,
            payment: Some(Payment { code: "58".into(), iban: Some("DE89370400440532013000".into()), bic: None, account_name: None }),
            notes: vec![],
            seller: party("Seller GmbH", "DE123456789", "DE", "9930:DE123456789"),
            buyer: party("Käufer & Co", "DE987654321", "DE", "0088:4000001000005"),
            delivery_country: Some("DE".into()),
            delivery_date: Some("2026-10-31".into()),
            lines: vec![
                Line {
                    id: "1".into(),
                    name: "Advice".into(),
                    quantity: dec("2"),
                    unit: "HUR".into(),
                    price: dec("100"),
                    net: dec("200.00"),
                    category: "S",
                    percent: dec("19"),
                },
                Line {
                    id: "2".into(),
                    name: "Book".into(),
                    quantity: dec("10"),
                    unit: "H87".into(),
                    price: dec("20"),
                    net: dec("200.00"),
                    category: "S",
                    percent: dec("7"),
                },
                Line {
                    id: "3".into(),
                    name: "Cover".into(),
                    quantity: dec("1"),
                    unit: "H87".into(),
                    price: dec("50"),
                    net: dec("50.00"),
                    category: "E",
                    percent: dec("0"),
                },
            ],
            allowances: vec![],
            subtotals: vec![
                Subtotal { category: "S", percent: dec("19"), taxable: dec("200.00"), tax: dec("38.00"), reason_code: None, reason: None },
                Subtotal { category: "S", percent: dec("7"), taxable: dec("200.00"), tax: dec("14.00"), reason_code: None, reason: None },
                Subtotal {
                    category: "E",
                    percent: dec("0"),
                    taxable: dec("50.00"),
                    tax: dec("0.00"),
                    reason_code: Some("VATEX-EU-132-1E".into()),
                    reason: None,
                },
            ],
            line_total: dec("450.00"),
            allowance_total: dec("0"),
            tax_exclusive: dec("450.00"),
            tax_total: dec("52.00"),
            tax_inclusive: dec("502.00"),
            rounding: dec("0"),
            payable: dec("502.00"),
        }
    }

    #[test]
    fn a_compliant_invoice_writes_peppol_bis_ubl() {
        let inv = sample();
        assert_eq!(validate(&inv), Vec::<String>::new());
        let xml = to_xml(&inv);
        for part in [
            "<cbc:CustomizationID>urn:cen.eu:en16931:2017#compliant#urn:fdc:peppol.eu:2017:poacc:billing:3.0</cbc:CustomizationID>",
            "<cbc:EndpointID schemeID=\"0088\">4000001000005</cbc:EndpointID>",
            "<cbc:Name>Käufer &amp; Co</cbc:Name>",
            "<cbc:InvoiceTypeCode>380</cbc:InvoiceTypeCode>",
            "<cbc:TaxAmount currencyID=\"EUR\">52.00</cbc:TaxAmount>",
            "<cbc:ID>E</cbc:ID><cbc:Percent>0</cbc:Percent><cbc:TaxExemptionReasonCode>VATEX-EU-132-1E</cbc:TaxExemptionReasonCode>",
            "<cbc:PayableAmount currencyID=\"EUR\">502.00</cbc:PayableAmount>",
            "<cbc:InvoicedQuantity unitCode=\"HUR\">2</cbc:InvoicedQuantity>",
            "<cac:PaymentMeans><cbc:PaymentMeansCode>58</cbc:PaymentMeansCode><cac:PayeeFinancialAccount><cbc:ID>DE89370400440532013000</cbc:ID>",
        ] {
            assert!(xml.contains(part), "{part} in {xml}");
        }
        assert!(xml.starts_with("<?xml") && xml.ends_with("</Invoice>"));
    }

    #[test]
    fn what_breaks_en_16931_is_said() {
        let mut bad = sample();
        bad.subtotals[2].reason_code = None;
        bad.subtotals[0].tax = dec("40.00");
        bad.tax_total = dec("54.00");
        bad.seller.vat = None;
        bad.payable = dec("1.00");
        let p = validate(&bad).join("\n");
        for rule in ["BR-E-10", "BR-CO-17", "BR-S-02", "BR-CO-15", "BR-CO-16"] {
            assert!(p.contains(rule), "{rule} in {p}");
        }
        let mut k = sample();
        k.lines.truncate(1);
        k.lines[0].category = "K";
        k.lines[0].percent = Decimal::ZERO;
        k.subtotals = vec![Subtotal {
            category: "K",
            percent: Decimal::ZERO,
            taxable: dec("200.00"),
            tax: Decimal::ZERO,
            reason_code: Some("VATEX-EU-IC".into()),
            reason: None,
        }];
        (k.line_total, k.tax_exclusive, k.tax_total, k.tax_inclusive, k.payable) = (dec("200.00"), dec("200.00"), dec("0"), dec("200.00"), dec("200.00"));
        assert_eq!(validate(&k), Vec::<String>::new());
        k.delivery_country = None;
        k.buyer.vat = None;
        let p = validate(&k).join("\n");
        assert!(p.contains("BR-IC-12") && p.contains("BR-AE-02/BR-K-02"), "{p}");
    }

    #[test]
    fn credit_notes_and_plain_en_16931() {
        let mut inv = sample();
        inv.credit_note = true;
        inv.original = Some(("SINV-26-00001".into(), "2026-10-31".into()));
        inv.buyer.endpoint = None;
        let xml = to_xml(&inv);
        assert!(xml.contains("<cbc:CreditNoteTypeCode>381</cbc:CreditNoteTypeCode>") && xml.contains("<cac:BillingReference>"));
        assert!(xml.contains("<cbc:CustomizationID>urn:cen.eu:en16931:2017</cbc:CustomizationID>") && !xml.contains("ProfileID"), "no Peppol without both IDs");
        assert!(xml.contains("<cbc:CreditedQuantity") && xml.ends_with("</CreditNote>"));
        assert_eq!(category_code(Some("Reduced"), dec("7")), "S");
        assert_eq!((unit_code("Kg"), unit_code("Nos"), unit_code("Pallet")), ("KGM", "H87", "C62"));
        assert_eq!(endpoint(Some("0088:4000001000005")), Some(("0088".into(), "4000001000005".into())));
        assert_eq!(endpoint(Some("GLN:1")), None);
    }
}
