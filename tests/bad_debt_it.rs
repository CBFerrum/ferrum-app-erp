//! Bad-debt write-off (FN-13) against a real PostgreSQL (`FERRUM_ERP_BADDEBT_TEST_DATABASE_URL`,
//! default `postgres://ferrum:ferrum@localhost:5432/ferrum_erp_baddebt_test`; reset first): a
//! customer's invoice written off in one action, waiting for the approval the workspace set, booked to
//! its reason's account less the tax relief a country gives through the localization seam (a made-up
//! one here); a recovery later is income with the relief owed again in its share, never more than was
//! written off, and a write-off with a recovery is not cancelled.

mod common;
use common::*;
use ferrum_app_erp::localization::{Localization, Relief};
use ferrum_kernel::{Ctx, Doc, Kernel, Result, Tx};
use rust_decimal::Decimal;
use serde_json::json;
use uuid::Uuid;

/// A made-up country: the invoice's tax in the share written off is given back.
struct TaxBack;

#[async_trait::async_trait]
impl Localization for TaxBack {
    fn package(&self) -> &'static str {
        "accounting"
    }
    async fn bad_debt_relief(&self, _k: &Kernel, _ctx: &Ctx, _tx: &mut Tx, _company: Uuid, entity: &str, invoice: &Doc, base: Decimal) -> Result<Vec<Relief>> {
        if entity != "SalesInvoice" {
            return Ok(vec![]);
        }
        let taxes = invoice["taxes"].as_array().cloned().unwrap_or_default();
        let total = dec(&invoice["base_grand_total"]);
        Ok(taxes
            .iter()
            .map(|t| Relief {
                account: t["account"].as_str().unwrap().parse().unwrap(),
                amount: (base * dec(&t["base_tax_amount"]) / total).round_dp(2),
                description: "Bad-debt relief".into(),
            })
            .collect())
    }
}

#[test]
fn bad_debt() {
    run(scenario());
}

async fn scenario() {
    let url =
        std::env::var("FERRUM_ERP_BADDEBT_TEST_DATABASE_URL").unwrap_or_else(|_| "postgres://ferrum:ferrum@localhost:5432/ferrum_erp_baddebt_test".into());
    ferrum_app_erp::localization::add(std::sync::Arc::new(TaxBack));
    let t = T::boot(url, |_, _| {}).await;
    let c = t.company.clone();
    let output = t.account("Output Tax").await;
    let bank = t.account("Primary Bank").await;
    let debtors = t.account("Debtors").await;
    let other_income = t.account("Other Income").await;
    let expenses = t.find_one("Account", json!({"account_name": "Indirect Expenses", "company": c})).await["id"].clone();
    let bad =
        uid(&t.insert("Account", json!({"account_name": "Bad Debts", "company": c, "root_type": "Expense", "parent_account": expenses})).await).to_string();
    let reason = uid(&t
        .insert("WriteOffReason", json!({"reason_name": "Customer insolvent", "accounts": [{"company": c, "account": bad, "recovery_account": other_income}]}))
        .await)
    .to_string();
    let vat = uid(&t.insert("TaxTemplate", json!({"title": "VAT", "company": c, "kind": "Sales", "rows": [{"charge_type": "On Net Total", "account": output, "description": "VAT", "rate": "10"}]})).await).to_string();
    let products = t.find_one("ItemGroup", json!({"group_name": "Products"})).await;
    let nos = t.find_one("Uom", json!({"uom_name": "Nos"})).await;
    let advice = uid(&t
        .insert("Item", json!({"item_code": "Advice", "item_name": "Advice", "item_group": products["id"], "stock_uom": nos["id"], "standard_rate": "1000", "is_stock_item": false}))
        .await)
    .to_string();
    let globex = uid(&t.insert("Customer", json!({"customer_name": "Globex"})).await).to_string();
    let si = uid(&t.post("SalesInvoice", json!({"company": c, "customer": globex, "tax_template": vat, "items": [{"item": advice, "qty": "1"}]})).await);

    // The workspace wants a manager's approval for every write-off.
    let approver = t.k.create_user(&t.ctx, "manager@fin.test", "Manager", Some("Correct-Horse-9"), &["Accounts Manager".into()], false).await.expect("user");
    let op: ferrum_kernel::customize::NewCustomization = serde_json::from_value(
        json!({"entity": "BadDebtWriteOff", "op": {"op": "set_approval", "on": "submit", "approval": {"steps": [{"label": "Manager", "approvers": {"role": "Accounts Manager"}}]}}}),
    )
    .unwrap();
    t.k.batch_customizations(&t.ctx, vec![op], false).await.map_err(|(_, e)| e).expect("policy");
    let admin = t.k.load_ctx(t.ctx.tenant, t.ctx.user, None, "it".into()).await.unwrap();
    let approver = t.k.load_ctx(t.ctx.tenant, approver, None, "it".into()).await.unwrap();

    let w = t.insert("BadDebtWriteOff", json!({"company": c, "customer": globex, "invoice": si.to_string(), "reason": reason})).await;
    assert_eq!((dec(&w["amount"]), w["write_off_account"].clone()), (Decimal::from(1100), json!(bad)), "all that is outstanding, to the reason's account");
    let wid = uid(&w);
    assert_eq!(t.k.submit(&admin, "BadDebtWriteOff", wid, None).await.expect("asks")["docstatus"], json!(0), "waits for the manager");
    t.k.decide_approval(&approver, "BadDebtWriteOff", wid, true, Some("Insolvent")).await.expect("approve");
    let w = t.get("BadDebtWriteOff", wid).await;
    assert_eq!(w["docstatus"], json!(1));
    let posted: Vec<(Uuid, Decimal)> = t
        .sql(
            "select l.account_id, sum(l.debit - l.credit) from journal_line l join journal_entry e on e.id = l.entry_id where e.source_id = $1 group by 1",
            wid,
        )
        .await;
    let on = |a: &str| posted.iter().find(|(x, _)| x.to_string() == a).map(|p| p.1).unwrap_or_default();
    assert_eq!(
        (on(&debtors), on(&output), on(&bad)),
        (Decimal::from(-1100), Decimal::from(100), Decimal::from(1000)),
        "the tax given back, the rest an expense"
    );
    assert_eq!(dec(&t.get("SalesInvoice", si).await["outstanding_amount"]), Decimal::ZERO);

    // Half comes back later.
    let over = t.action("BadDebtWriteOff", wid, "record_recovery", json!({"amount": "1200", "bank_account": bank})).await.unwrap_err();
    assert!(over.contains("not yet recovered"), "{over}");
    let w = t.action("BadDebtWriteOff", wid, "record_recovery", json!({"amount": "550", "bank_account": bank})).await.expect("recovery");
    assert_eq!(dec(&w["recovered_amount"]), Decimal::from(550));
    let posted: Vec<(Uuid, Decimal)> = t
        .sql("select l.account_id, sum(l.debit - l.credit) from journal_line l join journal_entry e on e.id = l.entry_id where e.source_id = $1 and e.source_number like '%recovery%' group by 1", wid)
        .await;
    let on = |a: &str| posted.iter().find(|(x, _)| x.to_string() == a).map(|p| p.1).unwrap_or_default();
    assert_eq!((on(&bank), on(&output), on(&other_income)), (Decimal::from(550), Decimal::from(-50), Decimal::from(-500)), "the relief's share owed again");
    let cancel = t.k.cancel(&admin, "BadDebtWriteOff", wid, None).await.unwrap_err();
    assert!(format!("{cancel:?}").contains("recovered"), "{cancel:?}");
}
