//! Finance: budgets with commitment control, bank accounts with statement import and
//! reconciliation (and the workbench's matching), period closing, payment terms and schedules,
//! dunning, subscriptions, the allocation of advance payments and of any unapplied money to a
//! party's open invoices, exchange revaluation at a period's end, and the overview of what needs
//! attention.
//!
//! Every posting goes through `ferrum_app_erp::ledger` (balanced entries, period locks,
//! append-only reversals, open items and allocations). Hooks on other packages' documents
//! (invoices, orders, payments, journals, any submitted document for budgets) do nothing for
//! tenants that did not install this package.

mod advances;
mod allocation;
mod bank;
mod budget;
mod closing;
mod common;
mod dunning;
pub mod payment_run;
mod reports;
mod revaluation;
mod rules;
mod statement;
mod subscription;
mod terms;

use ferrum_domain::reports::ReportSource;
use ferrum_kernel::{HookRegistry, Kernel, Result};
use std::sync::Arc;

include!(concat!(env!("OUT_DIR"), "/packages.rs"));

/// This app's manifest (`app.yaml`).
pub const MANIFEST: &str = include_str!("../app.yaml");

/// This app's metadata packages, embedded at build time.
pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {
    ferrum_meta::catalog::load_embedded(EMBEDDED)
}

/// Registers this app's document hooks (and web endpoints). Hooks on platform documents run
/// after the platform's own (registered first), so they see totals, postings and open items.
/// The supplier bank details Finance adds (`fin_iban`, `fin_bic`), erased with the supplier: the
/// platform's declarations cannot name a field another package's overlay adds, so the app does it in
/// the erasure's transaction, after the declared records are erased: the suppliers erased in it (by
/// their history's erasure events, as the email that found them is blank by then).
pub struct FinancePersonal;

#[async_trait::async_trait]
impl ferrum_kernel::privacy::PersonalHeld for FinancePersonal {
    fn app(&self) -> &'static str {
        "finance"
    }
    async fn report(
        &self,
        tx: &mut ferrum_kernel::Tx,
        tenant: uuid::Uuid,
        _: Option<uuid::Uuid>,
        emails: &[String],
    ) -> ferrum_kernel::Result<Vec<serde_json::Value>> {
        if !installed(tx).await? {
            return Ok(vec![]);
        }
        let held: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "select supplier_name, fin_iban, fin_bic from e_supplier where tenant_id = $1 and lower(email::text) = any($2) and (fin_iban is not null or fin_bic is not null)",
        )
        .bind(tenant)
        .bind(emails)
        .fetch_all(&mut **tx)
        .await?;
        Ok(held.into_iter().map(|(n, iban, bic)| serde_json::json!({"what": "Supplier bank details", "supplier": n, "iban": iban, "bic": bic})).collect())
    }
    async fn erase(&self, tx: &mut ferrum_kernel::Tx, tenant: uuid::Uuid, _: Option<uuid::Uuid>, _: &[String], _: &str) -> ferrum_kernel::Result<u64> {
        if !installed(tx).await? {
            return Ok(0);
        }
        let gone = sqlx::query(
            "update e_supplier s set fin_iban = null, fin_bic = null where s.tenant_id = $1 and (s.fin_iban is not null or s.fin_bic is not null) \
             and exists (select 1 from audit_event a where a.tenant_id = s.tenant_id and a.entity = 'Supplier' and a.record_id = s.id \
             and a.action = 'erase_personal_data' and a.at >= now())",
        )
        .bind(tenant)
        .execute(&mut **tx)
        .await?;
        Ok(gone.rows_affected())
    }
}

async fn installed(tx: &mut ferrum_kernel::Tx) -> ferrum_kernel::Result<bool> {
    Ok(sqlx::query_scalar("select exists (select 1 from information_schema.columns where table_name = 'e_supplier' and column_name = 'fin_iban')")
        .fetch_one(&mut **tx)
        .await?)
}

pub fn register(h: &mut HookRegistry) {
    h.add_personal_held(Arc::new(FinancePersonal));
    let schedule = Arc::new(terms::ScheduleHook);
    h.register("MonthlyDistribution", Arc::new(budget::DistributionHook))
        .register("Budget", Arc::new(budget::BudgetHook))
        .register("PurchaseOrder", Arc::new(budget::PurchaseOrderHook))
        .register("PurchaseInvoice", Arc::new(budget::PurchaseInvoiceDims))
        .register("MaterialRequest", Arc::new(budget::MaterialRequestHook))
        .register("*", Arc::new(budget::ActualExpenseCheck))
        .register("BankAccount", Arc::new(bank::BankAccountHook))
        .register("BankTransaction", Arc::new(bank::BankTransactionHook))
        .register("StatementFormat", Arc::new(statement::StatementFormatHook))
        .register("BankRule", Arc::new(bank::BankRuleHook))
        .register("PaymentEntry", Arc::new(bank::VoucherGuard))
        .register("JournalEntry", Arc::new(bank::VoucherGuard))
        .register("PeriodClosingVoucher", Arc::new(closing::ClosingHook))
        .register("PaymentTermsTemplate", Arc::new(terms::TemplateHook))
        .register("SalesInvoice", schedule.clone())
        .register("PurchaseInvoice", schedule.clone())
        .register("SalesOrder", schedule.clone())
        .register("PurchaseOrder", schedule)
        .register("SalesInvoice", Arc::new(advances::AdvanceHook))
        .register("PurchaseInvoice", Arc::new(advances::AdvanceHook))
        .register("PaymentEntry", Arc::new(advances::AdvancePaymentHook))
        .register("PaymentEntry", Arc::new(payment_run::DiscountHook))
        .register("PurchaseInvoice", Arc::new(payment_run::HoldHook))
        .register("PaymentRun", Arc::new(payment_run::RunHook))
        .register("PaymentAllocation", Arc::new(allocation::AllocationHook))
        .register("ExchangeRevaluation", Arc::new(revaluation::RevaluationHook))
        .register("SalesInvoice", Arc::new(allocation::AllocationGuard))
        .register("PurchaseInvoice", Arc::new(allocation::AllocationGuard))
        .register("PaymentEntry", Arc::new(allocation::AllocationGuard))
        .register("JournalEntry", Arc::new(allocation::AllocationGuard))
        .register("DunningType", Arc::new(dunning::DunningTypeHook))
        .register("Dunning", Arc::new(dunning::DunningHook))
        .register("SubscriptionPlan", Arc::new(subscription::PlanHook))
        .register("Subscription", Arc::new(subscription::SubscriptionHook));
}

/// This app's reports.
pub fn reports() -> Vec<Arc<dyn ReportSource>> {
    vec![Arc::new(reports::FinanceReports)]
}

/// One workspace's upkeep, which the platform runs about once a minute where Finance is installed, on any replica, and
/// may run again (docs/app-contract.md, Upkeep in the platform): subscriptions bill what is due (each period exactly
/// once); once a day, subscription statuses are refreshed and dunnings resolved and drafted; revaluations whose reversal
/// day has come are reversed. A failing step stops no other; the first failure is returned.
pub async fn maintenance(k: &Kernel, tenant: uuid::Uuid) -> Result<()> {
    let ctx = ferrum_kernel::Ctx::system(tenant, k.registry.schema(tenant).await?);
    let mut first = None;
    for (step, outcome) in [
        ("subscription", subscription::upkeep(k, &ctx).await),
        ("dunning", dunning::upkeep(k, &ctx).await),
        ("revaluation", revaluation::upkeep(k, &ctx).await),
    ] {
        if let Err(e) = outcome {
            tracing::warn!(%tenant, "{step} upkeep failed: {e}");
            first.get_or_insert(e);
        }
    }
    first.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    #[test]
    fn packages_validate_with_the_platform() {
        let mut all = ferrum_domain::packages().expect("platform packages");
        all.extend(ferrum_app_erp::packages().expect("ERP's packages"));
        let ours = super::packages().expect("finance packages parse");
        assert_eq!(ours.iter().map(|p| p.name()).collect::<Vec<_>>(), ["finance"]);
        all.extend(ours);
        ferrum_meta::Catalog::from_packages(all).expect("they validate together");
        assert!(super::MANIFEST.contains("id: finance"));
    }
}
