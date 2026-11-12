//! A numbering series chosen per document. A document's usual number comes from its entity's template
//! (`SINV-{FY}-{seq:5}`). A `NumberSeries` is another template an administrator defines for one kind of
//! document (a tax invoice against a simplified receipt, a branch's own run), gapless or not as the series
//! says, and the document that takes it is numbered from it when it is submitted.
//!
//! Which series a document takes: the one it names (`number_series`, set by a person, an app or a rule),
//! else the one the localization seam answers by name ([`crate::localization::Localization::number_series`]),
//! else the series flagged to be used when a document names none (the company's, else every company's), else
//! none: the usual number, as before. The running number is the platform's own counter under the series'
//! rendered prefix and suffix: a gapless series takes it inside the submitting transaction, where the counter's
//! row stays locked until the document commits or rolls back, so numbers are neither skipped nor repeated
//! however many are submitted at once.

use crate::util::{b, find, get, id, s, set};
use async_trait::async_trait;
use ferrum_kernel::{Doc, DocHook, Error, Filter, HookCtx, Result};
use ferrum_meta::{NumberingDef, NumberingOn};
use serde_json::Value;

/// The series `doc` is numbered from, if any.
async fn series_of(hc: &mut HookCtx<'_>, doc: &Doc) -> Result<Option<Doc>> {
    let entity = hc.entity.def.name.clone();
    if let Some(named) = id(doc, "number_series") {
        return get(hc.kernel, hc.ctx, hc.tx, "NumberSeries", named).await.map(Some);
    }
    let company = id(doc, "company");
    let mut filters = vec![Filter::eq("document", entity.as_str()), Filter::eq("disabled", false)];
    let asked = crate::localization::number_series(hc, &entity, doc).await?;
    match &asked {
        Some(name) => filters.push(Filter::eq("series_name", name.as_str())),
        None => filters.push(Filter::eq("use_by_default", true)),
    }
    let found = find(hc.kernel, hc.ctx, hc.tx, "NumberSeries", filters, None, 50).await?;
    // The company's own before every company's.
    let own = found.iter().find(|x| company.is_some() && id(x, "company") == company);
    let general = found.iter().find(|x| id(x, "company").is_none());
    match own.or(general) {
        Some(x) => Ok(Some(x.clone())),
        None if asked.is_some() => Err(Error::field("number_series", "the series the company's country asks for is not set up for this kind of document")),
        None => Ok(None),
    }
}

/// Numbers documents from their series, and checks the one a document names.
pub struct SeriesHook;

#[async_trait]
impl DocHook for SeriesHook {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        // A workspace whose accounting package is older than this build has no series: numbers as before.
        if hc.ctx.schema.entity("NumberSeries").is_none() {
            return Ok(());
        }
        let Some(named) = id(doc, "number_series") else { return Ok(()) };
        let series = get(hc.kernel, hc.ctx, hc.tx, "NumberSeries", named).await?;
        if s(&series, "document") != Some(hc.entity.def.name.as_str()) {
            return Err(Error::field(
                "number_series",
                format!(
                    "{} is a series for {}, not {}",
                    s(&series, "series_name").unwrap_or_default(),
                    s(&series, "document").unwrap_or_default(),
                    hc.entity.def.label
                ),
            ));
        }
        if b(&series, "disabled") {
            return Err(Error::field("number_series", format!("{} is switched off", s(&series, "series_name").unwrap_or_default())));
        }
        if id(&series, "company").is_some_and(|c| Some(c) != id(doc, "company")) {
            return Err(Error::field("number_series", format!("{} belongs to another company", s(&series, "series_name").unwrap_or_default())));
        }
        Ok(())
    }

    async fn before_submit(&self, hc: &mut HookCtx<'_>, doc: &mut Doc) -> Result<()> {
        if s(doc, "number").is_some_and(|n| !n.is_empty()) || hc.ctx.schema.entity("NumberSeries").is_none() {
            return Ok(());
        }
        let Some(series) = series_of(hc, doc).await? else { return Ok(()) };
        let mut ent: ferrum_meta::EffEntity = ferrum_meta::EffEntity::clone(hc.entity);
        let date_field = ent.def.numbering.as_ref().and_then(|n| n.date_field.clone());
        ent.def.numbering = Some(NumberingDef {
            template: s(&series, "template").unwrap_or_default().to_string(),
            gapless: b(&series, "gapless"),
            on: NumberingOn::Submit,
            date_field,
        });
        let number = ferrum_kernel::numbering::assign(&hc.kernel.db, hc.ctx, hc.tx, &ent, doc).await?;
        set(doc, "number", Value::from(number));
        Ok(())
    }
}

/// Keeps a series' template one that can number, and one default per kind and company.
pub struct SeriesRecordHook;

#[async_trait]
impl DocHook for SeriesRecordHook {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &Doc) -> Result<()> {
        let template = s(doc, "template").unwrap_or_default();
        let def = NumberingDef { template: template.to_string(), gapless: false, on: NumberingOn::Submit, date_field: None };
        ferrum_kernel::numbering::render(&def, &Doc::new(), 1, hc.ctx.today()).map_err(|e| Error::field("template", e.to_string()))?;
        if b(doc, "use_by_default") && !b(doc, "disabled") {
            let mut filters =
                vec![Filter::eq("document", s(doc, "document").unwrap_or_default()), Filter::eq("use_by_default", true), Filter::eq("disabled", false)];
            if let Some(c) = id(doc, "company") {
                filters.push(Filter::eq("company", c.to_string()));
            }
            let others = find(hc.kernel, hc.ctx, hc.tx, "NumberSeries", filters, None, 5).await?;
            if others.iter().any(|o| id(o, "id") != id(doc, "id") && id(o, "company") == id(doc, "company")) {
                return Err(Error::field("use_by_default", "another series is already the one used when a document names none"));
            }
        }
        Ok(())
    }
}
