//! Payment runs (FN-26) and early-payment discounts taken by themselves (FN-27).
//!
//! A run proposes the suppliers' open bills due by a day, and those not yet due whose early-payment
//! discount is still open on the pay date (paid less the discount), leaving out bills on hold, bills
//! another live run holds and suppliers whose total is under the run's least; a person adjusts the
//! lines; submitting it is the approval (the workspace's approval policy on Payment Run, when it sets
//! one); Make Payments then pays each supplier with one payment entry, each in a savepoint of its own,
//! so one that is refused is marked Failed with why and the others stand, and sends that supplier its
//! remittance advice (`finance.remittance_advice`). The run is locked while it pays and a line with a
//! payment is never paid again, so pressing it twice, or at once, pays nothing twice; the ledger's
//! allocation refuses more than is outstanding besides.
//!
//! A bill's discount is its payment terms': each installment still open whose discount date is on or
//! after the pay date gives its percent of what is open of it. The template says where the discount
//! is booked for each company and whether it reduces the tax; without an account for the company it is
//! not taken by itself.

use crate::common::installed;
use crate::rules;
use async_trait::async_trait;
use chrono::NaiveDate;
use ferrum_app_erp::ledger;
use ferrum_domain::util::{b, company, d, date, get, id, money, round, rows, s, set, sys};
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

fn obj(v: Value) -> Doc {
    v.as_object().cloned().unwrap_or_default()
}

/// An early-payment discount open on an invoice.
pub struct Discount {
    pub amount: Decimal,
    pub account: Uuid,
    pub reduces_tax: bool,
}

/// The discount open on `invoice` (a sales or purchase invoice with payment terms) for a payment on
/// `on`, given what is still outstanding of it; `None` when there is none or nowhere to book it.
pub async fn discount_for(k: &Kernel, ctx: &Ctx, tx: &mut Tx, entity: &str, invoice: Uuid, on: NaiveDate, outstanding: Decimal) -> Result<Option<Discount>> {
    if !matches!(entity, "SalesInvoice" | "PurchaseInvoice") || outstanding <= Decimal::ZERO {
        return Ok(None);
    }
    let inv = get(k, ctx, tx, entity, invoice).await?;
    let schedule = rows(&inv, "fin_payment_schedule");
    let Some(template) = id(&inv, "fin_payment_terms_template") else { return Ok(None) };
    if schedule.iter().all(|r| d(r, "discount_percent").is_zero()) {
        return Ok(None);
    }
    let template = get(k, ctx, tx, "PaymentTermsTemplate", template).await?;
    let cid = id(&inv, "company").unwrap_or_default();
    let field = if entity == "SalesInvoice" { "allowed_account" } else { "received_account" };
    let Some(account) = rows(&template, "discount_accounts").iter().find(|r| id(r, "company") == Some(cid)).and_then(|r| id(r, field)) else { return Ok(None) };
    let co = company(k, ctx, tx, cid).await?;
    let (_, precision) = ferrum_domain::util::currency(k, ctx, tx, id(&inv, "currency").unwrap_or(co.currency_id)).await?;
    let amounts: Vec<Decimal> = schedule.iter().map(|r| d(r, "payment_amount")).collect();
    let total: Decimal = amounts.iter().sum();
    let open = rules::installments_open(&amounts, total - outstanding);
    let mut amount = Decimal::ZERO;
    for (r, left) in schedule.iter().zip(open) {
        let in_time = date(r, "discount_date").is_ok_and(|last| last >= on);
        if in_time && left > Decimal::ZERO {
            amount += round(left * d(r, "discount_percent") / Decimal::ONE_HUNDRED, precision);
        }
    }
    Ok((amount > Decimal::ZERO).then_some(Discount { amount: amount.min(outstanding), account, reduces_tax: b(&template, "discount_reduces_tax") }))
}

/// Takes an invoice's early-payment discount by itself on a payment made in time: on each allocation
/// that names no discount, as much of the open discount as what is allocated leaves room for (an
/// allocation of the whole outstanding takes none). A discount given by hand, 0 included, is kept.
pub struct DiscountHook;

#[async_trait]
impl DocHook for DiscountHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if !installed(hc.ctx) || b(doc, "is_refund") || s(doc, "payment_type") == Some("Internal Transfer") {
            return Ok(());
        }
        let Ok(on) = date(doc, "posting_date") else { return Ok(()) };
        let mut refs = rows(doc, "references");
        let mut changed = false;
        for r in refs.iter_mut() {
            if r.get("discount_amount").is_some_and(|v| !v.is_null() && v != "") {
                continue;
            }
            let Some(inv) = id(r, "invoice") else { continue };
            let Some(item) = ledger::load_open_item(hc.tx, hc.ctx.tenant, inv, false).await? else { continue };
            let Some(disc) = discount_for(hc.kernel, hc.ctx, hc.tx, &item.entity, inv, on, item.outstanding).await? else { continue };
            let take = disc.amount.min(item.outstanding - d(r, "allocated_amount"));
            if take > Decimal::ZERO {
                set(r, "discount_amount", money(take));
                set(r, "discount_account", disc.account.to_string());
                set(r, "discount_reduces_tax", disc.reduces_tax);
                changed = true;
            }
        }
        if changed {
            set(doc, "references", Value::Array(refs.into_iter().map(Value::Object).collect()));
        }
        Ok(())
    }
}

/// Holding a supplier's bill from payment, and releasing it.
pub struct HoldHook;

#[async_trait]
impl DocHook for HoldHook {
    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        let hold = match name {
            "fin_hold_payment" => true,
            "fin_release_payment" => false,
            _ => return Ok(None),
        };
        let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let reason = params.get("reason").and_then(Value::as_str).map(str::trim).filter(|r| !r.is_empty());
        if hold && reason.is_none() {
            return Err(Error::field("reason", "say why it is held"));
        }
        let mut v = Doc::new();
        v.insert("fin_payment_hold".into(), json!(hold));
        v.insert("fin_hold_reason".into(), json!(reason));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "PurchaseInvoice", pid, v).await?;
        Ok(Some(Value::Object(hc.kernel.get_tx(hc.ctx, hc.tx, "PurchaseInvoice", pid, false).await?)))
    }
}

pub struct RunHook;

fn pays(r: &Doc) -> bool {
    b(r, "pay") && id(r, "payment_entry").is_none()
}

#[async_trait]
impl DocHook for RunHook {
    async fn before_validate(&self, _hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let total: Decimal = rows(doc, "lines").iter().filter(|r| b(r, "pay")).map(|r| d(r, "amount")).sum();
        set(doc, "total_amount", money(total));
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let bank = get(hc.kernel, hc.ctx, hc.tx, "Account", id(doc, "bank_account").unwrap_or_default()).await?;
        if id(&bank, "company") != Some(cid) || !matches!(s(&bank, "account_type"), Some("Bank" | "Cash")) {
            return Err(Error::field("bank_account", "choose a Bank or Cash account of this company"));
        }
        let mut errors = Vec::new();
        for (i, r) in rows(doc, "lines").iter().enumerate().filter(|(_, r)| b(r, "pay")) {
            if d(r, "amount") <= Decimal::ZERO {
                errors.push(FieldError::new("amount", "pays nothing: untick it or give an amount").in_row("lines", i));
            } else if d(r, "amount") + d(r, "discount") > d(r, "outstanding") {
                errors.push(
                    FieldError::new("amount", format!("with the discount it pays more than the {} outstanding", d(r, "outstanding").normalize()))
                        .in_row("lines", i),
                );
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if !rows(doc, "lines").iter().any(|r| b(r, "pay")) {
            return Err(Error::field("lines", "there is nothing to pay: propose the bills first"));
        }
        let mut v = Doc::new();
        v.insert("run_status".into(), json!("Approved"));
        hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "PaymentRun", id(doc, "id").unwrap_or_default(), v).await
    }

    async fn on_cancel(&self, _hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if rows(doc, "lines").iter().any(|r| id(r, "payment_entry").is_some()) {
            return Err(Error::invalid("this run has made payments: they stand on their own, cancel them first"));
        }
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        let rid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        match name {
            "propose" => propose(hc, rid).await?,
            "make_payments" => make_payments(hc, rid).await?,
            _ => return Ok(None),
        }
        Ok(Some(Value::Object(hc.kernel.get_tx(hc.ctx, hc.tx, "PaymentRun", rid, false).await?)))
    }
}

async fn propose(hc: &mut HookCtx<'_>, rid: Uuid) -> Result<()> {
    let run = hc.kernel.get_tx(hc.ctx, hc.tx, "PaymentRun", rid, true).await?;
    let cid = id(&run, "company").ok_or_else(|| Error::field("company", "is required"))?;
    let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
    let (code, _) = ferrum_domain::util::currency(hc.kernel, hc.ctx, hc.tx, id(&run, "currency").unwrap_or(co.currency_id)).await?;
    let on = date(&run, "posting_date")?;
    let due_by = date(&run, "due_by")?;
    let (Some(lines), Some(bills)) = (hc.ctx.schema.entity("PaymentRun").and_then(|e| e.children.get("lines")), hc.ctx.schema.entity("PurchaseInvoice")) else {
        return Err(Error::internal("payment runs need buying"));
    };
    let runs = hc.ctx.schema.entity("PaymentRun").map(|e| e.table.clone()).unwrap_or_default();
    #[allow(clippy::type_complexity)]
    let open: Vec<(Uuid, String, Option<String>, Uuid, Option<NaiveDate>, Decimal)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select o.id, o.entity, o.number, o.supplier_id, o.due_date, o.outstanding from open_item o join e_supplier s on s.tenant_id = o.tenant_id and s.id = o.supplier_id \
         where o.tenant_id = $1 and o.company_id = $2 and o.status = 'open' and o.outstanding > 0 and trim(o.currency) = $3 \
         and ($4::uuid is null or o.supplier_id = $4) and ($5::uuid is null or s.supplier_group = $5) \
         and not exists (select 1 from {bills} p where p.tenant_id = o.tenant_id and p.id = o.id and p.fin_payment_hold) \
         and not exists (select 1 from {lines} l join {runs} r on r.tenant_id = l.tenant_id and r.id = l.parent_id where l.tenant_id = o.tenant_id \
             and l.invoice = o.id::text and r.id <> $6 and r.docstatus < 2 and l.pay and l.payment_entry is null and coalesce(l.state, 'Proposed') <> 'Failed') \
         order by s.supplier_name, o.supplier_id, coalesce(o.due_date, o.posting_date), o.posting_date limit 1000",
        bills = ferrum_kernel::ddl::q(&bills.table),
        lines = ferrum_kernel::ddl::q(&lines.table),
        runs = ferrum_kernel::ddl::q(&runs),
    )))
    .bind(hc.ctx.tenant)
    .bind(cid)
    .bind(&code)
    .bind(id(&run, "supplier"))
    .bind(id(&run, "supplier_group"))
    .bind(rid)
    .fetch_all(&mut **hc.tx)
    .await?;
    let mut chosen: Vec<Doc> = Vec::new();
    for (item, entity, number, supplier, due, outstanding) in open {
        let discount = if b(&run, "take_discounts") { discount_for(hc.kernel, hc.ctx, hc.tx, &entity, item, on, outstanding).await? } else { None };
        if due.is_some_and(|d| d > due_by) && discount.is_none() {
            continue;
        }
        let taken = discount.as_ref().map(|x| x.amount).unwrap_or_default();
        let sup = get(hc.kernel, hc.ctx, hc.tx, "Supplier", supplier).await?;
        let mut line = obj(json!({
            "pay": true, "supplier": supplier, "supplier_name": s(&sup, "supplier_name"), "iban": s(&sup, "fin_iban"), "bic": s(&sup, "fin_bic"), "invoice": item, "invoice_type": hc.ctx.schema.entity(&entity).map(|e| e.def.label.clone()).unwrap_or(entity),
            "invoice_number": number, "due_date": due, "outstanding": money(outstanding), "discount": money(taken), "amount": money(outstanding - taken),
            "state": "Proposed",
        }));
        if let Some(x) = discount {
            line.insert("discount_account".into(), json!(x.account));
            line.insert("discount_reduces_tax".into(), json!(x.reduces_tax));
        }
        chosen.push(line);
    }
    // A supplier whose bills come to less than the least waits for a later run.
    let least = d(&run, "minimum_amount");
    let mut per: BTreeMap<String, Decimal> = BTreeMap::new();
    for l in &chosen {
        *per.entry(s(l, "supplier").unwrap_or_default().to_string()).or_default() += d(l, "amount");
    }
    chosen.retain(|l| per.get(s(l, "supplier").unwrap_or_default()).copied().unwrap_or_default() >= least);
    let total: Decimal = chosen.iter().map(|l| d(l, "amount")).sum();
    let sctx = sys(hc.ctx);
    hc.kernel.set_child_rows_tx(&sctx, hc.tx, "PaymentRun", "lines", rid, chosen).await?;
    let mut v = Doc::new();
    v.insert("total_amount".into(), money(total));
    v.insert("currency_code".into(), json!(code));
    hc.kernel.set_values_tx(&sctx, hc.tx, "PaymentRun", rid, v).await
}

async fn make_payments(hc: &mut HookCtx<'_>, rid: Uuid) -> Result<()> {
    // Locked and read again: a second press waits here, then finds the lines paid.
    let run = hc.kernel.get_tx(&sys(hc.ctx), hc.tx, "PaymentRun", rid, true).await?;
    if run.get("docstatus").and_then(Value::as_i64) != Some(1) {
        return Err(Error::invalid("a run is paid once it is submitted (approved)"));
    }
    let lines = rows(&run, "lines");
    let mut by_supplier: Vec<(Uuid, Vec<Doc>)> = Vec::new();
    for l in lines.iter().filter(|l| pays(l)) {
        let supplier = id(l, "supplier").unwrap_or_default();
        match by_supplier.iter_mut().find(|(s, _)| *s == supplier) {
            Some((_, ls)) => ls.push(l.clone()),
            None => by_supplier.push((supplier, vec![l.clone()])),
        }
    }
    let number = s(&run, "number").unwrap_or_default().to_string();
    let sctx = sys(hc.ctx);
    for (supplier, ls) in by_supplier {
        sqlx::query("savepoint payment_run_supplier").execute(&mut **hc.tx).await?;
        let made = pay_supplier(hc, &run, supplier, &ls, &number).await;
        let (state, payment, error) = match made {
            Ok(pid) => {
                sqlx::query("release savepoint payment_run_supplier").execute(&mut **hc.tx).await?;
                ("Paid", Some(pid), None)
            }
            Err(e) => {
                sqlx::query("rollback to savepoint payment_run_supplier").execute(&mut **hc.tx).await?;
                ("Failed", None, Some(e.to_string()))
            }
        };
        for l in &ls {
            let mut v = Doc::new();
            v.insert("state".into(), json!(state));
            v.insert("payment_entry".into(), json!(payment));
            v.insert("error".into(), json!(error));
            hc.kernel.set_child_values_tx(&sctx, hc.tx, "PaymentRun", "lines", id(l, "id").unwrap_or_default(), v).await?;
        }
    }
    let after = rows(&hc.kernel.get_tx(&sctx, hc.tx, "PaymentRun", rid, false).await?, "lines");
    let wanted: Vec<&Doc> = after.iter().filter(|l| b(l, "pay")).collect();
    let paid: Decimal = wanted.iter().filter(|l| id(l, "payment_entry").is_some()).map(|l| d(l, "amount")).sum();
    let failed = wanted.iter().filter(|l| s(l, "state") == Some("Failed")).count();
    let done = wanted.iter().all(|l| id(l, "payment_entry").is_some());
    let mut v = Doc::new();
    v.insert("paid_amount".into(), money(paid));
    v.insert("failed_count".into(), json!(failed));
    v.insert("run_status".into(), json!(if done { "Paid" } else { "Partly Paid" }));
    hc.kernel.set_values_tx(&sctx, hc.tx, "PaymentRun", rid, v).await?;
    // The bank's file of what was paid, kept on the run (made again, replacing it, when it pays more).
    if let Some(template) = s(&run, "bank_file").map(str::trim).filter(|t| !t.is_empty())
        && !paid.is_zero()
    {
        let g = ferrum_kernel::documents::Generate {
            template: template.to_string(),
            record: rid,
            name: Some(format!("{number} bank file")),
            language: None,
            replace: true,
            values: Default::default(),
        };
        ferrum_kernel::documents::generate(hc.kernel, hc.tx, &sctx, &g).await?;
    }
    Ok(())
}

/// One payment entry for a supplier's lines, made and submitted as the person making the run's
/// payments (their right to pay is theirs, not the run's), and its remittance advice.
async fn pay_supplier(hc: &mut HookCtx<'_>, run: &Doc, supplier: Uuid, ls: &[Doc], number: &str) -> Result<Uuid> {
    let references: Vec<Value> = ls
        .iter()
        .map(|l| {
            json!({"invoice": s(l, "invoice"), "allocated_amount": money(d(l, "amount")), "discount_amount": money(d(l, "discount")),
                   "discount_account": s(l, "discount_account"), "discount_reduces_tax": b(l, "discount_reduces_tax")})
        })
        .collect();
    let paid: Decimal = ls.iter().map(|l| d(l, "amount")).sum();
    let pe = hc
        .kernel
        .insert_tx(
            hc.ctx,
            hc.tx,
            "PaymentEntry",
            obj(json!({
                "company": s(run, "company"), "posting_date": s(run, "posting_date"), "payment_type": "Pay", "supplier": supplier,
                "bank_account": s(run, "bank_account"), "mode_of_payment": s(run, "mode_of_payment"), "currency": s(run, "currency"),
                "paid_amount": money(paid), "references": references, "remarks": format!("Payment run {number}"),
            })),
        )
        .await?;
    let pid = id(&pe, "id").ok_or_else(|| Error::internal("no id"))?;
    let pe = hc.kernel.submit_tx(hc.ctx, hc.tx, "PaymentEntry", pid, None).await?;
    let sctx = sys(hc.ctx);
    let to = ferrum_kernel::messages::recipients(hc.kernel, hc.tx, &sctx, "PaymentEntry", &Value::Object(pe.clone()), "field:supplier").await?;
    let bills: Vec<String> = ls
        .iter()
        .map(|l| format!("{} ({})", s(l, "invoice_number").unwrap_or_default(), money(d(l, "amount") + d(l, "discount")).as_str().unwrap_or_default()))
        .collect();
    let mut values = serde_json::Map::new();
    values.insert("bills".into(), json!(bills.join(", ")));
    let m = ferrum_kernel::messages::Message {
        template: "finance.remittance_advice".into(),
        entity: Some("PaymentEntry".into()),
        record: Some(pid),
        to,
        values,
        ..Default::default()
    };
    ferrum_kernel::messages::send(hc.kernel, hc.tx, &sctx, &m).await?;
    Ok(pid)
}
