//! Mexico: IVA (Ley del IVA; SAT) at 16%, 8% in the border regions for businesses the decree's
//! incentive authorises, 0% for exports and the goods the law lists (most foods, medicines,
//! agriculture), exempt acts (land, housing, medical services, education, interest); IVA on
//! services imported from abroad, which the company pays itself and credits. IVA is owed when it
//! is collected and credited when it is paid (cash basis, articles 1-B and 5 of the law), so the
//! return works from payments: each one's share of its invoices' IVA.

use crate::engine::{Country, EXEMPT, EXPORT, REVERSE_CHARGED, Rate, ReverseCharge, Row, ZERO_RATED};
use crate::util::money;
use async_trait::async_trait;
use ferrum_domain::reports::{Params, ReportSource, col, param_def, period, read_tx, resolve_company};
use ferrum_kernel::{Ctx, Error, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use uuid::Uuid;

pub static COUNTRY: Country = Country {
    package: "mx",
    country: "MX",
    rows: &[Row { label: "IVA", sales: "2131", purchases: Some("1151"), self_assessed: None }],
    rates: &[
        Rate { category: "General 16%", row: 0, from: "2010-01-01", rate: "16" },
        Rate { category: "Border Region 8%", row: 0, from: "2019-01-01", rate: "8" },
    ],
    category_field: "mx_iva_category",
    default_category: "General 16%",
    kind_field: "mx_kind",
    number_field: "mx_rfc",
    number,
    number_hint: "an RFC: 12 characters for a company (3 letters), 13 for a person (4 letters), then 6 digits and 3 characters",
    export_reason: "Exportación: IVA tasa 0%",
    exempt_reason: "Acto exento de IVA",
    reverse_charge: ReverseCharge::Always,
    simplified: None,
    accounts: &[],
};

pub fn number(raw: &str) -> Option<String> {
    let n: String = raw.chars().filter(|c| !matches!(c, ' ' | '-')).collect::<String>().to_uppercase();
    let letters = n.chars().count().checked_sub(9)?;
    if !(3..=4).contains(&letters) {
        return None;
    }
    let c: Vec<char> = n.chars().collect();
    let ok = c[..letters].iter().all(|x| x.is_ascii_alphabetic() || matches!(x, 'Ñ' | '&'))
        && c[letters..letters + 6].iter().all(char::is_ascii_digit)
        && c[letters + 6..].iter().all(char::is_ascii_alphanumeric);
    ok.then_some(n)
}

/// IVA on a cash basis: what payments in a period collected and paid, by category.
pub struct CashReturn;

/// Each category's (amount, IVA) over the payments of a side in a period, in proportion to what
/// each payment settles of its invoices.
async fn settled(
    tx: &mut Tx,
    ctx: &Ctx,
    company: Uuid,
    kind: &str,
    accounts: &[Uuid],
    from: chrono::NaiveDate,
    to: chrono::NaiveDate,
) -> Result<BTreeMap<String, (Decimal, Decimal)>> {
    let (entity, payment_type) = if kind == "Sales Invoice" { ("SalesInvoice", "Receive") } else { ("PurchaseInvoice", "Pay") };
    let invoices = ferrum_meta::table_for(entity);
    let breakdown = ferrum_kernel::ddl::q(&ferrum_meta::child_table(&invoices, "tax_breakdown"));
    let refs = ferrum_kernel::ddl::q(&ferrum_meta::child_table(&ferrum_meta::table_for("PaymentEntry"), "references"));
    let invoices = ferrum_kernel::ddl::q(&invoices);
    let rows: Vec<(Option<String>, bool, Option<Decimal>, Option<Decimal>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "select b.category, coalesce(b.reverse_charge, false), \
                sum(b.base_taxable_amount * r.allocated_amount / nullif(i.grand_total, 0)), sum(b.base_tax_amount * r.allocated_amount / nullif(i.grand_total, 0)) \
         from e_payment_entry p join {refs} r on r.tenant_id = p.tenant_id and r.parent_id = p.id \
         join {invoices} i on i.tenant_id = r.tenant_id and i.id::text = r.invoice \
         join {breakdown} b on b.tenant_id = i.tenant_id and b.parent_id = i.id \
         where p.tenant_id = $1 and p.company = $2 and p.docstatus = 1 and p.payment_type = $3 and p.posting_date between $4 and $5 \
         and r.invoice_type = $6 and b.account = any($7) group by 1, 2"
    )))
    .bind(ctx.tenant)
    .bind(company)
    .bind(payment_type)
    .bind(from)
    .bind(to)
    .bind(kind)
    .bind(accounts)
    .fetch_all(&mut **tx)
    .await?;
    let mut out: BTreeMap<String, (Decimal, Decimal)> = BTreeMap::new();
    for (category, reverse, net, tax) in rows {
        let e = out.entry(if reverse { REVERSE_CHARGED.to_string() } else { category.unwrap_or_default() }).or_default();
        e.0 += net.unwrap_or_default().round_dp(2);
        e.1 += tax.unwrap_or_default().round_dp(2);
    }
    Ok(out)
}

#[async_trait]
impl ReportSource for CashReturn {
    fn catalog(&self) -> Vec<Value> {
        let params: Vec<Value> = ["company", "from_date", "to_date"].iter().map(|p| param_def(p)).collect();
        vec![
            json!({"name": "mx_iva", "label": "Declaración de IVA (cash basis)", "module": "accounting", "entity": "SalesInvoice", "scoped": false, "params": params}),
        ]
    }

    async fn run(&self, k: &Kernel, ctx: &Ctx, name: &str, raw: &HashMap<String, String>) -> Result<Value> {
        if name != "mx_iva" {
            return Err(Error::NotFound(format!("report {name}")));
        }
        let p = Params { raw };
        let mut tx = read_tx(k, ctx).await?;
        let co = resolve_company(&mut tx, ctx, &p).await?;
        let (from, to) = period(&p, &co)?;
        let accounts: Vec<Uuid> = sqlx::query_scalar("select id from e_account where tenant_id = $1 and company = $2 and account_number = any($3)")
            .bind(ctx.tenant)
            .bind(co.id)
            .bind(vec!["2131".to_string(), "1151".to_string()])
            .fetch_all(&mut *tx)
            .await?;
        let sold = settled(&mut tx, ctx, co.id, "Sales Invoice", &accounts, from, to).await?;
        let bought = settled(&mut tx, ctx, co.id, "Purchase Invoice", &accounts, from, to).await?;
        tx.rollback().await?;
        let get = |m: &BTreeMap<String, (Decimal, Decimal)>, cats: &[&str]| -> (Decimal, Decimal) {
            cats.iter().filter_map(|c| m.get(*c)).fold((Decimal::ZERO, Decimal::ZERO), |a, v| (a.0 + v.0, a.1 + v.1))
        };
        let (g16, iva16) = get(&sold, &["General 16%"]);
        let (g8, iva8) = get(&sold, &["Border Region 8%"]);
        let (zero, _) = get(&sold, &[ZERO_RATED, EXPORT]);
        let (exempt, _) = get(&sold, &[EXEMPT]);
        let (_, reverse) = get(&bought, &[REVERSE_CHARGED]);
        let (_, credit) = get(&bought, &["General 16%", "Border Region 8%", REVERSE_CHARGED]);
        let trasladado = iva16 + iva8 + reverse;
        let rows = vec![
            json!({"box": "G16", "label": "Actos gravados a la tasa del 16% (cobrados)", "amount": money(g16)}),
            json!({"box": "G8", "label": "Actos gravados a la tasa del 8%, región fronteriza (cobrados)", "amount": money(g8)}),
            json!({"box": "G0", "label": "Actos gravados a la tasa del 0% (cobrados)", "amount": money(zero)}),
            json!({"box": "EX", "label": "Actos exentos (cobrados)", "amount": money(exempt)}),
            json!({"box": "IVA-T", "label": "IVA trasladado cobrado, e IVA por servicios importados", "amount": money(trasladado)}),
            json!({"box": "IVA-A", "label": "IVA acreditable pagado", "amount": money(credit)}),
            json!({"box": "IVA", "label": "IVA a cargo (a favor cuando es negativo)", "amount": money(trasladado - credit)}),
        ];
        Ok(json!({"name": name, "label": "Declaración de IVA (cash basis)", "rows": rows, "meta": {"from_date": from, "to_date": to},
                  "columns": [col("box", "Box", "data"), col("label", "", "data"), col("amount", "Amount", "currency")]}))
    }
}

fn reports_list() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {
    vec![std::sync::Arc::new(CashReturn)]
}

crate::app!("mx");

#[cfg(test)]
mod tests {
    #[test]
    fn numbers() {
        assert_eq!(super::number("abc-680524-p76"), Some("ABC680524P76".into()), "a company");
        assert_eq!(super::number("GODE561231GR8"), Some("GODE561231GR8".into()), "a person");
        assert_eq!((super::number("AB680524P76"), super::number("ABC68052AP76")), (None, None));
    }
}
