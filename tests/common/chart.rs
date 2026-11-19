//! A national chart of accounts for tests (erp-4): the standard chart of `company` renumbered with a 7 before each
//! number and renamed, each account standing for the standard one it was. Shared by ERP's and the add-ons' suites
//! (`#[path = "…/tests/common/chart.rs"] mod chart;`), so each proves its own accounts are found by role.

use ferrum_kernel::{Ctx, Kernel, ListQuery};
use serde_json::{Value, json};
use std::collections::HashMap;

/// Makes the template from `company`'s accounts, but for those standing for `leave_out` (made by an add-on, which
/// must make them again), and returns its id.
pub async fn national_template(k: &Kernel, ctx: &Ctx, company: &str, name: &str, leave_out: &[&str]) -> String {
    let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"company": company})).unwrap(), limit: Some(500), ..Default::default() };
    let accounts = k.list(ctx, "Account", &q).await.unwrap().rows;
    let number_of: HashMap<&str, &str> = accounts.iter().filter_map(|a| Some((a["id"].as_str()?, a["account_number"].as_str()?))).collect();
    let rows: Vec<Value> = accounts
        .iter()
        .filter_map(|a| {
            let number = a["account_number"].as_str().filter(|n| !leave_out.contains(n))?;
            let parent = a["parent_account"].as_str().and_then(|p| number_of.get(p)).map(|p| format!("7{p}"));
            Some(json!({"account_number": format!("7{number}"), "account_name": format!("National {}", a["account_name"].as_str().unwrap()),
                        "parent_number": parent, "is_group": a["is_group"], "root_type": a["root_type"], "account_type": a["account_type"],
                        "standard_number": number}))
        })
        .collect();
    let made = k.insert(ctx, "ChartTemplate", json!({"template_name": name, "accounts": rows}).as_object().cloned().unwrap()).await.unwrap();
    made["id"].as_str().unwrap().to_string()
}

/// The ledger lines `source` posted: (account id, debit minus credit).
pub async fn posted(k: &Kernel, ctx: &Ctx, source: &str) -> Vec<(String, rust_decimal::Decimal)> {
    let mut tx = k.db.tenant_tx(ctx.tenant).await.unwrap();
    let lines: Vec<(String, rust_decimal::Decimal)> = sqlx::query_as(
        "select l.account_id::text, l.debit - l.credit from journal_line l join journal_entry e on e.tenant_id = l.tenant_id and e.id = l.entry_id \
         where e.source_id = $1::uuid",
    )
    .bind(source)
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    lines
}

/// The id of `company`'s account standing for the standard account `number`.
pub async fn account(k: &Kernel, ctx: &Ctx, company: &str, number: &str) -> String {
    let q = ListQuery { filters: ferrum_kernel::query::parse_filters(&json!({"company": company, "standard_number": number})).unwrap(), ..Default::default() };
    let rows = k.list(ctx, "Account", &q).await.unwrap().rows;
    assert_eq!(rows.len(), 1, "one account stands for {number}");
    rows[0]["id"].as_str().unwrap().to_string()
}
