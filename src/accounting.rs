//! Journal entries and payment entries. A journal entry may reverse itself, as an accrual or a
//! prepayment does: it is taken out again on its reversal day (the day after, unless another is
//! chosen), once, when the invoice or payment it stood in for is booked.

use crate::ledger::{self, Entry, Line, OpenItem};
use crate::util::{b, company, d, date, exchange_rate, get, id, money, round, rows, s, set, sys};
use async_trait::async_trait;
use chrono::Duration;
use ferrum_kernel::{Ctx, Doc, DocHook, Error, FieldError, HookCtx, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use uuid::Uuid;

pub struct JournalEntryHook;

#[async_trait]
impl DocHook for JournalEntryHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let mut lines = rows(doc, "accounts");
        // A party's line in their currency, or a bank or cash account's in the currency it is kept in (as a payment's
        // bank line is): its debit or credit is the amount in it at its rate. A line given in the company's currency on
        // an account kept in another is posted with that currency's amount at the day's rate, so every caller keeps
        // working and the account's own balance moves; an exchange revaluation's lines change the company's figure
        // alone. Where a line needs a rate and none is given, the day's is taken for the entry's day at each save and
        // shown as `day_rate`, never copied. Amounts finer than their currency are refused.
        let on = date(doc, "posting_date").ok();
        let revaluation = s(doc, "entry_type") == Some("Exchange Revaluation");
        let mut errors = Vec::new();
        for (i, l) in lines.iter_mut().enumerate() {
            let at = |f: &str, m: String| FieldError::new(f, m).in_row("accounts", i);
            set(l, "day_rate", Value::Null);
            set(l, "without_rate", false);
            let party = s(l, "party_type").is_some();
            let given = id(l, "currency").filter(|c| *c != co.currency_id);
            // What will be posted is checked: a line the journal works out (in another currency, or the balancing one)
            // is not checked before it is worked out.
            if given.is_none() && !b(l, "balances") {
                for f in ["debit", "credit"] {
                    if d(l, f).normalize().scale() > co.precision {
                        errors.push(at(f, format!("{} has more decimals than {} has ({})", d(l, f).normalize(), co.currency_code, co.precision)));
                    }
                }
            }
            // The account's own currency, when a line without a party is on one kept in another.
            let kept = match (party, id(l, "account")) {
                (false, Some(a)) => id(&get(hc.kernel, hc.ctx, hc.tx, "Account", a).await?, "account_currency").filter(|c| *c != co.currency_id),
                _ => None,
            };
            let Some(cur) = given.or(kept.filter(|_| !revaluation)) else { continue };
            let (code, precision) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, cur).await?;
            if given.is_some() && !party && kept != Some(cur) {
                errors.push(at(
                    "currency",
                    format!(
                        "only a customer's or supplier's line is carried in another currency, or a line on an account kept in it; book {code} to an account kept in {code}, or in the company's currency"
                    ),
                ));
                continue;
            }
            // A rate given on the line is used; else the day's. A line given in the company's currency with no rate
            // anywhere posts as before 0.4.1, its account's currency left out, and is marked (the accounts managers are
            // told when it is posted).
            let mut rate = d(l, "exchange_rate");
            if rate <= Decimal::ZERO && !party {
                let Some(on) = on else { continue };
                match exchange_rate(hc.kernel, hc.ctx, hc.tx, cur, co.currency_id, on).await {
                    Ok(r) if r > Decimal::ZERO => {
                        rate = r;
                        set(l, "day_rate", money(r));
                    }
                    _ if given.is_none() => {
                        set(l, "without_rate", true);
                        continue;
                    }
                    _ => {
                        errors.push(at("exchange_rate", format!("there is no rate from {code} to {} on {on}: give the rate", co.currency_code)));
                        continue;
                    }
                }
            }
            if rate <= Decimal::ZERO {
                errors.push(at("exchange_rate", format!("the rate {code} is carried at is needed")));
                continue;
            }
            if given.is_none() {
                // Less than the smallest amount of the account's currency at its rate: posted as before 0.4.1, marked.
                let amount = (d(l, "debit") - d(l, "credit")).abs();
                if !amount.is_zero() && ledger::round_as(amount / rate, precision, co.rounding).is_zero() {
                    set(l, "without_rate", true);
                }
                continue;
            }
            let (dr, cr) = (d(l, "debit_in_currency"), d(l, "credit_in_currency"));
            if let Some(f) =
                [("debit_in_currency", dr), ("credit_in_currency", cr)].into_iter().find(|(_, v)| v.normalize().scale() > precision).map(|(f, _)| f)
            {
                errors.push(at(f, format!("{} has more decimals than {code} has ({precision})", d(l, f).normalize())));
            } else if (dr > Decimal::ZERO) == (cr > Decimal::ZERO) {
                errors.push(at("debit_in_currency", format!("a debit or a credit in {code}, one of the two")));
            } else {
                set(l, "debit", money(ledger::round_as(dr * rate, co.precision, co.rounding)));
                set(l, "credit", money(ledger::round_as(cr * rate, co.precision, co.rounding)));
            }
        }
        // One line in the company's currency may take what balances the entry once the others are rounded, on the side
        // it is given (any amount there is replaced): a journal with lines in another currency then balances to the
        // last unit without its maker rounding anything.
        let balancing: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| b(l, "balances")).map(|(i, _)| i).collect();
        if let Some(&i) = balancing.get(1) {
            errors.push(FieldError::new("balances", "one line balances the entry").in_row("accounts", i));
        } else if let Some(&i) = balancing.first() {
            let at = |m: String| FieldError::new("balances", m).in_row("accounts", i);
            let (dr, cr) = (d(&lines[i], "debit") > Decimal::ZERO, d(&lines[i], "credit") > Decimal::ZERO);
            if id(&lines[i], "currency").is_some_and(|c| c != co.currency_id) {
                errors.push(at("the line that balances the entry is in the company's currency".into()));
            } else if dr == cr {
                errors.push(at("give the side the line that balances the entry is on: a debit or a credit, any amount".into()));
            } else {
                let others = |f: &str| lines.iter().enumerate().filter(|(j, _)| *j != i).map(|(_, l)| round(d(l, f), co.precision)).sum::<Decimal>();
                let figure = if dr { others("credit") - others("debit") } else { others("debit") - others("credit") };
                if figure <= Decimal::ZERO {
                    let side = if dr { "debit" } else { "credit" };
                    errors.push(at(format!("its {side} would be {figure}: the other lines leave nothing on that side to balance")));
                } else {
                    set(&mut lines[i], if dr { "debit" } else { "credit" }, money(figure));
                }
            }
        }
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        set(doc, "accounts", Value::Array(lines.iter().cloned().map(Value::Object).collect()));
        let dr: Decimal = lines.iter().map(|l| round(d(l, "debit"), co.precision)).sum();
        let cr: Decimal = lines.iter().map(|l| round(d(l, "credit"), co.precision)).sum();
        set(doc, "total_debit", money(dr));
        set(doc, "total_credit", money(cr));
        crate::localization::fill_voucher_registrations(hc.ctx, hc.entity, doc, &co.doc, None);
        Ok(())
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        if id(doc, "company").is_none() {
            return Err(Error::field("company", "is required"));
        }
        let mut errors = Vec::new();
        for (i, l) in rows(doc, "accounts").iter().enumerate() {
            let Some(acc) = id(l, "account") else { continue };
            let a = get(hc.kernel, hc.ctx, hc.tx, "Account", acc).await?;
            let typ = s(&a, "account_type").unwrap_or_default();
            // What customers and suppliers owe is the same in every book.
            if id(doc, "finance_book").is_some() && (s(l, "party_type").is_some() || matches!(typ, "Receivable" | "Payable")) {
                errors.push(
                    FieldError::new("account", "a finance book's entries adjust its valuations, not what customers and suppliers owe").in_row("accounts", i),
                );
                continue;
            }
            if let Some((field, m)) = party_problem(s(l, "party_type"), typ) {
                errors.push(FieldError::new(field, m).in_row("accounts", i));
            }
            if b(doc, "auto_reverse") && s(l, "party_type").is_some() {
                errors.push(
                    FieldError::new(
                        "party_type",
                        "an entry that reverses itself books to accounts: what a customer or supplier owes comes with their invoices",
                    )
                    .in_row("accounts", i),
                );
            }
        }
        if b(doc, "auto_reverse")
            && let (Ok(on), Ok(back)) = (date(doc, "posting_date"), date(doc, "reversal_date"))
            && back <= on
        {
            errors.push(FieldError::new("reversal_date", "must be after the day of the entry"));
        }
        if errors.is_empty() { Ok(()) } else { Err(Error::Validation(errors)) }
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let jid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let on = date(doc, "posting_date")?;
        let mut entry = Entry::new(co.id, on, "JournalEntry", jid, s(doc, "number").map(str::to_string));
        entry.memo = s(doc, "remark").map(str::to_string);
        entry.finance_book = id(doc, "finance_book");
        let mut lacking: Vec<String> = Vec::new();
        for l in rows(doc, "accounts") {
            let amount = d(&l, "debit") - d(&l, "credit");
            let line_id = id(&l, "id").ok_or_else(|| Error::internal("line without id"))?;
            let mut line = Line::new(id(&l, "account").unwrap_or_default(), amount, &co.currency_code)
                .customer(id(&l, "customer").filter(|_| s(&l, "party_type") == Some("Customer")))
                .supplier(id(&l, "supplier").filter(|_| s(&l, "party_type") == Some("Supplier")))
                .cost_center(id(&l, "cost_center"))
                .project(id(&l, "project"))
                .dims(crate::dimensions::of(l.get("dimensions")));
            if let Some(r) = s(&l, "remarks") {
                line = line.remarks(r);
            }
            if line.customer.is_some() || line.supplier.is_some() {
                line = line.against("JournalEntry", line_id);
                // In the party's currency when the line names one: the amount in it, at its rate.
                let (code, rate, txn) = match id(&l, "currency").filter(|c| *c != co.currency_id) {
                    Some(cur) => {
                        let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, cur).await?;
                        (code, d(&l, "exchange_rate"), d(&l, "debit_in_currency") - d(&l, "credit_in_currency"))
                    }
                    None => (co.currency_code.clone(), Decimal::ONE, amount),
                };
                line = line.txn(txn, &code, rate);
                // Party lines become open items so they show in ageing and can be settled: under
                // the number the party knows and on the day it falls due, when the line gives them.
                let sign = if line.customer.is_some() { Decimal::ONE } else { -Decimal::ONE };
                ledger::open_item(
                    hc.tx,
                    hc.ctx,
                    &OpenItem {
                        id: line_id,
                        company: co.id,
                        entity: "JournalEntry".into(),
                        number: s(&l, "reference_no").filter(|r| !r.trim().is_empty()).or(s(doc, "number")).map(str::to_string),
                        customer: line.customer,
                        supplier: line.supplier,
                        account: line.account,
                        posting_date: on,
                        due_date: Some(date(&l, "due_date").unwrap_or(on)),
                        currency: code,
                        exchange_rate: rate,
                        amount: sign * txn,
                        base_amount: sign * amount,
                    },
                )
                .await?;
            } else if let Some(cur) = id(&l, "currency").filter(|c| *c != co.currency_id) {
                // A bank or cash line in the currency its account is kept in (checked when it was saved), at its rate or
                // the day's.
                let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, cur).await?;
                let rate = Some(d(&l, "exchange_rate")).filter(|r| *r > Decimal::ZERO).unwrap_or_else(|| d(&l, "day_rate"));
                line = line.txn(d(&l, "debit_in_currency") - d(&l, "credit_in_currency"), &code, rate);
            } else if b(&l, "without_rate") {
                // Posted as before 0.4.1: no rate for its account's currency, or less than that currency's smallest amount.
                let a = get(hc.kernel, hc.ctx, hc.tx, "Account", line.account).await?;
                let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, id(&a, "account_currency").unwrap_or(co.currency_id)).await?;
                let rate = Some(d(&l, "exchange_rate")).filter(|r| *r > Decimal::ZERO).unwrap_or_else(|| d(&l, "day_rate"));
                let why = if rate > Decimal::ZERO {
                    format!("less than the smallest amount of {code} at {rate}")
                } else {
                    format!("no rate from {code} to {} on {on}", co.currency_code)
                };
                lacking.push(format!("{} {} on {} ({why})", amount.abs(), co.currency_code, s(&a, "account_name").unwrap_or_default()));
            } else if s(doc, "entry_type") != Some("Exchange Revaluation") && (d(&l, "exchange_rate") > Decimal::ZERO || d(&l, "day_rate") > Decimal::ZERO) {
                // Given in the company's currency on an account kept in another: that currency's amount at the rate given
                // on the line, else the day's.
                let a = get(hc.kernel, hc.ctx, hc.tx, "Account", line.account).await?;
                if let Some(cur) = id(&a, "account_currency").filter(|c| *c != co.currency_id) {
                    let (code, precision) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, cur).await?;
                    let rate = Some(d(&l, "exchange_rate")).filter(|r| *r > Decimal::ZERO).unwrap_or_else(|| d(&l, "day_rate"));
                    line = line.txn(ledger::round_as(amount / rate, precision, co.rounding), &code, rate);
                }
            }
            entry.lines.push(line);
        }
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
        if !lacking.is_empty() {
            let number = s(doc, "number").unwrap_or_default().to_string();
            let body = without_rate_notice(&number, &lacking);
            tell_accounts_managers(hc.tx, hc.ctx.tenant, Some(co.id), &format!("Journal {number} posted without a rate"), &body, "JournalEntry", Some(jid))
                .await?;
        }
        if b(doc, "auto_reverse") {
            reverse_from(hc, "JournalEntry", doc, on, co.precision).await?;
        }
        Ok(())
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let jid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let settled: bool = sqlx::query_scalar(
            "select exists(select 1 from allocation a where a.tenant_id = $1 and a.payment_entity = 'JournalEntry' and a.payment_id = $2 \
             and a.reversal_of is null and not exists (select 1 from allocation r where r.tenant_id = a.tenant_id and r.reversal_of = a.id))",
        )
        .bind(hc.ctx.tenant)
        .bind(jid)
        .fetch_one(&mut **hc.tx)
        .await?;
        if settled {
            return Err(Error::invalid(
                "this entry settled what was owed against it, so it cannot be cancelled: book a reversing journal (its lines the other way round) instead",
            ));
        }
        for l in rows(doc, "accounts") {
            if s(&l, "party_type").is_some()
                && let Some(lid) = id(&l, "id")
                && ledger::load_open_item(hc.tx, hc.ctx.tenant, lid, true).await?.is_some()
            {
                ledger::cancel_open_item(hc.tx, hc.ctx, lid).await?;
            }
        }
        // What was reversed on its day stays as it is: the two net to nothing.
        ledger::reverse(hc.tx, hc.ctx, "JournalEntry", jid, hc.ctx.today(), co.precision).await?;
        Ok(())
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, params: &Value) -> Result<Option<Value>> {
        if name != "settle_line" {
            return Ok(None);
        }
        settle_line(hc, doc, params).await.map(Some)
    }
}

/// What the accounts managers are told when `what` posted `lacking` (each "500 INR on USD Bank (why)") in the company's
/// currency on accounts kept in another, without that currency's amount. A journal in the company's currency on such an
/// account is itself converted; an Exchange Revaluation's line in the company's currency is not, so the advice names it.
pub(crate) fn without_rate_notice(what: &str, lacking: &[String]) -> String {
    format!(
        "{what} posted {} without the amount in the account's own currency, so that account's own balance lacks it. Add the rate where \
         there is none, then book a journal entry of type Exchange Revaluation with two lines on the account: the missing amount in its \
         currency, and the same value in the company's currency the other way round (an Exchange Revaluation leaves that line \
         unconverted), so that only the account's own balance moves.",
        lacking.join("; ")
    )
}

/// Tells `title` and `body`, linked to `entity` `record`, to the Accounts Managers of `company` as the platform counts
/// role holders (a current grant; an active member, not a service, support, portal or guest account), each kept to that
/// company or to none; to the workspace's administrators when there is none.
pub(crate) async fn tell_accounts_managers(
    tx: &mut Tx,
    tenant: Uuid,
    company: Option<Uuid>,
    title: &str,
    body: &str,
    entity: &str,
    record: Option<Uuid>,
) -> Result<()> {
    let mut people: Vec<Uuid> = sqlx::query_scalar(
        "select r.user_id from user_role r join app_user u on u.tenant_id = r.tenant_id and u.id = r.user_id \
         where r.tenant_id = $1 and r.role = 'Accounts Manager' and (r.expires_at is null or r.expires_at > now()) \
         and u.is_active and u.kind = 'member' \
         and (not exists (select 1 from user_scope c where c.tenant_id = r.tenant_id and c.user_id = r.user_id and c.entity = 'Company') \
              or exists (select 1 from user_scope c where c.tenant_id = r.tenant_id and c.user_id = r.user_id and c.entity = 'Company' and c.value = $2)) \
         order by 1",
    )
    .bind(tenant)
    .bind(company)
    .fetch_all(&mut **tx)
    .await?;
    if people.is_empty() {
        people = sqlx::query_scalar("select id from app_user where tenant_id = $1 and is_admin and is_active and kind = 'member' order by 1")
            .bind(tenant)
            .fetch_all(&mut **tx)
            .await?;
    }
    for user in people {
        ferrum_kernel::notify::send(tx, tenant, user, title, Some(body), Some(entity), record).await?;
    }
    Ok(())
}

/// Tells the accounts managers of `company` ([`tell_accounts_managers`]) that the upkeep could not do its work on
/// `record` of `entity`, and why, once: the upkeep runs about once a minute, and a round that fails the same way on the
/// record as it stands tells nobody. The reason and the record's version are kept (`upkeep_told`, a row a record,
/// written in the transaction that tells), so two rounds at once tell once, and people are told again only when the
/// reason changes or the record was changed since.
pub(crate) async fn tell_once(k: &Kernel, ctx: &Ctx, entity: &str, record: Uuid, company: Option<Uuid>, title: &str, reason: &str) -> Result<()> {
    let table = ctx.schema.entity(entity).map(|e| ferrum_kernel::ddl::q(&e.table)).ok_or_else(|| Error::internal(format!("no entity {entity}")))?;
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let changed: Option<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "insert into upkeep_told as t (tenant_id, entity, record_id, reason, version) \
         values ($1, $2, $3, $4, (select version from {table} where tenant_id = $1 and id = $3)) \
         on conflict (tenant_id, entity, record_id) do update set reason = excluded.reason, version = excluded.version, told_at = now() \
         where t.reason is distinct from excluded.reason or t.version is distinct from excluded.version returning 1"
    )))
    .bind(ctx.tenant)
    .bind(entity)
    .bind(record)
    .bind(reason)
    .fetch_optional(&mut *tx)
    .await?;
    if changed.is_some() {
        tell_accounts_managers(&mut tx, ctx.tenant, company, title, reason, entity, Some(record)).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// `settle_line`: a customer's or supplier's line of this submitted entry settled against another open item of the
/// same party, account and currency on the other side (an invoice, a credit note, a payment, another entry's line), as
/// far as both are open or `amount` (in the line's currency) says. Each side gives up its own value in the company's
/// currency. Both items are locked in id order and an allocation never takes more than is open (`ledger::allocate`),
/// so two settles at once take what is open once; the gate's key answers a repeat without settling again. Answers
/// what was settled and what is left open on the line.
async fn settle_line(hc: &mut HookCtx<'_>, doc: &Doc, params: &Value) -> Result<Value> {
    let jid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let p = params.as_object().cloned().unwrap_or_default();
    let line = id(&p, "line")
        .filter(|l| rows(doc, "accounts").iter().any(|r| id(r, "id") == Some(*l) && s(r, "party_type").is_some()))
        .ok_or_else(|| Error::field("line", "settle_line takes a customer's or supplier's line of this entry"))?;
    let against = id(&p, "against").ok_or_else(|| Error::field("against", "what it is settled against is not open"))?;
    // `amount` is absent (all that is open) or a positive amount of the line's currency at its precision; anything else
    // is refused, never read as "all".
    let row = rows(doc, "accounts").into_iter().find(|r| id(r, "id") == Some(line)).unwrap_or_default();
    let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
    let (code, precision) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, id(&row, "currency").unwrap_or(co.currency_id)).await?;
    let cap = match p.get("amount") {
        None | Some(Value::Null) => None,
        Some(Value::String(v)) if v.trim().is_empty() => None,
        Some(v) => {
            let text = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
            match text.trim().parse::<Decimal>() {
                Ok(a) if a > Decimal::ZERO && a.normalize().scale() <= precision => Some(a),
                _ => {
                    return Err(Error::field(
                        "amount",
                        format!(
                            "{text} is not an amount of {code} to settle: give one more than zero with at most {precision} decimals, or none for all that is open"
                        ),
                    ));
                }
            }
        }
    };
    let (first, second) = if line <= against { (line, against) } else { (against, line) };
    let a = ledger::load_open_item(hc.tx, hc.ctx.tenant, first, true).await?;
    let b = ledger::load_open_item(hc.tx, hc.ctx.tenant, second, true).await?;
    let (mine, other) = if first == line { (a, b) } else { (b, a) };
    let open = |o: Option<ledger::OpenItemRow>| o.filter(|o| o.status != "cancelled" && !o.outstanding.is_zero());
    let mine = open(mine).ok_or_else(|| Error::field("line", "this line is not open: nothing of it is left to settle"))?;
    let other = open(other).ok_or_else(|| Error::field("against", "what it is settled against is not open"))?;
    if (mine.customer_id, mine.supplier_id) != (other.customer_id, other.supplier_id) {
        return Err(Error::field("against", "what it is settled against is another customer's or supplier's"));
    }
    if mine.currency != other.currency {
        return Err(Error::field(
            "against",
            format!("what it is settled against is in another currency ({}; this line is in {})", other.currency, mine.currency),
        ));
    }
    // Accounts belong to one company: the same account is the same company, and each account's open items keep
    // agreeing with its balance.
    if mine.account_id != other.account_id {
        return Err(Error::field("against", "what it is settled against is on another account"));
    }
    if (mine.outstanding > Decimal::ZERO) == (other.outstanding > Decimal::ZERO) {
        return Err(Error::field("against", "what it is settled against is on the same side: settle what is owed against what is due"));
    }
    let mut x = mine.outstanding.abs().min(other.outstanding.abs());
    if let Some(cap) = cap {
        x = x.min(cap);
    }
    // Dated where the books can carry it: the later of the two items' days, or today when that is later; never in a
    // closed period, whether or not a difference is posted.
    let days: Vec<(chrono::NaiveDate,)> = sqlx::query_as("select posting_date from open_item where tenant_id = $1 and id = any($2)")
        .bind(hc.ctx.tenant)
        .bind(vec![mine.id, other.id])
        .fetch_all(&mut **hc.tx)
        .await?;
    let on = days.into_iter().map(|d| d.0).chain([hc.ctx.today()]).max().unwrap_or_else(|| hc.ctx.today());
    let period: Option<String> =
        sqlx::query_scalar("select status from accounting_period where tenant_id = $1 and company_id = $2 and $3 between period_start and period_end")
            .bind(hc.ctx.tenant)
            .bind(co.id)
            .bind(on)
            .fetch_optional(&mut **hc.tx)
            .await?;
    if period.is_none() {
        return Err(Error::invalid(format!("no accounting period holds {on}: make the year's periods first")));
    }
    if !ledger::period_open(hc.tx, hc.ctx.tenant, co.id, on).await? {
        return Err(Error::invalid(format!("the books are closed on {on}: settle it on a day of an open period")));
    }
    let (pos, neg) = if mine.outstanding > Decimal::ZERO { (&mine, &other) } else { (&other, &mine) };
    let at = |r: Decimal| ledger::round_as(x * r, co.precision, co.rounding);
    let given = ledger::allocate(hc.kernel, hc.tx, hc.ctx, co.id, pos.id, "JournalEntry", jid, on, x, at(pos.exchange_rate)).await?;
    let taken = -ledger::allocate(hc.kernel, hc.tx, hc.ctx, co.id, neg.id, "JournalEntry", jid, on, -x, -at(neg.exchange_rate)).await?;
    let diff = given - taken;
    if !diff.is_zero() {
        // Each side gave up its own value in the company's currency: what they differ by (two rates, or what each
        // document's rounding left) is an exchange difference, cleared from the party's account.
        let sign = if pos.customer_id.is_some() { -Decimal::ONE } else { Decimal::ONE };
        let gl = co.require("exchange_gain_loss_account", "exchange gain/loss account")?;
        let mut e = Entry::new(co.id, on, "JournalEntry", jid, s(doc, "number").map(str::to_string));
        e.memo = Some(format!("Exchange difference on settling {}", other.number.clone().unwrap_or_default()));
        e.lines.push(
            Line::new(pos.account_id, sign * diff, &co.currency_code)
                .txn(Decimal::ZERO, &pos.currency, Decimal::ONE)
                .customer(pos.customer_id)
                .supplier(pos.supplier_id)
                .against(&pos.entity, pos.id),
        );
        e.lines.push(Line::new(gl, -sign * diff, &co.currency_code).remarks("Exchange difference"));
        ledger::post(hc.tx, hc.ctx, e, co.precision).await?;
    }
    Ok(json!({"settled": money(x), "left": money(mine.outstanding.abs() - x)}))
}

/// On submitting a document that reverses itself (a journal entry, an exchange revaluation): its
/// reversal day is the one it names, else the day after its own, taken now rather than while it
/// was a draft, so a draft whose day changed does not keep a day worked out from the old one. When
/// that day has come, it is reversed at once.
pub async fn reverse_from(hc: &mut HookCtx<'_>, entity: &str, doc: &Doc, on: chrono::NaiveDate, precision: u32) -> Result<()> {
    let rid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let back = date(doc, "reversal_date").unwrap_or(on + Duration::days(1));
    let mut v = Doc::new();
    v.insert("reversal_date".into(), json!(back.to_string()));
    if back <= hc.ctx.today() {
        ledger::reverse_on(hc.tx, hc.ctx, entity, rid, back, precision).await?;
        v.insert("reversed_on".into(), json!(back.to_string()));
    }
    hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, entity, rid, v).await
}

/// Takes an entry that reverses itself out again on its reversal day, once: the entry records the day.
async fn reverse_posted(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc) -> Result<()> {
    let jid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let co = company(k, ctx, tx, id(doc, "company").unwrap_or_default()).await?;
    let on = date(doc, "reversal_date")?;
    ledger::reverse_on(tx, ctx, "JournalEntry", jid, on, co.precision).await?;
    let mut v = Doc::new();
    v.insert("reversed_on".into(), json!(on.to_string()));
    k.set_values_tx(&sys(ctx), tx, "JournalEntry", jid, v).await
}

/// The entries whose reversal day has come: each in a transaction of its own, so one that cannot
/// be reversed (its period closed) waits for the next round without holding up the others; its
/// company's accounts managers are told once ([`tell_once`]).
pub async fn upkeep(k: &Kernel, ctx: &Ctx) -> Result<()> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await?;
    let due: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "select id, company from e_journal_entry where tenant_id = $1 and docstatus = 1 and auto_reverse and reversed_on is null and reversal_date <= $2 \
         order by reversal_date, id",
    )
    .bind(ctx.tenant)
    .bind(ctx.today())
    .fetch_all(&mut *tx)
    .await?;
    tx.rollback().await?;
    for (jid, company) in due {
        let mut tx = k.db.tenant_tx(ctx.tenant).await?;
        let done = async {
            // Locked: run on two replicas at once, the second finds it reversed.
            let doc = k.get_tx(&sys(ctx), &mut tx, "JournalEntry", jid, true).await?;
            if s(&doc, "reversed_on").is_some() || doc.get("docstatus").and_then(Value::as_i64) != Some(1) {
                return Ok(());
            }
            reverse_posted(k, ctx, &mut tx, &doc).await
        }
        .await;
        match done {
            Ok(()) => tx.commit().await?,
            Err(e) => {
                tx.rollback().await?;
                tracing::warn!(tenant = %ctx.tenant, entry = %jid, "journal entry not reversed: {e}");
                tell_once(k, ctx, "JournalEntry", jid, company, "A journal entry was not reversed on its day", &e.to_string()).await?;
            }
        }
    }
    Ok(())
}

pub struct PaymentEntryHook;

/// Takes back what a submitted payment did, dated `on`: its allocations, its credit left open, its
/// ledger entries and its stored value. What was taken back already is not taken back twice, so a
/// payment whose instrument bounced (taken back on the bounce's day) is then cancelled as it stands.
pub async fn undo_payment(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc, on: chrono::NaiveDate) -> Result<()> {
    let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
    let co = company(k, ctx, tx, id(doc, "company").unwrap_or_default()).await?;
    ledger::unallocate(k, tx, ctx, "PaymentEntry", pid, on).await?;
    if ledger::load_open_item(tx, ctx.tenant, pid, true).await?.is_some_and(|i| i.status != "cancelled") {
        ledger::cancel_open_item(tx, ctx, pid).await?;
    }
    ledger::reverse(tx, ctx, "PaymentEntry", pid, on, co.precision).await?;
    crate::stored_value::reverse_entries(k, ctx, tx, "PaymentEntry", pid, on, "this payment").await?;
    Ok(())
}

/// What an open item is, for people: its entity's label ("Sales Invoice", "Journal Entry").
fn item_kind(ctx: &ferrum_kernel::Ctx, entity: &str) -> String {
    ctx.schema.entity(entity).map(|e| e.def.label.clone()).unwrap_or_else(|| entity.to_string())
}

/// The payment's party (customer or supplier) and whether money comes in.
fn party(doc: &Doc) -> (Option<Uuid>, Option<Uuid>, bool) {
    let receive = s(doc, "payment_type") != Some("Pay");
    if to_customer(doc) { (id(doc, "customer"), None, receive) } else { (None, id(doc, "supplier"), receive) }
}

/// A customer's when received, a supplier's when paid; the other way round for a refund.
fn to_customer(doc: &Doc) -> bool {
    (s(doc, "payment_type") != Some("Pay")) != refund(doc)
}

/// Whether the payment gives back a party's credit (Refund ticked): money paid to a customer, or
/// received from a supplier. It settles what the party is owed (credit notes, overpayments,
/// advances), whose open items are negative, and never more than that. Never inferred: a payment
/// without the tick is a customer's receipt or a payment to a supplier.
fn refund(doc: &Doc) -> bool {
    b(doc, "is_refund")
}

/// Money moved between two of the company's own bank and cash accounts: no party, nothing settled.
fn transfer(doc: &Doc) -> bool {
    s(doc, "payment_type") == Some("Internal Transfer")
}

/// A transfer's figures: each side in its account's currency (an account without one is in the
/// company's), the amount paid at the payment's rate and the amount received at the receiving
/// side's. Between two currencies, a rate not given is worked out from the two amounts (so the
/// company's figure is the same on both sides), or taken from the exchange rates when only the
/// amount paid is known; given both amounts and both rates, the sides may differ in the company's
/// currency, and the difference is an exchange gain or loss as on any payment.
async fn transfer_values(hc: &mut HookCtx<'_>, doc: &mut Doc, co: &crate::util::Company) -> Result<()> {
    for f in ["customer", "supplier", "party_account"] {
        set(doc, f, Value::Null);
    }
    set(doc, "is_refund", false);
    let on = date(doc, "posting_date")?;
    let mut names = Vec::new();
    let mut currencies = Vec::new();
    for f in ["bank_account", "paid_to"] {
        let cur = match id(doc, f) {
            Some(a) => {
                let a = get(hc.kernel, hc.ctx, hc.tx, "Account", a).await?;
                names.push(s(&a, "account_name").unwrap_or_default().to_string());
                id(&a, "account_currency").unwrap_or(co.currency_id)
            }
            None => id(doc, "currency").unwrap_or(co.currency_id),
        };
        currencies.push(cur);
    }
    let (from, to) = (currencies[0], currencies[1]);
    set(doc, "party_name", names.join(" → "));
    set(doc, "currency", from.to_string());
    set(doc, "received_currency", to.to_string());
    let given = |f: &str| Some(d(doc, f)).filter(|r| *r > Decimal::ZERO && *r != Decimal::ONE);
    let rate = match given("exchange_rate") {
        _ if from == co.currency_id => Decimal::ONE,
        Some(r) => r,
        None => exchange_rate(hc.kernel, hc.ctx, hc.tx, from, co.currency_id, on).await?,
    };
    let paid = d(doc, "paid_amount");
    let base_paid = ledger::round_as(paid * rate, co.precision, co.rounding);
    let (_, to_precision) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, to).await?;
    let received = d(doc, "received_amount");
    let (received, target, base_received) = if to == from {
        (paid, rate, base_paid)
    } else if to == co.currency_id {
        let r = if received > Decimal::ZERO { received } else { base_paid };
        (r, Decimal::ONE, r)
    } else {
        match given("target_exchange_rate") {
            Some(t) => {
                let r = if received > Decimal::ZERO { received } else { round(base_paid / t, to_precision) };
                (r, t, ledger::round_as(r * t, co.precision, co.rounding))
            }
            None if received > Decimal::ZERO => (received, (base_paid / received).round_dp(9), base_paid),
            None => {
                let t = exchange_rate(hc.kernel, hc.ctx, hc.tx, to, co.currency_id, on).await?;
                let r = round(base_paid / t, to_precision);
                (r, t, ledger::round_as(r * t, co.precision, co.rounding))
            }
        }
    };
    set(doc, "exchange_rate", money(rate));
    set(doc, "base_paid_amount", money(base_paid));
    set(doc, "received_amount", money(received));
    set(doc, "target_exchange_rate", money(target));
    set(doc, "base_received_amount", money(base_received));
    set(doc, "total_deductions", money(rows(doc, "deductions").iter().map(|r| d(r, "amount")).sum::<Decimal>()));
    set(doc, "unallocated_amount", money(Decimal::ZERO));
    Ok(())
}

/// A transfer's checks: two different Bank or Cash accounts of the company, something received,
/// and nothing a party's payment has (allocations, deductions).
async fn check_transfer(hc: &mut HookCtx<'_>, doc: &Doc, cid: Uuid) -> Result<()> {
    if !rows(doc, "references").is_empty() {
        return Err(Error::field("references", "a transfer between the company's own accounts settles no invoices: remove its allocations"));
    }
    if !rows(doc, "deductions").is_empty() {
        return Err(Error::field("deductions", "a transfer has no deductions: book its bank charges with a journal entry"));
    }
    for (f, which) in [("bank_account", "the money leaves"), ("paid_to", "the money goes to")] {
        let Some(acc) = id(doc, f) else { return Err(Error::field(f, format!("choose the account {which}"))) };
        let a = get(hc.kernel, hc.ctx, hc.tx, "Account", acc).await?;
        if id(&a, "company") != Some(cid) || !matches!(s(&a, "account_type"), Some("Bank" | "Cash")) {
            return Err(Error::field(
                f,
                format!("{} is not a Bank or Cash account of this company: a transfer moves money between two of them", s(&a, "account_name").unwrap_or("it")),
            ));
        }
    }
    if id(doc, "bank_account") == id(doc, "paid_to") {
        return Err(Error::field("paid_to", "a transfer moves money from one account to another: choose another account to send it to"));
    }
    if d(doc, "received_amount") <= Decimal::ZERO {
        return Err(Error::field("received_amount", "what reached the other account is needed"));
    }
    Ok(())
}

/// The early-payment discount on one invoice, `base` in company currency: discount allowed (a debit)
/// on money received, discount received (a credit) on money paid. When it reduces the tax, each of the
/// invoice's taxes takes back its share of the discount, in the proportion the invoice charged it
/// (reverse-charge and withheld rows are not charged to the party, so they take none).
async fn discount_lines(
    hc: &mut HookCtx<'_>,
    entity: &str,
    invoice: Uuid,
    base: Decimal,
    receive: bool,
    account: Uuid,
    reduces_tax: bool,
    co: &crate::util::Company,
    taken: &mut Vec<Value>,
) -> Result<Vec<Line>> {
    let sign = if receive { Decimal::ONE } else { -Decimal::ONE };
    let mut lines = Vec::new();
    let mut taxed = Decimal::ZERO;
    if reduces_tax && matches!(entity, "SalesInvoice" | "PurchaseInvoice") {
        let inv = get(hc.kernel, hc.ctx, hc.tx, entity, invoice).await?;
        let total = d(&inv, "base_grand_total");
        if !total.is_zero() {
            let breakdown = rows(&inv, "tax_breakdown");
            for t in rows(&inv, "taxes").iter().filter(|t| !b(t, "reverse_charge") && !b(t, "deduct")) {
                let (Some(acc), tax) = (id(t, "account"), d(t, "base_tax_amount")) else { continue };
                let share = round(base * tax / total, co.precision);
                if !share.is_zero() {
                    taxed += share;
                    lines.push(Line::new(acc, sign * share, &co.currency_code).remarks("Early-payment discount: tax taken back"));
                    // For the tax ledger: the tax taken back and the base it was on, as the invoice's breakdown has them for this account.
                    let mine: Vec<&Doc> = breakdown.iter().filter(|r| id(r, "account") == Some(acc)).collect();
                    let taxable: Decimal = mine.iter().map(|r| d(r, "base_taxable_amount")).sum();
                    let rate = d(t, "rate");
                    let category = s(t, "category")
                        .map(str::to_string)
                        .or_else(|| mine.first().and_then(|r| s(r, "category")).map(str::to_string))
                        .unwrap_or_else(|| if rate > Decimal::ZERO { "Standard".into() } else { "Zero Rated".into() });
                    taken.push(json!({"invoice_type": entity, "invoice": invoice, "account": acc, "description": s(t, "description"), "category": category,
                        "rate": money(rate), "taxable": money(round(base * taxable / total, co.precision)), "tax": money(share)}));
                }
            }
        }
    }
    lines.push(Line::new(account, sign * (base - taxed), &co.currency_code).remarks("Early-payment discount"));
    Ok(lines)
}

/// What a payment settles with its parties: the bank amount, plus deductions (debits) on a receipt
/// or minus them on a payment, so that 98 received with 2 of bank charges settles 100.
fn settled(doc: &Doc, receive: bool) -> Decimal {
    let deductions = d(doc, "total_deductions");
    d(doc, "paid_amount") + if receive { deductions } else { -deductions }
}

#[async_trait]
impl DocHook for PaymentEntryHook {
    async fn before_validate(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let Some(cid) = id(doc, "company") else { return Ok(()) };
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let named = match party(doc) {
            _ if transfer(doc) => None,
            (Some(c), _, _) => Some(("Customer", get(hc.kernel, &sys(hc.ctx), hc.tx, "Customer", c).await?)),
            (_, Some(sup), _) => Some(("Supplier", get(hc.kernel, &sys(hc.ctx), hc.tx, "Supplier", sup).await?)),
            _ => None,
        };
        crate::localization::fill_voucher_registrations(hc.ctx, hc.entity, doc, &co.doc, named.as_ref().map(|(e, p)| (*e, p)));
        if transfer(doc) {
            return transfer_values(hc, doc, &co).await;
        }
        let (customer, supplier, receive) = party(doc);
        let to_customer = to_customer(doc);
        // The other party is left over from before the type changed, unless nothing else is named:
        // then it is a refund not ticked, said in words rather than booked either way.
        let (other, other_field) = if to_customer { (id(doc, "supplier"), "supplier") } else { (id(doc, "customer"), "customer") };
        if other.is_some() && customer.is_none() && supplier.is_none() {
            let msg = match (to_customer, s(doc, "payment_type") == Some("Pay")) {
                (false, true) => "a payment to a customer gives back their credit: tick Refund, or choose the supplier paid",
                (true, false) => "money from a supplier gives back their credit: tick Refund, or choose the customer who paid",
                (false, false) => "a refund received comes from a supplier: choose the supplier, or untick Refund",
                (true, true) => "a refund paid goes to a customer: choose the customer, or untick Refund",
            };
            return Err(Error::field(other_field, msg));
        }
        set(doc, other_field, Value::Null);
        let party_doc = match (customer, supplier) {
            (Some(c), _) => Some(get(hc.kernel, hc.ctx, hc.tx, "Customer", c).await?),
            (_, Some(s_)) => Some(get(hc.kernel, hc.ctx, hc.tx, "Supplier", s_).await?),
            _ => None,
        };
        if let Some(p) = &party_doc {
            let name = s(p, if to_customer { "customer_name" } else { "supplier_name" }).unwrap_or_default().to_string();
            set(doc, "party_name", name);
            let acc = if to_customer {
                id(p, "receivable_account").or(co.account("default_receivable_account"))
            } else {
                id(p, "payable_account").or(co.account("default_payable_account"))
            };
            set(doc, "party_account", acc.map(|a| json!(a.to_string())).unwrap_or(Value::Null));
        }
        if id(doc, "currency").is_none() {
            set(doc, "currency", co.currency_id.to_string());
        }
        let cur = id(doc, "currency").unwrap_or(co.currency_id);
        let rate = if cur == co.currency_id {
            Decimal::ONE
        } else {
            let given = d(doc, "exchange_rate");
            if given > Decimal::ZERO && given != Decimal::ONE {
                given
            } else {
                exchange_rate(hc.kernel, hc.ctx, hc.tx, cur, co.currency_id, date(doc, "posting_date")?).await?
            }
        };
        set(doc, "exchange_rate", money(rate));
        set(doc, "base_paid_amount", money(ledger::round_as(d(doc, "paid_amount") * rate, co.precision, co.rounding)));
        let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, cur).await?;
        let mut refs = rows(doc, "references");
        let mut allocated = Decimal::ZERO;
        // A refund settles credits: what is shown is the credit left to give back.
        let shown = if refund(doc) { -Decimal::ONE } else { Decimal::ONE };
        for r in refs.iter_mut() {
            let Some(inv) = id(r, "invoice") else { continue };
            if let Some(item) = ledger::load_open_item(hc.tx, hc.ctx.tenant, inv, false).await? {
                set(r, "invoice_type", item_kind(hc.ctx, &item.entity));
                set(r, "invoice_number", item.number.clone().map(Value::String).unwrap_or(Value::Null));
                set(r, "outstanding", money(shown * item.outstanding));
                set(r, "due_date", item.due_date.map(|x| json!(x.to_string())).unwrap_or(Value::Null));
                if item.currency != code {
                    return Err(Error::Validation(vec![FieldError::new("invoice", format!("this invoice is in {}, the payment in {code}", item.currency))]));
                }
            }
            allocated += d(r, "allocated_amount");
        }
        set(doc, "references", Value::Array(refs.into_iter().map(Value::Object).collect()));
        let deductions: Decimal = rows(doc, "deductions").iter().map(|r| d(r, "amount")).sum();
        set(doc, "total_deductions", money(deductions));
        // Withheld from a supplier, it settles their bills on top of the money.
        let withheld = crate::withholding::payment(hc, doc, &co, rate, settled(doc, receive), allocated).await?;
        set(doc, "unallocated_amount", money(settled(doc, receive) + withheld - allocated));
        crate::stored_value::payment_fill(hc, doc).await
    }

    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        if transfer(doc) {
            return check_transfer(hc, doc, cid).await;
        }
        let currency = id(doc, "currency").ok_or_else(|| Error::field("currency", "is required"))?;
        let (code, places) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, currency).await?;
        // A payment is booked to its currency's places: a finer figure (99.995 rupees) would leave its
        // lines apart by a rounding nobody typed.
        let fine = |v: Decimal| (v != v.round_dp(places)).then(|| format!("{} has more decimal places than {code} is booked in ({places})", v.normalize()));
        let mut errors: Vec<FieldError> = fine(d(doc, "paid_amount")).map(|m| FieldError::new("paid_amount", m)).into_iter().collect();
        for (table, fields) in [("deductions", &["amount"][..]), ("references", &["allocated_amount", "discount_amount"][..])] {
            for (i, r) in rows(doc, table).iter().enumerate() {
                errors.extend(fields.iter().filter_map(|f| fine(d(r, f)).map(|m| FieldError::new(*f, m).in_row(table, i))));
            }
        }
        if !errors.is_empty() {
            return Err(Error::Validation(errors));
        }
        let over = -d(doc, "unallocated_amount");
        if over > Decimal::ZERO {
            let what = if d(doc, "total_deductions").is_zero() { "the paid amount" } else { "the paid amount and deductions" };
            return Err(Error::field("references", format!("allocations exceed {what} by {} {code}", over.normalize())));
        }
        if let Some(bank) = id(doc, "bank_account")
            && !crate::stored_value::pays(doc)
        {
            let a = get(hc.kernel, hc.ctx, hc.tx, "Account", bank).await?;
            if id(&a, "company") != Some(cid) || !matches!(s(&a, "account_type"), Some("Bank" | "Cash")) {
                return Err(Error::field("bank_account", "choose a Bank or Cash account of this company"));
            }
        }
        if id(doc, "party_account").is_none() {
            return Err(Error::field("party_account", "no receivable/payable account found for this party or company"));
        }
        for (i, r) in rows(doc, "deductions").iter().enumerate() {
            let bad = |field: &str, msg: &str| Err(Error::Validation(vec![FieldError::new(field, msg).in_row("deductions", i)]));
            if d(r, "amount").is_zero() {
                return bad("amount", "cannot be zero");
            }
            let Some(acc) = id(r, "account") else { return bad("account", "is required") };
            if Some(acc) == id(doc, "bank_account") || Some(acc) == id(doc, "party_account") {
                return bad("account", "use an account other than the bank and party accounts");
            }
            let a = get(hc.kernel, hc.ctx, hc.tx, "Account", acc).await?;
            if matches!(s(&a, "account_type"), Some("Receivable" | "Payable")) {
                return bad("account", "receivable and payable accounts need a party; allocate to an invoice instead");
            }
        }
        let (customer, supplier, _) = party(doc);
        let refund = refund(doc);
        if refund && d(doc, "unallocated_amount") > Decimal::ZERO {
            let whom = if customer.is_some() { "a customer" } else { "a supplier" };
            return Err(Error::field(
                "references",
                format!(
                    "a refund gives back {whom}'s credit: allocate all of it to their credit notes, advances or overpayments ({} is not allocated)",
                    d(doc, "unallocated_amount").normalize()
                ),
            ));
        }
        for (i, r) in rows(doc, "references").iter().enumerate() {
            let Some(inv) = id(r, "invoice") else {
                return Err(Error::Validation(vec![FieldError::new("invoice", "not a valid invoice id").in_row("references", i)]));
            };
            let Some(item) = ledger::load_open_item(hc.tx, hc.ctx.tenant, inv, false).await? else {
                return Err(Error::Validation(vec![FieldError::new("invoice", "invoice is not open for payment").in_row("references", i)]));
            };
            if item.customer_id != customer || item.supplier_id != supplier {
                return Err(Error::Validation(vec![FieldError::new("invoice", "invoice belongs to another party").in_row("references", i)]));
            }
            let what = item.number.clone().unwrap_or_else(|| item_kind(hc.ctx, &item.entity));
            if refund && item.outstanding >= Decimal::ZERO {
                return Err(Error::Validation(vec![
                    FieldError::new("invoice", format!("{what} is owed by the party, not a credit to give back")).in_row("references", i),
                ]));
            }
            if !refund && item.outstanding < Decimal::ZERO {
                return Err(Error::Validation(vec![
                    FieldError::new("invoice", format!("{what} is a credit of the party's: give it back with a refund (a payment the other way)"))
                        .in_row("references", i),
                ]));
            }
            let open = item.outstanding.abs();
            let discount = d(r, "discount_amount");
            if discount > Decimal::ZERO {
                let bad = |f: &str, m: String| Err(Error::Validation(vec![FieldError::new(f, m).in_row("references", i)]));
                if refund {
                    return bad("discount_amount", "a refund takes no discount".into());
                }
                let Some(acc) = id(r, "discount_account") else { return bad("discount_account", format!("say where the discount on {what} is booked")) };
                let a = get(hc.kernel, hc.ctx, hc.tx, "Account", acc).await?;
                if id(&a, "company") != id(doc, "company") || matches!(s(&a, "account_type"), Some("Receivable" | "Payable" | "Bank" | "Cash")) {
                    return bad("discount_account", "an income or expense account of the company".into());
                }
                if d(r, "allocated_amount") + discount > open {
                    return bad("discount_amount", format!("the allocation and the discount settle more than is outstanding on {what} ({})", open.normalize()));
                }
            }
            if d(r, "allocated_amount") > open {
                let msg =
                    if refund { format!("more than the credit left on {what} ({})", open.normalize()) } else { "more than the outstanding amount".to_string() };
                return Err(Error::Validation(vec![FieldError::new("allocated_amount", msg).in_row("references", i)]));
            }
        }
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        crate::instruments::check_payment(hc, doc, &co).await?;
        crate::stored_value::check_payment(hc, doc, &co).await
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        crate::instruments::fill(hc, doc, &co).await
    }

    async fn on_submit(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        let on = date(doc, "posting_date")?;
        let (customer, supplier, receive) = party(doc);
        let rate = d(doc, "exchange_rate");
        let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, id(doc, "currency").unwrap_or(co.currency_id)).await?;
        let paid = d(doc, "paid_amount");
        let base_paid = d(doc, "base_paid_amount");
        if transfer(doc) {
            let (from, to) = (id(doc, "bank_account").unwrap_or_default(), id(doc, "paid_to").ok_or_else(|| Error::field("paid_to", "is required"))?);
            let (to_code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, id(doc, "received_currency").unwrap_or(co.currency_id)).await?;
            let mut entry = Entry::new(co.id, on, "PaymentEntry", pid, s(doc, "number").map(str::to_string));
            entry.memo = s(doc, "remarks").map(str::to_string);
            entry.lines.push(Line::new(from, -base_paid, &co.currency_code).txn(-paid, &code, rate));
            let base_received = d(doc, "base_received_amount");
            entry.lines.push(Line::new(to, base_received, &co.currency_code).txn(d(doc, "received_amount"), &to_code, d(doc, "target_exchange_rate")));
            // The two sides at their own rates: what they differ by is an exchange gain or loss.
            let diff = base_received - base_paid;
            if !diff.is_zero() {
                let gl = co.require("exchange_gain_loss_account", "exchange gain/loss account")?;
                entry.lines.push(Line::new(gl, -diff, &co.currency_code).remarks("Exchange difference"));
                let mut v = Doc::new();
                v.insert("exchange_difference".into(), money(diff));
                hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "PaymentEntry", pid, v).await?;
            }
            let header = crate::dimensions::of(doc.get("dimensions"));
            for l in entry.lines.iter_mut() {
                l.dims = header.clone();
            }
            ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
            return Ok(());
        }
        let party_account = id(doc, "party_account").ok_or_else(|| Error::field("party_account", "is required"))?;
        // A refund takes from a credit (a negative item), so its allocations are negative.
        let item_sign = if refund(doc) { -Decimal::ONE } else { Decimal::ONE };
        let mut lines = Vec::new();
        let mut discount_taxes = Vec::new();
        for r in rows(doc, "references") {
            let discount = d(&r, "discount_amount").max(Decimal::ZERO);
            // What it settles: the money allocated and the discount taken.
            let amount = d(&r, "allocated_amount") + discount;
            if amount.is_zero() {
                continue;
            }
            let inv = id(&r, "invoice").ok_or_else(|| Error::internal("allocation without invoice"))?;
            let item = ledger::load_open_item(hc.tx, hc.ctx.tenant, inv, true).await?.ok_or_else(|| Error::invalid("invoice is no longer open"))?;
            if discount > Decimal::ZERO {
                let account = id(&r, "discount_account").ok_or_else(|| Error::field("discount_account", "is required"))?;
                let base = ledger::round_as(discount * rate, co.precision, co.rounding);
                lines.extend(discount_lines(hc, &item.entity, inv, base, receive, account, b(&r, "discount_reduces_tax"), &co, &mut discount_taxes).await?);
            }
            // What the invoice gives up at its own rate (all it has left, when this clears it); what the
            // money is worth at today's differs by the exchange difference below.
            let at_rate = ledger::round_as(amount * item.exchange_rate, co.precision, co.rounding);
            let base_at_invoice =
                item_sign * ledger::allocate(hc.kernel, hc.tx, hc.ctx, co.id, inv, "PaymentEntry", pid, on, item_sign * amount, item_sign * at_rate).await?;
            let sign = if receive { -Decimal::ONE } else { Decimal::ONE };
            lines.push(
                Line::new(party_account, sign * base_at_invoice, &co.currency_code)
                    .txn(sign * amount, &code, rate)
                    .customer(customer)
                    .supplier(supplier)
                    .against(&item.entity, inv),
            );
        }
        if !discount_taxes.is_empty() {
            // The tax taken back is in the tax ledger on the payment's day, as an invoice's tax is on its own.
            crate::tax_return::check_open(hc, doc).await?;
            let mut v = Doc::new();
            v.insert("discount_taxes".into(), Value::Array(discount_taxes));
            hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "PaymentEntry", pid, v).await?;
        }
        let unallocated = d(doc, "unallocated_amount");
        if unallocated > Decimal::ZERO {
            let base_un = ledger::round_as(unallocated * rate, co.precision, co.rounding);
            let sign = if receive { -Decimal::ONE } else { Decimal::ONE };
            lines.push(
                Line::new(party_account, sign * base_un, &co.currency_code)
                    .txn(sign * unallocated, &code, rate)
                    .customer(customer)
                    .supplier(supplier)
                    .against("PaymentEntry", pid),
            );
            ledger::open_item(
                hc.tx,
                hc.ctx,
                &OpenItem {
                    id: pid,
                    company: co.id,
                    entity: "PaymentEntry".into(),
                    number: s(doc, "number").map(str::to_string),
                    customer,
                    supplier,
                    account: party_account,
                    posting_date: on,
                    due_date: Some(on),
                    currency: code.clone(),
                    exchange_rate: rate,
                    amount: -unallocated,
                    base_amount: -base_un,
                },
            )
            .await?;
        }
        let bank = id(doc, "bank_account").ok_or_else(|| Error::field("bank_account", "is required"))?;
        // Paid by instrument: it waits in the instruments account until it clears into the bank.
        let bank = crate::instruments::holding_account(hc, doc, &co, receive).await?.unwrap_or(bank);
        let bank_sign = if receive { Decimal::ONE } else { -Decimal::ONE };
        lines.push(Line::new(bank, bank_sign * base_paid, &co.currency_code).txn(bank_sign * paid, &code, rate));
        for r in rows(doc, "deductions") {
            let (amount, Some(account)) = (d(&r, "amount"), id(&r, "account")) else { continue };
            lines.push(
                Line::new(account, ledger::round_as(amount * rate, co.precision, co.rounding), &co.currency_code)
                    .txn(amount, &code, rate)
                    .cost_center(id(&r, "cost_center"))
                    .dims(crate::dimensions::of(r.get("dimensions")))
                    .remarks(s(&r, "remarks").unwrap_or("Payment deduction")),
            );
        }
        lines.extend(crate::withholding::lines(hc, doc, &co).await?);
        // In another currency, what is left is the exchange difference between the invoices' booked
        // rates and today's, with the rounding of each figure converted at it: positive is a gain,
        // credited; negative a loss, debited. In the company's own currency nothing is converted, so
        // there is none: lines that do not balance are refused (`ledger::post`), not booked to it.
        let diff: Decimal = lines.iter().map(|l| l.amount).sum();
        if code != co.currency_code && !diff.is_zero() {
            let gl = co.require("exchange_gain_loss_account", "exchange gain/loss account")?;
            lines.push(Line::new(gl, -diff, &co.currency_code).remarks("Exchange difference"));
            let mut v = Doc::new();
            v.insert("exchange_difference".into(), money(diff));
            hc.kernel.set_values_tx(&sys(hc.ctx), hc.tx, "PaymentEntry", pid, v).await?;
        }
        let header = crate::dimensions::of(doc.get("dimensions"));
        for l in lines.iter_mut() {
            l.dims = crate::dimensions::over(&header, &l.dims);
        }
        let mut entry = Entry::new(co.id, on, "PaymentEntry", pid, s(doc, "number").map(str::to_string));
        entry.lines = lines;
        entry.memo = s(doc, "remarks").map(str::to_string);
        ledger::post(hc.tx, hc.ctx, entry, co.precision).await?;
        crate::instruments::payment_submitted(hc, doc, &co, receive).await?;
        crate::stored_value::payment_submitted(hc, doc, &co).await
    }

    async fn on_cancel(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let co = company(hc.kernel, hc.ctx, hc.tx, id(doc, "company").unwrap_or_default()).await?;
        if doc.get("discount_taxes").and_then(Value::as_array).is_some_and(|t| !t.is_empty()) {
            crate::tax_return::check_open(hc, doc).await?;
        }
        crate::instruments::payment_cancelled(hc, doc).await?;
        // Allocations, ledger and receivables all reverse on the same date, so as-of reports agree.
        let (on, _) = ledger::reversal_point(hc.tx, hc.ctx, co.id, doc).await?;
        undo_payment(hc.kernel, hc.ctx, hc.tx, doc, on).await
    }

    async fn action(&self, hc: &mut HookCtx<'_>, name: &str, doc: &Doc, _params: &Value) -> Result<Option<Value>> {
        if name != "get_outstanding_invoices" {
            return Ok(None);
        }
        if transfer(doc) {
            return Err(Error::invalid("a transfer between the company's own accounts settles no invoices"));
        }
        let pid = id(doc, "id").ok_or_else(|| Error::internal("no id"))?;
        let cid = id(doc, "company").ok_or_else(|| Error::field("company", "is required"))?;
        let (customer, supplier, receive) = party(doc);
        let co = company(hc.kernel, hc.ctx, hc.tx, cid).await?;
        let (code, _) = crate::util::currency(hc.kernel, hc.ctx, hc.tx, id(doc, "currency").unwrap_or(co.currency_id)).await?;
        // A refund is offered the party's credits (negative items), shown as the credit left.
        let sign = if refund(doc) { -Decimal::ONE } else { Decimal::ONE };
        let items: Vec<(Uuid, String, Option<String>, Decimal, Option<chrono::NaiveDate>)> = sqlx::query_as(
            "select id, entity, number, $6 * outstanding, due_date from open_item where tenant_id = $1 and company_id = $2 and status = 'open' \
             and $6 * outstanding > 0 and customer_id is not distinct from $3 and supplier_id is not distinct from $4 and trim(currency) = $5 \
             order by coalesce(due_date, posting_date), posting_date limit 500",
        )
        .bind(hc.ctx.tenant)
        .bind(cid)
        .bind(customer)
        .bind(supplier)
        .bind(&code)
        .bind(sign)
        .fetch_all(&mut **hc.tx)
        .await?;
        // Deductions settle invoices too, so they count towards what can be allocated.
        let mut budget = settled(doc, receive);
        let fill_all = d(doc, "paid_amount").is_zero();
        let mut refs = Vec::new();
        let mut total = Decimal::ZERO;
        for (inv, entity, number, outstanding, due) in items {
            let take = if fill_all { outstanding } else { outstanding.min(budget) };
            if take <= Decimal::ZERO {
                break;
            }
            budget -= take;
            total += take;
            let kind = item_kind(hc.ctx, &entity);
            refs.push(json!({"invoice_type": kind, "invoice": inv.to_string(), "invoice_number": number, "due_date": due, "outstanding": outstanding.to_string(), "allocated_amount": take.to_string()}));
        }
        let mut patch = Doc::new();
        patch.insert("references".into(), Value::Array(refs));
        if fill_all {
            let bank_amount = total - (settled(doc, receive) - d(doc, "paid_amount"));
            patch.insert("paid_amount".into(), json!(bank_amount.to_string()));
        }
        let updated = hc.kernel.update_tx(hc.ctx, hc.tx, "PaymentEntry", pid, patch, None).await?;
        Ok(Some(Value::Object(updated)))
    }
}

/// What is wrong with a ledger line's party for its account's type, as a journal entry checks it: the field and the
/// words. A customer's line is on a Receivable account and a supplier's on a Payable one; those two need a party.
pub(crate) fn party_problem(party_type: Option<&str>, account_type: &str) -> Option<(&'static str, &'static str)> {
    match (party_type, account_type) {
        (Some("Customer"), t) if t != "Receivable" => Some(("account", "a customer line must use a Receivable account")),
        (Some("Supplier"), t) if t != "Payable" => Some(("account", "a supplier line must use a Payable account")),
        (None, "Receivable" | "Payable") => Some(("party_type", "receivable/payable lines need a party")),
        _ => None,
    }
}
