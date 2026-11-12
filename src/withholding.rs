//! Withholding by category (FN-28). A category says when it applies (on a supplier's bill, on a
//! payment to a supplier, or collected on a sale), on what (the net or the grand total), its rates
//! and thresholds by date, how the base is taken once a yearly threshold is crossed, how amounts
//! round, and the account each company books it to. A party's certificate lowers the rate up to a
//! limit. Nothing here names a country: a country's categories are records, its add-on's to seed.
//!
//! Each document keeps what it withheld in `withholdings` (company currency), even when nothing
//! was: those rows are the party's year so far, which thresholds read, and what the report lists.
//! A submitted document's rows count; a cancelled one's do not, and its postings are reversed.

use crate::ledger::{Line, load_open_item};
use crate::pricing::{Side, breakup};
use crate::util::{Company, b, d, date, get, id, money, round, rows, s, set};
use chrono::{Months, NaiveDate};
use ferrum_kernel::{Ctx, Doc, Error, HookCtx, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

/// The documents that keep withholdings: (entity, its party field, the party's name on it).
pub(crate) const KEEPERS: &[(&str, &str, &str)] =
    &[("PurchaseInvoice", "supplier", "supplier_name"), ("PaymentEntry", "supplier", "party_name"), ("SalesInvoice", "customer", "customer_name")];

/// A category as it stands on a day.
struct Category {
    id: Uuid,
    applies: String,
    gross: bool,
    whole_year: bool,
    unit: bool,
    rate: Decimal,
    single: Option<Decimal>,
    yearly: Option<Decimal>,
}

impl Category {
    fn amount(&self, taxable: Decimal, rate: Decimal, precision: u32) -> Decimal {
        rounded(taxable * rate / Decimal::ONE_HUNDRED, self.unit, precision)
    }
}

/// To a whole unit when the category says so, else to the currency's smallest unit.
fn rounded(v: Decimal, unit: bool, precision: u32) -> Decimal {
    if unit { round(v, 0) } else { round(v, precision) }
}

/// The category with its rate and thresholds in force on `on`; none before its first rate.
async fn category(hc: &mut HookCtx<'_>, cat: Uuid, on: NaiveDate) -> Result<Option<Category>> {
    let c = get(hc.kernel, hc.ctx, hc.tx, "WithholdingCategory", cat).await?;
    let on = on.to_string();
    let Some(r) = rows(&c, "rates")
        .into_iter()
        .filter(|r| s(r, "valid_from").is_some_and(|f| f <= on.as_str()))
        .max_by(|a, b| s(a, "valid_from").cmp(&s(b, "valid_from")))
    else {
        return Ok(None);
    };
    let given = |f: &str| r.get(f).filter(|v| !v.is_null()).map(|_| d(&r, f));
    Ok(Some(Category {
        id: cat,
        applies: s(&c, "applies").unwrap_or("On Bill").to_string(),
        gross: s(&c, "base") == Some("Grand Total"),
        whole_year: s(&c, "cumulative_base") != Some("Above Threshold"),
        unit: b(&c, "round_to_unit"),
        rate: d(&r, "rate"),
        single: given("single_threshold"),
        yearly: given("yearly_threshold"),
    }))
}

/// The part of `base` withheld on. `prior` is the party's base in the category earlier in the
/// fiscal year and `taxed` what of it was withheld on. Without thresholds, all of it. Below both,
/// nothing. Above the per-document threshold, the document's whole base. Once the year crosses the
/// yearly threshold: with `whole_year`, this base and the year's earlier base not yet withheld on;
/// otherwise only the part of the year's base above the threshold that this document brings.
fn taxable(base: Decimal, prior: Decimal, taxed: Decimal, single: Option<Decimal>, yearly: Option<Decimal>, whole_year: bool) -> Decimal {
    if base <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let over_single = single.is_some_and(|t| base > t);
    let over_year = yearly.is_some_and(|t| prior + base > t);
    match (single, yearly) {
        (None, None) => base,
        _ if over_year && whole_year => base + (prior - taxed).max(Decimal::ZERO),
        _ if over_single => base,
        (_, Some(t)) if over_year => base.min(prior + base - t),
        _ => Decimal::ZERO,
    }
}

/// What a party's submitted documents other than `this` keep, by entity: (base, withheld on,
/// advance base used by bills, base of payments for no bill), in a category or under a
/// certificate, posted in `[from, to)` when given.
#[allow(clippy::too_many_arguments)]
async fn kept(
    tx: &mut Tx,
    ctx: &Ctx,
    party_field: &str,
    party: Uuid,
    category: Option<Uuid>,
    certificate: Option<Uuid>,
    period: Option<(NaiveDate, NaiveDate)>,
    this: Uuid,
) -> Result<BTreeMap<&'static str, [Decimal; 4]>> {
    let mut out = BTreeMap::new();
    for (entity, field, _) in KEEPERS.iter().filter(|(_, f, _)| *f == party_field) {
        let Some(ent) = ctx.schema.entity(entity) else { continue };
        let Some(lines) = ent.children.get("withholdings") else { continue };
        let row: (Decimal, Decimal, Decimal, Decimal) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "select coalesce(sum(w.base), 0), coalesce(sum(w.taxable), 0), coalesce(sum(w.advance_base), 0), coalesce(sum(w.base) filter (where w.against is null), 0) \
             from {p} p join {w} w on w.tenant_id = p.tenant_id and w.parent_id = p.id where p.tenant_id = $1 and p.docstatus = 1 and p.{field} = $2 \
             and ($3::uuid is null or w.category = $3) and ($4::uuid is null or w.certificate = $4) and ($5::date is null or p.posting_date >= $5) \
             and ($6::date is null or p.posting_date < $6) and p.id <> $7",
            p = ferrum_kernel::ddl::q(&ent.table),
            w = ferrum_kernel::ddl::q(&lines.table),
        )))
        .bind(ctx.tenant)
        .bind(party)
        .bind(category)
        .bind(certificate)
        .bind(period.map(|p| p.0))
        .bind(period.map(|p| p.1))
        .bind(this)
        .fetch_one(&mut **tx)
        .await?;
        out.insert(*entity, [row.0, row.1, row.2, row.3]);
    }
    Ok(out)
}

fn total(k: &BTreeMap<&str, [Decimal; 4]>, i: usize) -> Decimal {
    k.values().map(|v| v[i]).sum()
}

fn row(cat: Uuid, certificate: Option<Uuid>, base: Decimal, advance_base: Decimal, taxable: Decimal, rate: Decimal, amount: Decimal) -> Doc {
    json!({"id": Uuid::now_v7().to_string(), "category": cat.to_string(), "certificate": certificate.map(|c| c.to_string()), "base": money(base),
           "advance_base": money(advance_base), "taxable": money(taxable), "rate": money(rate), "amount": money(amount)})
    .as_object()
    .cloned()
    .unwrap_or_default()
}

/// What is withheld on `base` (company currency) for a party in a category on a day: one row, or
/// two when a certificate's limit runs out part way (the certificate's part first). The document's
/// base and advance base go on the first.
#[allow(clippy::too_many_arguments)]
async fn withhold(
    hc: &mut HookCtx<'_>,
    c: &Category,
    party_field: &str,
    party: Uuid,
    on: NaiveDate,
    base: Decimal,
    advance_base: Decimal,
    this: Uuid,
    precision: u32,
) -> Result<Vec<Doc>> {
    // One party's documents in a category at a time, so two bills cannot both miss the threshold.
    sqlx::query("select pg_advisory_xact_lock(hashtext($1))").bind(format!("withholding:{}:{party}", c.id)).execute(&mut **hc.tx).await?;
    let from = ferrum_domain::reports::fy_start(on, hc.ctx.fy_start_month);
    let to = from.checked_add_months(Months::new(12)).unwrap_or(from);
    let year = kept(hc.tx, hc.ctx, party_field, party, Some(c.id), None, Some((from, to)), this).await?;
    let mut left = taxable(base, total(&year, 0), total(&year, 1), c.single, c.yearly, c.whole_year);
    let mut out = Vec::new();
    if left > Decimal::ZERO
        && let Some(cert) = certificate(hc, c.id, party_field, party, on).await?
    {
        let (cid, rate) = (id(&cert, "id").unwrap_or_default(), d(&cert, "rate"));
        let room = match cert.get("limit").filter(|v| !v.is_null()) {
            Some(_) => {
                let used = kept(hc.tx, hc.ctx, party_field, party, None, Some(cid), None, this).await?;
                (d(&cert, "limit") - total(&used, 1)).max(Decimal::ZERO)
            }
            None => left,
        };
        let part = room.min(left);
        if part > Decimal::ZERO {
            out.push(row(c.id, Some(cid), base, advance_base, part, rate, c.amount(part, rate, precision)));
            left -= part;
        }
    }
    if left > Decimal::ZERO || out.is_empty() {
        let (b_, a_) = if out.is_empty() { (base, advance_base) } else { (Decimal::ZERO, Decimal::ZERO) };
        out.push(row(c.id, None, b_, a_, left, c.rate, c.amount(left, c.rate, precision)));
    }
    Ok(out)
}

/// The party's certificate in the category valid on the day, if any.
async fn certificate(hc: &mut HookCtx<'_>, cat: Uuid, party_field: &str, party: Uuid, on: NaiveDate) -> Result<Option<Doc>> {
    use ferrum_kernel::{Filter, Op};
    let found = crate::util::find(
        hc.kernel,
        hc.ctx,
        hc.tx,
        "WithholdingCertificate",
        vec![
            Filter::eq("category", cat.to_string()),
            Filter::eq(party_field, party.to_string()),
            Filter { field: "valid_from".into(), op: Op::Lte, value: json!(on.to_string()) },
        ],
        Some("valid_from desc"),
        20,
    )
    .await?;
    Ok(found.into_iter().find(|c| s(c, "valid_to").is_none_or(|to| to >= on.to_string().as_str())))
}

/// A draft invoice's withholdings: On Bill categories on a purchase invoice, On Sale ones on a
/// sales invoice, each line in the invoice's category, else its item's, else the party's. A bill
/// is withheld only on what advances to the supplier in the category have not been withheld on
/// already. A return against an invoice takes back the same share of what the invoice withheld
/// as its net is of the invoice's. Sets `withholding_amount` in the invoice's currency.
pub(crate) async fn invoice(hc: &mut HookCtx<'_>, doc: &mut Doc, side: Side, co: &Company) -> Result<()> {
    if hc.entity.field("withholdings").is_none() || !d(doc, "docstatus").is_zero() {
        return Ok(());
    }
    let (party_field, party_entity, applies) = match side {
        Side::Buying => ("supplier", "Supplier", "On Bill"),
        Side::Selling => ("customer", "Customer", "On Sale"),
    };
    let Some(party) = id(doc, party_field) else { return Ok(()) };
    let this = id(doc, "id").unwrap_or_default();
    let on = date(doc, "posting_date")?;
    let mut out = Vec::new();
    if b(doc, "is_return") {
        if let Some(orig) = id(doc, "return_against") {
            let o = get(hc.kernel, hc.ctx, hc.tx, hc.entity.def.name.as_str(), orig).await?;
            let net = d(&o, "base_net_total");
            let ratio = if net.is_zero() { Decimal::ZERO } else { d(doc, "base_net_total") / net };
            for r in rows(&o, "withholdings") {
                let Some(cat) = id(&r, "category") else { continue };
                let unit = b(&get(hc.kernel, hc.ctx, hc.tx, "WithholdingCategory", cat).await?, "round_to_unit");
                let scaled = |f: &str| round(d(&r, f) * ratio, co.precision);
                let amount = rounded(d(&r, "amount") * ratio, unit, co.precision);
                out.push(row(cat, id(&r, "certificate"), scaled("base"), scaled("advance_base"), scaled("taxable"), d(&r, "rate"), amount));
            }
        }
    } else {
        let over = id(doc, "withholding_category");
        let party_cat = id(&get(hc.kernel, hc.ctx, hc.tx, party_entity, party).await?, "withholding_category");
        let mut bases: BTreeMap<Uuid, Decimal> = BTreeMap::new();
        for (line, base) in breakup(doc).lines {
            let item_cat = match (over, id(&line, "item")) {
                (None, Some(item)) => id(&get(hc.kernel, hc.ctx, hc.tx, "Item", item).await?, "withholding_category"),
                _ => None,
            };
            if let Some(cat) = over.or(item_cat).or(party_cat) {
                *bases.entry(cat).or_default() += base;
            }
        }
        for (cat, net) in bases {
            let Some(c) = category(hc, cat, on).await? else { continue };
            if c.applies != applies {
                continue;
            }
            let base = match d(doc, "base_net_total") {
                n if c.gross && !n.is_zero() => round(net * d(doc, "base_grand_total") / n, co.precision),
                _ => net,
            };
            if base <= Decimal::ZERO {
                continue;
            }
            let advance_base = if side == Side::Buying {
                let k = kept(hc.tx, hc.ctx, party_field, party, Some(cat), None, None, this).await?;
                (k.get("PaymentEntry").map_or(Decimal::ZERO, |v| v[3]) - k.get("PurchaseInvoice").map_or(Decimal::ZERO, |v| v[2])).max(Decimal::ZERO).min(base)
            } else {
                Decimal::ZERO
            };
            out.extend(withhold(hc, &c, party_field, party, on, base - advance_base, advance_base, this, co.precision).await?);
        }
    }
    finish(doc, out, d(doc, "conversion_rate"), co.precision)
}

/// Writes the rows and their total in the document's currency; returns nothing.
fn finish(doc: &mut Doc, out: Vec<Doc>, rate: Decimal, precision: u32) -> Result<()> {
    let rate = if rate.is_zero() { Decimal::ONE } else { rate };
    let total: Decimal = out.iter().map(|r| d(r, "amount")).sum();
    set(doc, "withholdings", Value::Array(out.into_iter().map(Value::Object).collect()));
    set(doc, "withholding_amount", money(round(total / rate, precision)));
    Ok(())
}

/// A draft payment to a supplier's withholdings, in its currency: on what it settles of each bill
/// in an On Payment category (the payment's, else the bill's, else the supplier's), in proportion
/// to the bill's net (or grand total) in what it settles; and on what it advances beyond its
/// allocations, in an On Bill or On Payment category (the payment's, else the supplier's), so a
/// later bill is withheld only on the rest. `settled` is what the money and deductions settle,
/// `allocated` what is allocated; returns what is withheld, which settles on top of the money.
pub(crate) async fn payment(hc: &mut HookCtx<'_>, doc: &mut Doc, co: &Company, rate: Decimal, settled: Decimal, allocated: Decimal) -> Result<Decimal> {
    if hc.entity.field("withholdings").is_none() || !d(doc, "docstatus").is_zero() {
        return Ok(d(doc, "withholding_amount"));
    }
    let (Some("Pay"), false, Some(supplier)) = (s(doc, "payment_type"), b(doc, "is_refund"), id(doc, "supplier")) else {
        set(doc, "withholdings", json!([]));
        set(doc, "withholding_amount", money(Decimal::ZERO));
        return Ok(Decimal::ZERO);
    };
    let this = id(doc, "id").unwrap_or_default();
    let on = date(doc, "posting_date")?;
    let over = id(doc, "withholding_category");
    let party_cat = id(&get(hc.kernel, hc.ctx, hc.tx, "Supplier", supplier).await?, "withholding_category");
    let mut out = Vec::new();
    for r in rows(doc, "references") {
        let (Some(inv), amount) = (id(&r, "invoice"), d(&r, "allocated_amount")) else { continue };
        let Some(item) = load_open_item(hc.tx, hc.ctx.tenant, inv, false).await? else { continue };
        if item.entity != "PurchaseInvoice" || amount <= Decimal::ZERO {
            continue;
        }
        let bill = get(hc.kernel, hc.ctx, hc.tx, "PurchaseInvoice", inv).await?;
        let Some(cat) = over.or(id(&bill, "withholding_category")).or(party_cat) else { continue };
        let Some(c) = category(hc, cat, on).await? else { continue };
        let whole = d(&bill, "base_rounded_total");
        if c.applies != "On Payment" || whole.is_zero() {
            continue;
        }
        let part = d(&bill, if c.gross { "base_grand_total" } else { "base_net_total" });
        let base = round(amount * rate * part / whole, co.precision);
        let mut rows_ = withhold(hc, &c, "supplier", supplier, on, base, Decimal::ZERO, this, co.precision).await?;
        for w in rows_.iter_mut() {
            set(w, "against", inv.to_string());
            set(w, "against_number", item.number.clone().map_or(Value::Null, Value::String));
        }
        out.extend(rows_);
    }
    let rate_ = if rate.is_zero() { Decimal::ONE } else { rate };
    let withheld: Decimal = out.iter().map(|r| d(r, "amount")).sum::<Decimal>() / rate_;
    let advanced = settled + withheld - allocated;
    if advanced > Decimal::ZERO
        && let Some(cat) = over.or(party_cat)
        && let Some(c) = category(hc, cat, on).await?
        && matches!(c.applies.as_str(), "On Bill" | "On Payment")
    {
        out.extend(withhold(hc, &c, "supplier", supplier, on, round(advanced * rate, co.precision), Decimal::ZERO, this, co.precision).await?);
    }
    finish(doc, out, rate, co.precision)?;
    Ok(d(doc, "withholding_amount"))
}

/// The postings of a submitted document's withholdings: each row's amount credited to its
/// category's account for the company (a return's negative amounts are debits).
pub(crate) async fn lines(hc: &mut HookCtx<'_>, doc: &Doc, co: &Company) -> Result<Vec<Line>> {
    let mut out = Vec::new();
    for r in rows(doc, "withholdings") {
        let amount = d(&r, "amount");
        let (false, Some(cat)) = (amount.is_zero(), id(&r, "category")) else { continue };
        let c = get(hc.kernel, hc.ctx, hc.tx, "WithholdingCategory", cat).await?;
        let mut account = rows(&c, "accounts").iter().find(|a| id(a, "company") == Some(co.id)).and_then(|a| id(a, "account"));
        // A company without a row of its own: its account with the category's account number (as a seeded category says).
        if account.is_none()
            && let Some(number) = s(&c, "account_number")
        {
            account = sqlx::query_scalar(
                "select id from e_account where tenant_id = $1 and company = $2 and account_number = $3 and not coalesce(is_group, false) limit 1",
            )
            .bind(hc.ctx.tenant)
            .bind(co.id)
            .bind(number)
            .fetch_optional(&mut **hc.tx)
            .await?;
        }
        let account = account.ok_or_else(|| {
            Error::field("withholdings", format!("the withholding category {} has no account for this company", s(&c, "category_name").unwrap_or("?")))
        })?;
        out.push(Line::new(account, -amount, &co.currency_code).remarks(format!("Withholding: {}", s(&c, "category_name").unwrap_or("?"))));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::taxable;
    use rust_decimal::Decimal;

    fn n(v: i64) -> Decimal {
        Decimal::from(v)
    }

    #[test]
    fn thresholds_and_the_base_once_crossed() {
        let (single, yearly) = (Some(n(3000)), Some(n(5000)));
        assert_eq!(taxable(n(800), n(0), n(0), single, yearly, true), n(0), "under both");
        assert_eq!(taxable(n(3500), n(0), n(0), single, yearly, false), n(3500), "one document above its threshold");
        assert_eq!(taxable(n(1000), n(4600), n(0), single, yearly, true), n(5600), "crossing the year: the whole year");
        assert_eq!(taxable(n(1000), n(4600), n(0), single, yearly, false), n(600), "crossing the year: above the threshold");
        assert_eq!(taxable(n(1000), n(5600), n(5600), single, yearly, true), n(1000), "after crossing: each document");
        assert_eq!(taxable(n(1000), n(5600), n(600), None, yearly, false), n(1000));
        assert_eq!(taxable(n(1000), n(0), n(0), None, None, true), n(1000), "no thresholds");
        assert_eq!(taxable(n(-5), n(0), n(0), None, None, true), n(0));
    }
}
