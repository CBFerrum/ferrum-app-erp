//! Accounting dimensions (FN-01): what a workspace analyses its books by beyond cost centre and
//! project (a department, a branch, a product line). Each is an `AccountingDimension` record: a code,
//! its values (a list, or the records of an entity), the accounts that need it and its defaults. A
//! ledger line keeps its values in `journal_line.dims` as `{code: value}`; a line written before a
//! dimension existed has `{}`, which reads as "none". Cost centre and project stay the columns they
//! are and answer to the codes `cost_center` and `project` in reports.
//!
//! Every posting goes through [`prepare`] (from `ledger::post`): a line without a value takes the
//! first matching default, a value that is not one of the dimension's is refused, and so is a line
//! on an account that requires a dimension and has none. Reports filter with `dims @> {code: value}`,
//! which the partial GIN index `journal_line_dims` (jsonb_path_ops, rows with any dimension) serves.

use crate::ledger::Line;
use ferrum_kernel::{Ctx, DocHook, Error, HookCtx, Result, Tx};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use uuid::Uuid;

/// A line's dimension values by code.
pub type Dims = BTreeMap<String, String>;

/// The codes cost centre and project answer to.
pub const BUILT_IN: [&str; 2] = ["cost_center", "project"];

/// The values of a `{code: value}` object (a document's or line's `dimensions`, a ledger line's
/// `dims`); empty values are none.
pub fn of(v: Option<&Value>) -> Dims {
    let Some(Value::Object(m)) = v else { return Dims::new() };
    m.iter()
        .filter_map(|(k, v)| {
            let text = match v {
                Value::String(s) => s.trim().to_string(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            (!text.is_empty()).then(|| (k.clone(), text))
        })
        .collect()
}

/// A line's dimensions over its document's: the line's own value wins.
pub fn over(header: &Dims, line: &Dims) -> Dims {
    let mut d = header.clone();
    d.extend(line.iter().map(|(k, v)| (k.clone(), v.clone())));
    d
}

/// A document's `dimensions` with its line's over them.
pub fn of_line(doc: &ferrum_kernel::Doc, line: &ferrum_kernel::Doc) -> Dims {
    over(&of(doc.get("dimensions")), &of(line.get("dimensions")))
}

#[derive(Default)]
struct Rule {
    company: Option<Uuid>,
    cost_center: Option<Uuid>,
    project: Option<Uuid>,
    customer: Option<Uuid>,
    supplier: Option<Uuid>,
    item: Option<Uuid>,
    value: String,
}

impl Rule {
    fn matches(&self, company: Uuid, l: &Line) -> bool {
        let fits = |want: Option<Uuid>, have: Option<Uuid>| want.is_none() || want == have;
        fits(self.company, Some(company))
            && fits(self.cost_center, l.cost_center)
            && fits(self.project, l.project)
            && fits(self.customer, l.customer)
            && fits(self.supplier, l.supplier)
            && fits(self.item, l.item)
    }
}

pub struct Def {
    pub code: String,
    pub name: String,
    /// The list of values; `None` when they are records of `entity`.
    pub values: Option<BTreeSet<String>>,
    pub entity: Option<String>,
    disabled: bool,
    accounts: BTreeSet<Uuid>,
    kinds: BTreeSet<String>,
    defaults: Vec<Rule>,
}

/// The workspace's dimensions, in code order.
pub async fn load(tx: &mut Tx, ctx: &Ctx) -> Result<Vec<Def>> {
    if ctx.schema.entity("AccountingDimension").is_none() {
        return Ok(vec![]);
    }
    let heads: Vec<(Uuid, String, String, Option<String>, Option<String>, Option<bool>)> = sqlx::query_as(
        "select id, code, dimension_name, source, source_entity, disabled from e_accounting_dimension where tenant_id = $1 and code is not null order by code",
    )
    .bind(ctx.tenant)
    .fetch_all(&mut **tx)
    .await?;
    if heads.is_empty() {
        return Ok(vec![]);
    }
    let values: Vec<(Uuid, String)> = sqlx::query_as("select parent_id, value from e_accounting_dimension__values where tenant_id = $1 and value is not null")
        .bind(ctx.tenant)
        .fetch_all(&mut **tx)
        .await?;
    let needs: Vec<(Uuid, Option<Uuid>, Option<String>, Option<String>)> =
        sqlx::query_as("select parent_id, account, account_type, root_type from e_accounting_dimension__required_for where tenant_id = $1")
            .bind(ctx.tenant)
            .fetch_all(&mut **tx)
            .await?;
    #[allow(clippy::type_complexity)]
    let rules: Vec<(Uuid, Option<Uuid>, Option<Uuid>, Option<Uuid>, Option<Uuid>, Option<Uuid>, Option<Uuid>, Option<String>)> = sqlx::query_as(
        "select parent_id, company, cost_center, project, customer, supplier, item, value from e_accounting_dimension__defaults \
         where tenant_id = $1 order by parent_id, idx",
    )
    .bind(ctx.tenant)
    .fetch_all(&mut **tx)
    .await?;
    Ok(heads
        .into_iter()
        .map(|(id, code, name, source, entity, disabled)| {
            let records = source.as_deref() == Some("Records");
            let mut d = Def {
                code,
                name,
                values: (!records).then(|| values.iter().filter(|v| v.0 == id).map(|v| v.1.trim().to_string()).collect()),
                entity: entity.filter(|_| records),
                disabled: disabled.unwrap_or(false),
                accounts: BTreeSet::new(),
                kinds: BTreeSet::new(),
                defaults: vec![],
            };
            for (_, account, kind, root) in needs.iter().filter(|n| n.0 == id) {
                d.accounts.extend(account);
                d.kinds.extend(kind.iter().chain(root).cloned());
            }
            for (_, company, cost_center, project, customer, supplier, item, value) in rules.iter().filter(|r| r.0 == id) {
                let Some(value) = value.as_ref().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()) else { continue };
                d.defaults.push(Rule {
                    company: *company,
                    cost_center: *cost_center,
                    project: *project,
                    customer: *customer,
                    supplier: *supplier,
                    item: *item,
                    value,
                });
            }
            d
        })
        .collect())
}

/// Fills defaults into the lines of one entry, then checks every value and every required dimension.
pub async fn prepare(tx: &mut Tx, ctx: &Ctx, company: Uuid, lines: &mut [Line]) -> Result<()> {
    let defs = load(tx, ctx).await?;
    if defs.is_empty() {
        if let Some(code) = lines.iter().flat_map(|l| l.dims.keys()).next() {
            return Err(Error::invalid(format!("there is no accounting dimension {code}")));
        }
        return Ok(());
    }
    for l in lines.iter_mut() {
        let fill: Vec<(String, String)> = defs
            .iter()
            .filter(|d| !d.disabled && !l.dims.contains_key(&d.code))
            .filter_map(|d| d.defaults.iter().find(|r| r.matches(company, l)).map(|r| (d.code.clone(), r.value.clone())))
            .collect();
        l.dims.extend(fill);
    }
    let by_code: HashMap<&str, &Def> = defs.iter().map(|d| (d.code.as_str(), d)).collect();
    let mut records: BTreeMap<&str, BTreeSet<Uuid>> = BTreeMap::new();
    for l in lines.iter() {
        for (code, value) in &l.dims {
            let Some(d) = by_code.get(code.as_str()) else { return Err(Error::invalid(format!("there is no accounting dimension {code}"))) };
            match (&d.values, &d.entity) {
                (Some(list), _) if !list.contains(value) => {
                    return Err(Error::invalid(format!("{value} is not one of the values of the dimension {}", d.name)));
                }
                (None, Some(entity)) => {
                    let id = value.parse::<Uuid>().map_err(|_| Error::invalid(format!("the dimension {} takes a {entity} record, not {value}", d.name)))?;
                    records.entry(entity.as_str()).or_default().insert(id);
                }
                _ => {}
            }
        }
    }
    for (entity, ids) in records {
        let ent = ctx.schema.entity(entity).ok_or_else(|| Error::invalid(format!("the dimension's entity {entity} is not in this workspace")))?;
        let ids: Vec<Uuid> = ids.into_iter().collect();
        let found: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "select count(*) from {} where tenant_id = $1 and id = any($2)",
            ferrum_kernel::ddl::q(&ent.table)
        )))
        .bind(ctx.tenant)
        .bind(&ids)
        .fetch_one(&mut **tx)
        .await?;
        if found != ids.len() as i64 {
            return Err(Error::invalid(format!("a dimension value is not a {entity} of this workspace")));
        }
    }
    let required: Vec<&Def> = defs.iter().filter(|d| !d.disabled && (!d.accounts.is_empty() || !d.kinds.is_empty())).collect();
    if required.is_empty() {
        return Ok(());
    }
    let ids: Vec<Uuid> = lines.iter().map(|l| l.account).collect::<BTreeSet<_>>().into_iter().collect();
    let accounts: HashMap<Uuid, (String, Option<String>, Option<String>)> = sqlx::query_as::<_, (Uuid, String, Option<String>, Option<String>)>(
        "select id, coalesce(account_number || ' - ', '') || account_name, account_type, root_type from e_account where tenant_id = $1 and id = any($2)",
    )
    .bind(ctx.tenant)
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|(id, name, kind, root)| (id, (name, kind, root)))
    .collect();
    for l in lines.iter() {
        let Some((name, kind, root)) = accounts.get(&l.account) else { continue };
        for d in &required {
            let needed = d.accounts.contains(&l.account) || kind.iter().chain(root).any(|k| d.kinds.contains(k));
            if needed && !l.dims.contains_key(&d.code) {
                return Err(Error::invalid(format!("{name} needs the dimension {}: give the line one", d.name)));
            }
        }
    }
    Ok(())
}

/// A report's dimension filter: `dimension` (a code, `cost_center` or `project`) and its value.
/// Cost centre and project come back as such, any other code as a `dims @> {code: value}` filter.
pub enum Filter {
    None,
    CostCenter(Uuid),
    Project(Uuid),
    Dims(Value),
}

pub fn filter(code: Option<&str>, value: Option<&str>) -> Result<Filter> {
    let (Some(code), Some(value)) = (code.map(str::trim).filter(|c| !c.is_empty()), value.map(str::trim).filter(|v| !v.is_empty())) else {
        return Ok(Filter::None);
    };
    let uuid = || value.parse::<Uuid>().map_err(|_| Error::field("dimension_value", "choose a record"));
    Ok(match code {
        "cost_center" => Filter::CostCenter(uuid()?),
        "project" => Filter::Project(uuid()?),
        _ => Filter::Dims(serde_json::json!({ code: value })),
    })
}

/// What a value of a dimension is called: a list value itself, a record's title.
pub async fn label(tx: &mut Tx, ctx: &Ctx, entity: Option<&str>, value: &str) -> Result<String> {
    let Some(ent) = entity.and_then(|e| ctx.schema.entity(e)) else { return Ok(value.to_string()) };
    let Ok(id) = value.parse::<Uuid>() else { return Ok(value.to_string()) };
    let title = ent.def.title_field.clone().unwrap_or_else(|| "id".into());
    let t: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "select {}::text from {} where tenant_id = $1 and id = $2",
        ferrum_kernel::ddl::q(&title),
        ferrum_kernel::ddl::q(&ent.table)
    )))
    .bind(ctx.tenant)
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(t.unwrap_or_else(|| value.to_string()))
}

/// Checks a dimension's definition: its code is free and its entity is in the workspace.
pub struct DimensionHook;

#[async_trait::async_trait]
impl DocHook for DimensionHook {
    async fn validate(&self, hc: &mut HookCtx<'_>, doc: &ferrum_kernel::Doc) -> Result<()> {
        if crate::util::s(doc, "source") == Some("Records") {
            let entity = crate::util::s(doc, "source_entity").unwrap_or_default();
            if hc.ctx.schema.entity(entity).is_none() {
                return Err(Error::field("source_entity", format!("there is no entity {entity} in this workspace")));
            }
        }
        for (i, r) in crate::util::rows(doc, "required_for").iter().enumerate() {
            let given = ["account", "account_type", "root_type"].iter().filter(|f| r.get(**f).is_some_and(|v| !v.is_null() && v != "")).count();
            if given != 1 {
                return Err(Error::Validation(vec![
                    ferrum_kernel::FieldError::new("account", "name one account, one account type or one root type").in_row("required_for", i),
                ]));
            }
        }
        Ok(())
    }
}
