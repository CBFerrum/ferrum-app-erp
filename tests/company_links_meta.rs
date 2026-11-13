//! A link to a company's master (a warehouse, an account, a cost centre ...) on a record that has a
//! company must limit its choices to that company: `link_filters: {company: {from: company}}`
//! (`{from: parent.company}` in a child table). Without it a form offers every company's records
//! by bare name (four "Stores" in a workspace of four companies) and the save is refused afterwards.
//! This walks the metadata of ERP's packages and its add-ons, so a new field cannot forget it.
//! It needs no database.

use ferrum_meta::types::{EntityKind, FieldDef as Field, FieldType, OverlayOp, Package};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `(entity, field, why the filter cannot apply)`. Empty: nothing in ERP needs an exemption.
const EXEMPT: &[(&str, &str, &str)] = &[];

fn package_dirs(dir: &Path, out: &mut Vec<PathBuf>) {
    if dir.join("package.yaml").is_file() {
        out.push(dir.to_path_buf());
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for e in read.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if e.path().is_dir() && !matches!(n.as_str(), "target" | "node_modules" | "ui" | ".git") {
            package_dirs(&e.path(), out);
        }
    }
}

pub fn repo_packages() -> Vec<Package> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut dirs = Vec::new();
    package_dirs(&root.join("packages"), &mut dirs);
    package_dirs(&root.join("addons"), &mut dirs);
    dirs.sort();
    assert!(dirs.len() > 30, "found only {} packages", dirs.len());
    dirs.iter().map(|d| ferrum_meta::catalog::load_package_dir(d).unwrap_or_else(|e| panic!("{}: {e}", d.display()))).collect()
}

/// Every entity's kind and fields, with the fields other packages add to it.
fn entities(pkgs: &[Package]) -> BTreeMap<String, (EntityKind, Vec<Field>)> {
    let mut m: BTreeMap<String, (EntityKind, Vec<Field>)> = BTreeMap::new();
    for p in pkgs {
        for e in &p.entities {
            let x = m.entry(e.name.clone()).or_insert((e.kind, vec![]));
            x.0 = e.kind;
            x.1.extend(e.fields.iter().cloned());
        }
    }
    for p in pkgs {
        for ov in &p.manifest.overlays {
            for op in &ov.ops {
                match op {
                    OverlayOp::AddField { field, .. } => m.entry(ov.entity.clone()).or_insert((EntityKind::Master, vec![])).1.push(field.clone()),
                    OverlayOp::DefineEntity { entity } => {
                        let x = m.entry(entity.name.clone()).or_insert((entity.kind, vec![]));
                        x.1.extend(entity.fields.iter().cloned());
                    }
                    _ => {}
                }
            }
        }
    }
    m
}

fn has_company(fields: &[Field]) -> bool {
    fields.iter().any(|f| f.name == "company" && f.ftype == FieldType::Link && f.to.as_deref() == Some("Company"))
}

/// What is wrong with the links in `pkgs`, given the masters that belong to a company (`masters`, which may
/// come from packages other than `pkgs`, such as ERP's for an app built on it).
pub fn faults(pkgs: &[Package], others: &[Package], exempt: &[(&str, &str, &str)]) -> Vec<String> {
    let mut all: Vec<Package> = others.to_vec();
    all.extend(pkgs.iter().cloned());
    let ents = entities(&all);
    let masters: Vec<&String> = ents.iter().filter(|(_, (k, f))| *k == EntityKind::Master && has_company(f)).map(|(n, _)| n).collect();
    let mine = entities(pkgs);
    // Who holds each child entity as a table.
    let mut parents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (n, (k, fs)) in &ents {
        if *k != EntityKind::Child {
            for f in fs.iter().filter(|f| f.ftype == FieldType::Table) {
                parents.entry(f.of.as_deref().unwrap_or("")).or_default().push(n);
            }
        }
    }
    let mut bad = Vec::new();
    for (n, (_, fields)) in &mine {
        // Whether the record has a company is known from every package's fields, not only these.
        let (kind, whole) = (&ents[n].0, &ents[n].1);
        let source = if *kind != EntityKind::Child {
            has_company(whole).then_some("company")
        } else if has_company(whole) {
            Some("company")
        } else {
            let ps = parents.get(n.as_str()).cloned().unwrap_or_default();
            (!ps.is_empty() && ps.iter().all(|p| has_company(&ents[*p].1))).then_some("parent.company")
        };
        let Some(source) = source else { continue };
        for f in fields.iter().filter(|f| f.ftype == FieldType::Link && f.name != "company") {
            let Some(to) = f.to.as_deref() else { continue };
            if !masters.iter().any(|m| m.as_str() == to) || exempt.iter().any(|(e, fl, why)| *e == n && *fl == f.name && !why.is_empty()) {
                continue;
            }
            let want = json!({"from": source});
            if f.link_filters.as_ref().and_then(|l| l.get("company")) != Some(&want) {
                bad.push(format!("{n}.{} (to {to}) needs link_filters: {{company: {{from: {source}}}}}", f.name));
            }
        }
    }
    bad.sort();
    bad.dedup();
    bad
}

/// A master that belongs to a company shows it in its list.
pub fn unlisted(pkgs: &[Package]) -> Vec<String> {
    entities(pkgs)
        .into_iter()
        .filter(|(_, (k, f))| *k == EntityKind::Master && has_company(f))
        .filter(|(_, (_, f))| !f.iter().any(|f| f.name == "company" && f.in_list))
        .map(|(n, _)| format!("{n}.company is not in_list"))
        .collect()
}

#[test]
fn every_link_to_a_company_master_is_limited_to_the_records_company() {
    let pkgs = repo_packages();
    let bad = faults(&pkgs, &[], EXEMPT);
    assert!(bad.is_empty(), "{} links offer every company's records:\n{}", bad.len(), bad.join("\n"));
}

#[test]
fn a_master_that_belongs_to_a_company_lists_it() {
    let bad = unlisted(&repo_packages());
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn the_walk_sees_what_it_should() {
    // A field without the filter is found: the walk is not vacuous.
    let mut pkgs = repo_packages();
    let p = pkgs.iter_mut().find(|p| p.entities.iter().any(|e| e.name == "SalesOrder")).unwrap();
    let e = p.entities.iter_mut().find(|e| e.name == "SalesOrder").unwrap();
    let f = e.fields.iter_mut().find(|f| f.name == "set_warehouse").unwrap();
    f.link_filters = None;
    assert_eq!(faults(&pkgs, &[], EXEMPT), vec!["SalesOrder.set_warehouse (to Warehouse) needs link_filters: {company: {from: company}}"]);
}
