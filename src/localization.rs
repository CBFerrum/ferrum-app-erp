//! Where a country's rules plug into ERP (Ferrum docs/plan/erp-repo.md, "Every country as an
//! add-on"). The engine knows no country: a country's add-on (India's is `ferrum-india`) adds the
//! fields it needs through its package and registers a [`Localization`] here. Each runs only in
//! workspaces that have its package, and acts only for companies of its country, so one workspace
//! can keep companies in several countries.

use crate::pricing::Side;
use async_trait::async_trait;
use ferrum_kernel::{Ctx, Doc, HookCtx, Kernel, Result, Tx};
use ferrum_meta::EffEntity;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use uuid::Uuid;

#[async_trait]
pub trait Localization: Send + Sync {
    /// The package that switches it on in a workspace (India: `india`).
    fn package(&self) -> &'static str;

    /// Before a document's prices and taxes are calculated: what is taxed, how (India: the GST
    /// template for the place of supply). Runs inside [`crate::pricing::calculate`], so ERP's
    /// documents, CRM's quotations, POS's sales and Commerce's carts all get it.
    async fn determine(&self, _k: &Kernel, _ctx: &Ctx, _tx: &mut Tx, _ent: &EffEntity, _doc: &mut Doc, _side: Side) -> Result<()> {
        Ok(())
    }

    /// Before a priced document is submitted (India: HSN/SAC codes on outward supplies).
    async fn check(&self, _hc: &mut HookCtx<'_>, _doc: &Doc, _side: Side) -> Result<()> {
        Ok(())
    }

    /// After a new company's standard chart is made (`accounts`: its accounts' ids by name): the
    /// country's own accounts and tax templates.
    async fn setup_company(&self, _hc: &mut HookCtx<'_>, _company: Uuid, _accounts: &HashMap<&str, String>) -> Result<()> {
        Ok(())
    }
}

static ALL: RwLock<Vec<Arc<dyn Localization>>> = RwLock::new(Vec::new());

/// Registers a country's rules (at start-up). One per package: registering again replaces it.
pub fn add(l: Arc<dyn Localization>) {
    let mut all = ALL.write().unwrap_or_else(|e| e.into_inner());
    all.retain(|x| x.package() != l.package());
    all.push(l);
}

/// The countries whose packages the workspace has, in registration order.
fn installed(ctx: &Ctx) -> Vec<Arc<dyn Localization>> {
    let all = ALL.read().unwrap_or_else(|e| e.into_inner());
    all.iter().filter(|l| ctx.schema.packages.iter().any(|p| p == l.package())).cloned().collect()
}

/// Every installed country's [`Localization::determine`].
pub(crate) async fn determine(k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()> {
    for l in installed(ctx) {
        l.determine(k, ctx, tx, ent, doc, side).await?;
    }
    Ok(())
}

/// Every installed country's checks before a priced document is submitted. Apps that submit
/// priced documents of their own (CRM's quotations, POS's sales) call it in `before_submit`.
pub async fn check(hc: &mut HookCtx<'_>, doc: &Doc, side: Side) -> Result<()> {
    for l in installed(hc.ctx) {
        l.check(hc, doc, side).await?;
    }
    Ok(())
}

/// Every installed country's part of a new company's set-up.
pub(crate) async fn setup_company(hc: &mut HookCtx<'_>, company: Uuid, accounts: &HashMap<&str, String>) -> Result<()> {
    for l in installed(hc.ctx) {
        l.setup_company(hc, company, accounts).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Named(&'static str);

    #[async_trait]
    impl Localization for Named {
        fn package(&self) -> &'static str {
            self.0
        }
    }

    #[test]
    fn one_registration_per_package() {
        add(Arc::new(Named("zz_test_a")));
        add(Arc::new(Named("zz_test_b")));
        add(Arc::new(Named("zz_test_a")));
        let all = ALL.read().unwrap();
        let ours: Vec<&str> = all.iter().map(|l| l.package()).filter(|p| p.starts_with("zz_test_")).collect();
        assert_eq!(ours, ["zz_test_b", "zz_test_a"], "the second registration of a package replaces the first");
    }
}
