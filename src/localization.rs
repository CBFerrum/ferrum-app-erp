//! Where a country's rules plug into ERP (Ferrum docs/plan/erp-repo.md, "Every country as an
//! add-on"). The engine knows no country: a country's add-on (India's is `ferrum-india`) adds the
//! fields it needs through its package and registers a [`Localization`] here. Each runs only in
//! workspaces that have its package, and acts only for companies of its country, so one workspace
//! can keep companies in several countries.

use crate::pricing::Side;
use crate::util::{d, find, id, s, set};
use async_trait::async_trait;
use ferrum_kernel::{Ctx, Doc, Filter, HookCtx, Kernel, Op, Result, Tx};
use ferrum_meta::EffEntity;
use rust_decimal::Decimal;
use serde_json::{Value, json};
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

    /// After a document's lines and tax rows are known, before the calculation: each line's own
    /// rates for the tax rows (`rates[i]` for `lines[i]`, whose item is `items[i]`), as rows like
    /// an item's tax rates (`tax_account`, `rate`, `category`, `exemption_reason`). They hold the
    /// items' own when it is called, as earlier countries left them. The EU sets the rate of the
    /// country whose VAT applies.
    #[allow(clippy::too_many_arguments)]
    async fn line_rates(
        &self,
        _k: &Kernel,
        _ctx: &Ctx,
        _tx: &mut Tx,
        _doc: &Doc,
        _lines: &[Doc],
        _items: &[Doc],
        _taxes: &[Doc],
        _rates: &mut [Vec<Doc>],
        _side: Side,
    ) -> Result<()> {
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
/// Rates kept as data (Tax Rate records) for a country on a day: each category's latest one in
/// force (keyed in lower case), a region's over the whole country's. A localization prefers them
/// to its built-in rates, so a new rate is a record, not a release.
pub async fn rates(k: &Kernel, ctx: &Ctx, tx: &mut Tx, country: &str, region: Option<&str>, on: &str) -> Result<HashMap<String, Decimal>> {
    if ctx.schema.entity("TaxRate").is_none() {
        return Ok(HashMap::new());
    }
    let filters = vec![Filter::eq("country", country), Filter { field: "valid_from".into(), op: Op::Lte, value: json!(on) }];
    let mut best: HashMap<String, (bool, String, Decimal)> = HashMap::new();
    for r in find(k, ctx, tx, "TaxRate", filters, None, 500).await? {
        let (Some(category), Some(from)) = (s(&r, "category"), s(&r, "valid_from")) else { continue };
        if s(&r, "valid_to").is_some_and(|to| to < on) {
            continue;
        }
        let regional = match (s(&r, "region"), region) {
            (None, _) => false,
            (Some(a), Some(b)) if a.eq_ignore_ascii_case(b) => true,
            _ => continue,
        };
        let key = category.to_lowercase();
        if best.get(&key).is_none_or(|(g, f, _)| (regional, from) >= (*g, f.as_str())) {
            best.insert(key, (regional, from.to_string(), d(&r, "rate")));
        }
    }
    Ok(best.into_iter().map(|(c, (_, _, rate))| (c, rate)).collect())
}

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

/// Every installed country's [`Localization::line_rates`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn line_rates(
    k: &Kernel,
    ctx: &Ctx,
    tx: &mut Tx,
    doc: &Doc,
    lines: &[Doc],
    items: &[Doc],
    taxes: &[Doc],
    rates: &mut [Vec<Doc>],
    side: Side,
) -> Result<()> {
    for l in installed(ctx) {
        l.line_rates(k, ctx, tx, doc, lines, items, taxes, rates, side).await?;
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

/// Where a document's goods go, as countries' rules read it (`country_code`, `state`,
/// `postal_code`, `city` of an Address): its ship-to; else, without one, its delivery country and
/// region (a document can state them without an Address record: a cart, a counter sale), as an
/// address with only `country_code` and `state`; else the party's address on it.
pub async fn ship_to(k: &Kernel, ctx: &Ctx, tx: &mut Tx, doc: &Doc) -> Result<Option<Doc>> {
    if let Some(address) = id(doc, "ship_to") {
        return address_of(k, ctx, tx, address).await;
    }
    let (country, region) = (s(doc, "delivery_country"), s(doc, "delivery_region"));
    if country.is_some() || region.is_some() {
        let code = country.map(|c| ferrum_domain::countries::code_for(c).unwrap_or(c));
        return Ok(Some(json!({"country_code": code, "state": region}).as_object().cloned().unwrap_or_default()));
    }
    let Some(address) = ["customer_address", "supplier_address"].into_iter().find_map(|f| id(doc, f)) else { return Ok(None) };
    address_of(k, ctx, tx, address).await
}

/// A draft's delivery country and region: its ship-to's when it has one, so they always say where
/// the goods go; without one, the country as typed becomes its ISO code. Any document with the two
/// fields gets them, another app's cart as much as ERP's orders.
pub(crate) async fn fill_delivery(k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc) -> Result<()> {
    if ent.field("delivery_country").is_none() || doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 {
        return Ok(());
    }
    if let Some(address) = id(doc, "ship_to") {
        if let Some(a) = address_of(k, ctx, tx, address).await? {
            set(doc, "delivery_country", a.get("country_code").cloned().unwrap_or(Value::Null));
            set(doc, "delivery_region", a.get("state").cloned().unwrap_or(Value::Null));
        }
    } else if let Some(code) = s(doc, "delivery_country").and_then(|c| ferrum_domain::countries::code_for(c.trim())) {
        set(doc, "delivery_country", code);
    }
    Ok(())
}

async fn address_of(k: &Kernel, ctx: &Ctx, tx: &mut Tx, address: Uuid) -> Result<Option<Doc>> {
    Ok(find(k, ctx, tx, "Address", vec![Filter::eq("id", address.to_string())], None, 1).await?.into_iter().next())
}

/// A draft's addresses: the party's (customer's billing, supplier's) and, on sales, the ship-to.
/// One not chosen is the party's primary address of that type; one that belongs to another party
/// (the party changed) gives way to the new party's.
pub(crate) async fn fill_addresses(k: &Kernel, ctx: &Ctx, tx: &mut Tx, ent: &EffEntity, doc: &mut Doc, side: Side) -> Result<()> {
    if doc.get("docstatus").and_then(Value::as_i64).unwrap_or(0) != 0 {
        return Ok(());
    }
    let (party_field, slots): (&str, &[(&str, &str)]) = match side {
        Side::Selling => ("customer", &[("customer_address", "Billing"), ("ship_to", "Shipping")]),
        Side::Buying => ("supplier", &[("supplier_address", "Billing")]),
    };
    let Some(party) = id(doc, party_field) else { return Ok(()) };
    for (field, kind) in slots {
        if ent.field(field).is_none() {
            continue;
        }
        if let Some(chosen) = id(doc, field) {
            match address_of(k, ctx, tx, chosen).await? {
                // The party's, or no one's in particular (a site, a warehouse); one that does not
                // exist is the kernel's to report.
                Some(a) if id(&a, party_field).is_none_or(|p| p == party) => continue,
                None => continue,
                Some(_) => {}
            }
        }
        let primary = find(
            k,
            ctx,
            tx,
            "Address",
            vec![Filter::eq(party_field, party.to_string()), Filter::eq("address_type", *kind), Filter::eq("is_primary", true)],
            Some("created_at asc"),
            1,
        )
        .await?;
        set(doc, field, primary.first().and_then(|a| id(a, "id")).map_or(Value::Null, |a| json!(a.to_string())));
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
