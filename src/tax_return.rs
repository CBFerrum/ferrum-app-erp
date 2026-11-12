//! Tax returns as definitions (FN-38, FN-40 in general terms). A `TaxReturnDefinition` lists boxes:
//! a sum of the tax ledger (the tax breakdown each submitted sales and purchase invoice keeps, in
//! company currency, credit notes negative, as the tax summary reads it) filtered by tax, rate,
//! category, direction, place of delivery and document kind; a formula over other boxes; or a figure
//! entered by hand. A `TaxReturn` for a period works its boxes out while it is a draft, keeps each
//! box's adjustment and reason, and once submitted (filed) keeps its values and refuses any invoice
//! of its period being submitted or cancelled. A box drills to its documents through the report
//! `tax_return_documents`. No country is named here: a country's return is a definition its add-on
//! seeds.

use crate::util::{company, d, date, id, money, rows, s, set};
use async_trait::async_trait;
use chrono::{Months, NaiveDate};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

/// One row of the tax ledger.
#[derive(Debug, Clone)]
pub struct TaxRow {
    /// The invoice's kind, which says the direction: output on a sale, input on a purchase.
    pub entity: &'static str,
    /// The document the row comes from: the invoice, or a payment whose discount took tax back.
    pub voucher: &'static str,
    pub id: Uuid,
    pub number: Option<String>,
    pub date: NaiveDate,
    pub party: Option<String>,
    pub tax: String,
    pub description: String,
    pub category: String,
    pub rate: Decimal,
    pub reverse_charge: bool,
    pub credit_note: bool,
    pub place: String,
    pub base: Decimal,
    pub amount: Decimal,
}

/// The tax ledger of a company's submitted invoices over `[from, to]`.
pub async fn ledger(tx: &mut Tx, ctx: &Ctx, company: Uuid, from: NaiveDate, to: NaiveDate) -> Result<Vec<TaxRow>> {
    let mut out = Vec::new();
    for (entity, party) in [("SalesInvoice", "customer_name"), ("PurchaseInvoice", "supplier_name")] {
        let Some(ent) = ctx.schema.entity(entity) else { continue };
        let Some(lines) = ent.children.get("tax_breakdown") else { continue };
        let place = if ent.field("delivery_country").is_some() { "i.delivery_country" } else { "null::text" };
        #[allow(clippy::type_complexity)]
        let data: Vec<(Uuid, Option<String>, NaiveDate, Option<String>, Option<String>, Option<String>, Option<String>, Option<Decimal>, Option<bool>, Option<bool>, Option<String>, Decimal, Decimal)> =
            sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "select i.id, i.number, i.posting_date, i.{party}, a.account_name, l.description, l.category, l.rate, l.reverse_charge, i.is_return, {place}, \
                 coalesce(l.base_taxable_amount, l.taxable_amount * coalesce(i.conversion_rate, 1), 0), coalesce(l.base_tax_amount, l.tax_amount * coalesce(i.conversion_rate, 1), 0) \
                 from {invoices} i join {lines} l on l.tenant_id = i.tenant_id and l.parent_id = i.id left join e_account a on a.tenant_id = l.tenant_id and a.id = l.account \
                 where i.tenant_id = $1 and i.company = $2 and i.docstatus = 1 and i.posting_date between $3 and $4 order by i.posting_date, i.number, i.id",
                invoices = ferrum_kernel::ddl::q(&ent.table),
                lines = ferrum_kernel::ddl::q(&lines.table),
            )))
            .bind(ctx.tenant)
            .bind(company)
            .bind(from)
            .bind(to)
            .fetch_all(&mut **tx)
            .await?;
        for (iid, number, day, name, account, description, category, rate, reverse, ret, place, base, amount) in data {
            out.push(TaxRow {
                entity,
                voucher: entity,
                id: iid,
                number,
                date: day,
                party: name,
                tax: account.unwrap_or_default(),
                description: description.unwrap_or_default(),
                category: category.unwrap_or_default(),
                rate: rate.unwrap_or_default(),
                reverse_charge: reverse.unwrap_or(false),
                credit_note: ret.unwrap_or(false),
                place: place.unwrap_or_default().trim().to_uppercase(),
                base,
                amount,
            });
        }
    }
    for r in discount_taxes(tx, ctx, company, from, to).await? {
        out.push(TaxRow {
            entity: r.entity,
            voucher: "PaymentEntry",
            id: r.id,
            number: r.number,
            date: r.date,
            party: r.party,
            tax: r.account_name,
            description: r.description,
            category: r.category,
            rate: r.rate,
            reverse_charge: false,
            credit_note: true,
            place: String::new(),
            base: -r.taxable,
            amount: -r.tax,
        });
    }
    Ok(out)
}

/// Tax an early-payment discount took back: a payment's row for one tax of one invoice.
pub struct DiscountTax {
    /// The invoice's kind, which says the direction (output on a sale, input on a purchase).
    pub entity: &'static str,
    /// The payment.
    pub id: Uuid,
    pub number: Option<String>,
    pub date: NaiveDate,
    pub party: Option<String>,
    pub account: Option<Uuid>,
    pub account_name: String,
    pub description: String,
    pub category: String,
    pub rate: Decimal,
    /// What the discount took off the base, and off the tax, positive, in company currency.
    pub taxable: Decimal,
    pub tax: Decimal,
}

/// The tax submitted payments of `[from, to]` took back with early-payment discounts, on the payment's day.
pub async fn discount_taxes(tx: &mut Tx, ctx: &Ctx, company: Uuid, from: NaiveDate, to: NaiveDate) -> Result<Vec<DiscountTax>> {
    let Some(ent) = ctx.schema.entity("PaymentEntry") else { return Ok(vec![]) };
    if ent.field("discount_taxes").is_none() {
        return Ok(vec![]);
    }
    #[allow(clippy::type_complexity)]
    let data: Vec<(
        Uuid,
        Option<String>,
        NaiveDate,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    )> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select p.id, p.number, p.posting_date, p.party_name, t->>'invoice_type', t->>'account', a.account_name, t->>'description', t->>'category', \
             t->>'rate', t->>'taxable', t->>'tax' from {payments} p cross join lateral jsonb_array_elements(coalesce(p.discount_taxes, '[]'::jsonb)) t \
             left join e_account a on a.tenant_id = p.tenant_id and a.id::text = t->>'account' \
             where p.tenant_id = $1 and p.company = $2 and p.docstatus = 1 and p.posting_date between $3 and $4 order by p.posting_date, p.number, p.id",
        payments = ferrum_kernel::ddl::q(&ent.table),
    )))
    .bind(ctx.tenant)
    .bind(company)
    .bind(from)
    .bind(to)
    .fetch_all(&mut **tx)
    .await?;
    let num = |v: Option<String>| v.and_then(|x| x.parse::<Decimal>().ok()).unwrap_or_default();
    Ok(data
        .into_iter()
        .map(|(pid, number, day, party, kind, account, name, description, category, rate, taxable, tax)| DiscountTax {
            entity: if kind.as_deref() == Some("PurchaseInvoice") { "PurchaseInvoice" } else { "SalesInvoice" },
            id: pid,
            number,
            date: day,
            party,
            account: account.and_then(|a| a.parse().ok()),
            account_name: name.unwrap_or_default(),
            description: description.unwrap_or_default(),
            category: category.unwrap_or_default(),
            rate: num(rate),
            taxable: num(taxable),
            tax: num(tax),
        })
        .collect())
}

/// Whether a ledger row is one a Sum box counts.
pub fn matches(b: &Doc, r: &TaxRow) -> bool {
    let text = |f: &str| s(b, f).map(str::trim).filter(|v| !v.is_empty());
    let direction = match text("direction") {
        Some("Output") => r.entity == "SalesInvoice",
        Some("Input") => r.entity == "PurchaseInvoice",
        _ => true,
    };
    let tax = text("tax").is_none_or(|t| t.eq_ignore_ascii_case(&r.tax) || t.eq_ignore_ascii_case(&r.description));
    let category = text("category").is_none_or(|c| c == r.category);
    let rate = b.get("rate").filter(|v| !v.is_null() && *v != "").is_none_or(|_| d(b, "rate") == r.rate);
    let reverse = match text("reverse_charge") {
        Some("Yes") => r.reverse_charge,
        Some("No") => !r.reverse_charge,
        _ => true,
    };
    let place = match text("place") {
        None => true,
        Some(p) => match p.strip_prefix('!') {
            Some(not) => r.place.is_empty() || !r.place.eq_ignore_ascii_case(not),
            None => r.place.eq_ignore_ascii_case(p),
        },
    };
    let document = match text("document") {
        Some("Invoice") => !r.credit_note,
        Some("Credit Note") => r.credit_note,
        _ => true,
    };
    direction && tax && category && rate && reverse && place && document
}

/// The last day of a period of `frequency` starting on `from`.
pub fn period_end(from: NaiveDate, frequency: Option<&str>) -> NaiveDate {
    let months = match frequency {
        Some("Bi-monthly") => 2,
        Some("Quarterly") => 3,
        Some("Half-yearly") => 6,
        Some("Yearly") => 12,
        _ => 1,
    };
    from.checked_add_months(Months::new(months)).and_then(|d| d.pred_opt()).unwrap_or(from)
}

/// Works out the boxes: sums from the ledger, then formulas in order over the values so far (each
/// value is what was computed plus its adjustment). `kept` carries each box's adjustment and reason.
pub fn compute(defs: &[Doc], ledger: &[TaxRow], kept: &HashMap<String, (Decimal, Option<String>)>, precision: u32) -> std::result::Result<Vec<Doc>, String> {
    let mut values: HashMap<String, Decimal> = HashMap::new();
    let mut out = Vec::new();
    for b in defs {
        let code = s(b, "code").unwrap_or_default().trim().to_string();
        let computed = match s(b, "kind").unwrap_or("Sum") {
            "Formula" => formula::eval(s(b, "formula").unwrap_or_default(), &values).map_err(|e| format!("box {code}: {e}"))?.round_dp(precision),
            "By Hand" => Decimal::ZERO,
            _ => {
                let base = s(b, "measure") == Some("Base");
                ledger.iter().filter(|r| matches(b, r)).map(|r| if base { r.base } else { r.amount }).sum::<Decimal>().round_dp(precision)
            }
        };
        let (adjustment, reason) = kept.get(&code).cloned().unwrap_or_default();
        values.insert(code.clone(), computed + adjustment);
        let mut row = Doc::new();
        row.insert("code".into(), json!(code));
        row.insert("label".into(), json!(s(b, "label")));
        row.insert("computed".into(), money(computed));
        row.insert("adjustment".into(), money(adjustment));
        row.insert("adjustment_reason".into(), json!(reason));
        row.insert("value".into(), money(computed + adjustment));
        out.push(row);
    }
    Ok(out)
}

pub struct ReturnHook;

#[async_trait]
impl DocHook for ReturnHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let (Some(def), Some(cid), Ok(from)) = (id(doc, "definition"), id(doc, "company"), date(doc, "from_date")) else { return Ok(()) };
        let def = crate::util::get(hc.kernel, hc.ctx, hc.tx, "TaxReturnDefinition", def).await?;
        let to = period_end(from, s(&def, "frequency"));
        set(doc, "to_date", to.to_string());
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let kept: HashMap<String, (Decimal, Option<String>)> = rows(doc, "boxes")
            .iter()
            .filter_map(|r| Some((s(r, "code")?.to_string(), (d(r, "adjustment"), s(r, "adjustment_reason").map(str::to_string)))))
            .collect();
        let ledger = ledger(hc.tx, hc.ctx, cid, from, to).await?;
        let boxes = compute(&rows(&def, "boxes"), &ledger, &kept, co.precision).map_err(|e| Error::field("definition", e))?;
        // Keep row ids, so an edit of an adjustment updates its row.
        let old: HashMap<String, Value> = rows(doc, "boxes").iter().filter_map(|r| Some((s(r, "code")?.to_string(), r.get("id")?.clone()))).collect();
        let boxes: Vec<Value> = boxes
            .into_iter()
            .map(|mut b| {
                let kept = s(&b, "code").and_then(|c| old.get(c)).cloned();
                b.insert("id".into(), kept.unwrap_or_else(|| json!(Uuid::now_v7().to_string())));
                Value::Object(b)
            })
            .collect();
        set(doc, "boxes", Value::Array(boxes));
        Ok(())
    }

    async fn validate(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let mut errors = Vec::new();
        for (i, r) in rows(doc, "boxes").iter().enumerate() {
            if !d(r, "adjustment").is_zero() && s(r, "adjustment_reason").is_none_or(|x| x.trim().is_empty()) {
                errors.push(FieldError::new("adjustment_reason", "say why the box is adjusted").in_row("boxes", i));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let rid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let table = hc.entity.table.clone();
        let clash: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select coalesce(number, '') from {} where tenant_id = $1 and company = $2 and definition = $3 and docstatus = 1 and id <> $4 \
             and from_date <= $6 and to_date >= $5 limit 1",
            ferrum_kernel::ddl::q(&table)
        )))
        .bind(hc.ctx.tenant)
        .bind(id(doc, "company"))
        .bind(id(doc, "definition"))
        .bind(rid)
        .bind(date(doc, "from_date")?)
        .bind(date(doc, "to_date")?)
        .fetch_optional(&mut **hc.tx)
        .await?;
        if let Some(n) = clash {
            return Err(Error::invalid(format!("the return {n} is filed for this period: cancel it to file again")));
        }
        let mut v = Doc::new();
        v.insert("filed_on".into(), json!(hc.ctx.today().to_string()));
        hc.kernel.set_values_tx(&crate::util::sys(hc.ctx), hc.tx, "TaxReturn", rid, v).await
    }
}

/// Refuses to submit or cancel an invoice dated in a filed return's period: its figures are kept as filed.
pub async fn check_open(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
    let Some(ent) = hc.ctx.schema.entity("TaxReturn") else { return Ok(()) };
    let (Some(cid), Ok(on)) = (id(doc, "company"), date(doc, "posting_date")) else { return Ok(()) };
    let filed: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select coalesce(number, '') from {} where tenant_id = $1 and company = $2 and docstatus = 1 and $3 between from_date and to_date limit 1",
        ferrum_kernel::ddl::q(&ent.table)
    )))
    .bind(hc.ctx.tenant)
    .bind(cid)
    .bind(on)
    .fetch_optional(&mut **hc.tx)
    .await?;
    match filed {
        Some(n) => Err(Error::invalid(format!(
            "the tax return {n} is filed for {on}: an invoice of its period is no longer submitted or cancelled; date it in an open period"
        ))),
        None => Ok(()),
    }
}

/// The documents behind a box of a return (`tax_return`, `box`): each with its base and tax.
pub async fn documents(k: &Kernel, ctx: &Ctx, tx: &mut Tx, ret: Uuid, code: &str) -> Result<Value> {
    let r = crate::util::get(k, ctx, tx, "TaxReturn", ret).await?;
    let def = crate::util::get(k, ctx, tx, "TaxReturnDefinition", id(&r, "definition").unwrap_or_default()).await?;
    let b = rows(&def, "boxes")
        .into_iter()
        .find(|b| s(b, "code").map(str::trim) == Some(code))
        .ok_or_else(|| Error::field("box", format!("the return has no box {code}")))?;
    if s(&b, "kind").unwrap_or("Sum") != "Sum" {
        return Err(Error::field("box", format!("{code} is not a sum of documents")));
    }
    let rows_: Vec<TaxRow> = ledger(tx, ctx, id(&r, "company").unwrap_or_default(), date(&r, "from_date")?, date(&r, "to_date")?).await?;
    let base = s(&b, "measure") == Some("Base");
    let mut total = Decimal::ZERO;
    let mut out = Vec::new();
    for x in rows_.iter().filter(|x| matches(&b, x)) {
        let v = if base { x.base } else { x.amount };
        total += v;
        out.push(json!({"voucher_type": x.voucher, "voucher_id": x.id, "voucher_no": x.number, "posting_date": x.date, "party": x.party,
                        "tax": x.tax, "rate": money(x.rate), "taxable": money(x.base), "tax_amount": money(x.amount), "counted": money(v)}));
    }
    Ok(json!({
        "columns": [ferrum_domain::reports::col("voucher_no", "Invoice", "data"), ferrum_domain::reports::col("posting_date", "Date", "date"),
                    ferrum_domain::reports::col("party", "Party", "data"), ferrum_domain::reports::col("tax", "Tax", "data"),
                    ferrum_domain::reports::col("rate", "Rate", "decimal"), ferrum_domain::reports::col("taxable", "Taxable Base", "currency"),
                    ferrum_domain::reports::col("tax_amount", "Tax", "currency"), ferrum_domain::reports::col("counted", "In the Box", "currency")],
        "rows": out,
        "totals": {"counted": money(total)},
        "meta": {"tax_return": ret, "box": code},
    }))
}

/// Box formulas: numbers and box codes with + - * / and parentheses, and max(a, b), min(a, b), in
/// exact decimals (not the platform's expressions, which compute in floating point).
pub mod formula {
    use rust_decimal::Decimal;
    use std::collections::HashMap;

    pub fn eval(src: &str, vars: &HashMap<String, Decimal>) -> Result<Decimal, String> {
        let toks = lex(src)?;
        let mut p = P { t: &toks, i: 0, vars };
        let v = p.sum()?;
        if p.i != toks.len() {
            return Err(format!("unexpected {:?}", toks[p.i]));
        }
        Ok(v)
    }

    #[derive(Debug, Clone, PartialEq)]
    enum T {
        Num(Decimal),
        Id(String),
        Op(char),
    }

    fn lex(src: &str) -> Result<Vec<T>, String> {
        let c: Vec<char> = src.chars().collect();
        let (mut i, mut out) = (0, Vec::new());
        while i < c.len() {
            let ch = c[i];
            if ch.is_whitespace() {
                i += 1;
            } else if "+-*/(),".contains(ch) {
                out.push(T::Op(ch));
                i += 1;
            } else if ch.is_ascii_digit() || ch == '.' {
                let s = i;
                while i < c.len() && (c[i].is_ascii_digit() || c[i] == '.') {
                    i += 1;
                }
                let n: String = c[s..i].iter().collect();
                out.push(T::Num(n.parse().map_err(|_| format!("{n} is not a number"))?));
            } else if ch.is_alphanumeric() || ch == '_' {
                let s = i;
                while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '.') {
                    i += 1;
                }
                out.push(T::Id(c[s..i].iter().collect()));
            } else {
                return Err(format!("{ch} is not understood"));
            }
        }
        Ok(out)
    }

    struct P<'a> {
        t: &'a [T],
        i: usize,
        vars: &'a HashMap<String, Decimal>,
    }

    impl P<'_> {
        fn peek(&self) -> Option<&T> {
            self.t.get(self.i)
        }
        fn eat(&mut self, op: char) -> bool {
            if self.peek() == Some(&T::Op(op)) {
                self.i += 1;
                true
            } else {
                false
            }
        }
        fn sum(&mut self) -> Result<Decimal, String> {
            let mut v = self.product()?;
            loop {
                if self.eat('+') {
                    v += self.product()?;
                } else if self.eat('-') {
                    v -= self.product()?;
                } else {
                    return Ok(v);
                }
            }
        }
        fn product(&mut self) -> Result<Decimal, String> {
            let mut v = self.unary()?;
            loop {
                if self.eat('*') {
                    v *= self.unary()?;
                } else if self.eat('/') {
                    let d = self.unary()?;
                    if d.is_zero() {
                        return Err("division by zero".into());
                    }
                    v /= d;
                } else {
                    return Ok(v);
                }
            }
        }
        fn unary(&mut self) -> Result<Decimal, String> {
            if self.eat('-') {
                return Ok(-self.unary()?);
            }
            match self.t.get(self.i).cloned() {
                Some(T::Num(n)) => {
                    self.i += 1;
                    Ok(n)
                }
                Some(T::Op('(')) => {
                    self.i += 1;
                    let v = self.sum()?;
                    if !self.eat(')') {
                        return Err("a ( is not closed".into());
                    }
                    Ok(v)
                }
                Some(T::Id(name)) => {
                    self.i += 1;
                    let f = name.to_lowercase();
                    if (f == "max" || f == "min") && self.eat('(') {
                        let a = self.sum()?;
                        if !self.eat(',') {
                            return Err(format!("{f} takes two values"));
                        }
                        let b = self.sum()?;
                        if !self.eat(')') {
                            return Err(format!("{f}( is not closed"));
                        }
                        return Ok(if f == "max" { a.max(b) } else { a.min(b) });
                    }
                    self.vars.get(&name).copied().ok_or_else(|| format!("{name} is not a box before this one"))
                }
                other => Err(format!("a value was expected, not {other:?}")),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn formulas_are_exact() {
            let vars: HashMap<String, Decimal> = [("T1".to_string(), Decimal::new(1001, 2)), ("T2".to_string(), Decimal::new(2002, 2))].into();
            assert_eq!(eval("max(T1 - T2, 0)", &vars), Ok(Decimal::ZERO));
            assert_eq!(eval("max(T2 - T1, 0)", &vars), Ok(Decimal::new(1001, 2)));
            assert_eq!(eval("(T1 + T2) * 2 - -1", &vars), Ok(Decimal::new(6106, 2)));
            assert_eq!(eval("0.1 + 0.2", &vars), Ok(Decimal::new(3, 1)), "decimals, not floating point");
            assert!(eval("T3 + 1", &vars).unwrap_err().contains("T3"));
            assert!(eval("T1 / 0", &vars).is_err());
            assert!(eval("max(T1", &vars).is_err());
        }
    }
}
