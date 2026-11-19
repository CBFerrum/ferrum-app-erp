//! An action a person can press (not `hidden`) says what it promises: a contract with `effect`, `returns` and
//! `completion` (docs/guides/06 of the platform; `scripts/agent_parity.py` checks the same against its registry).
//! One test per package of ERP and its add-ons, so a new action without one fails the build of its own package.
//! This reads the packages' files and needs no database.

use ferrum_meta::types::{ActionDef, OverlayOp, Package};
use std::path::{Path, PathBuf};

fn package_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    let read = std::fs::read_dir(dir).ok()?;
    for e in read.flatten() {
        let p = e.path();
        let n = e.file_name().to_string_lossy().to_string();
        if !p.is_dir() || matches!(n.as_str(), "target" | "node_modules" | "ui" | ".git" | "tests" | "src") {
            continue;
        }
        if p.join("package.yaml").is_file() {
            let text = std::fs::read_to_string(p.join("package.yaml")).unwrap_or_default();
            if text.lines().any(|l| l.trim_end() == format!("package: {name}")) {
                return Some(p);
            }
        } else if let Some(found) = package_dir(&p, name) {
            return Some(found);
        }
    }
    None
}

fn load(name: &str) -> Package {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = ["packages", "addons"].iter().find_map(|d| package_dir(&root.join(d), name)).unwrap_or_else(|| panic!("no package {name}"));
    ferrum_meta::catalog::load_package_dir(&dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
}

fn lacks(a: &ActionDef) -> Option<&'static str> {
    let c = a.contract.as_ref()?;
    let returns = c.returns.as_ref().is_some_and(|r| r.as_object().is_some_and(|o| !o.is_empty()));
    if c.effect.is_none() {
        Some("effect")
    } else if !returns {
        Some("returns")
    } else if c.completion.as_deref().is_none_or(|t| t.trim().is_empty()) {
        Some("completion")
    } else {
        None
    }
}

fn check(name: &str) {
    let p = load(name);
    let mut actions: Vec<(String, &ActionDef)> = Vec::new();
    for e in &p.entities {
        actions.extend(e.actions.iter().map(|a| (e.name.clone(), a)));
    }
    for o in &p.manifest.overlays {
        for op in &o.ops {
            if let OverlayOp::AddAction { action } = op {
                actions.push((o.entity.clone(), action));
            }
        }
    }
    assert!(!actions.is_empty(), "{name} has no actions: is the name right?");
    let bad: Vec<String> = actions
        .iter()
        .filter(|(_, a)| !a.hidden)
        .filter_map(|(e, a)| match &a.contract {
            None => Some(format!("{e}.{} has no contract", a.name)),
            Some(_) => lacks(a).map(|k| format!("{e}.{} has a contract without {k}", a.name)),
        })
        .collect();
    assert!(bad.is_empty(), "every action a person can press needs a contract with effect, returns and completion:\n{}", bad.join("\n"));
}

#[test]
fn accounting() {
    check("accounting");
}
