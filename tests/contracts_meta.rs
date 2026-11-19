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

/// `(package, Entity.action, why it has none yet)`: an action left without a contract on purpose. Each says what
/// has to change first; the list only shrinks.
const LEFT: &[(&str, &str, &str)] = &[
    (
        "selling",
        "SalesOrder.update_items",
        "Also takes a list of line changes (items), which cannot be declared yet: the platform has no array type for action parameters (src/change.rs:74-80)",
    ),
    (
        "buying",
        "PurchaseOrder.update_items",
        "Also takes a list of line changes (items), which cannot be declared yet: the platform has no array type for action parameters (src/change.rs:74-80)",
    ),
    (
        "inventory",
        "CarrierShipment.track",
        "Declared permission read but it writes the shipment's tracking under the system's rights and calls the carrier (src/shipping.rs:693-704)",
    ),
    (
        "finance",
        "BankTransaction.reconcile",
        "Takes a list of vouchers (vouchers), which cannot be declared yet: the platform has no array type for action parameters (addons/finance/src/bank.rs:418)",
    ),
    (
        "finance",
        "Subscription.change_plans",
        "Takes a list of plans (plans), which cannot be declared yet: the platform has no array type for action parameters (addons/finance/src/subscription.rs:549)",
    ),
    (
        "procurement",
        "MaterialRequest.make_purchase_order",
        "Also takes a list of line ids (items): the platform has no array type for action parameters (addons/procurement/src/material_request.rs:408)",
    ),
    (
        "procurement",
        "MaterialRequest.make_rfq",
        "Also takes lists (items, suppliers): the platform has no array type for action parameters (addons/procurement/src/material_request.rs:500-505)",
    ),
    (
        "procurement",
        "MaterialRequest.make_work_order",
        "Also takes a list of line ids (items): the platform has no array type for action parameters (addons/procurement/src/material_request.rs:459)",
    ),
    (
        "procurement",
        "MaterialRequest.make_stock_entry",
        "Also takes a list of line ids (items): the platform has no array type for action parameters (addons/procurement/src/material_request.rs:539)",
    ),
    (
        "procurement",
        "RequestForQuotation.send",
        "Also takes a list of suppliers: the platform has no array type for action parameters (addons/procurement/src/rfq.rs:221)",
    ),
    // LEFT-ROWS
];

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
        .filter(|(e, a)| !LEFT.iter().any(|(p, k, why)| *p == name && *k == format!("{e}.{}", a.name) && !why.is_empty()))
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

#[test]
fn selling() {
    check("selling");
}

#[test]
fn buying() {
    check("buying");
}

#[test]
fn inventory() {
    check("inventory");
}

#[test]
fn manufacturing() {
    check("manufacturing");
}

#[test]
fn finance() {
    check("finance");
}

#[test]
fn procurement() {
    check("procurement");
}

#[test]
fn mfg_ops() {
    check("mfg_ops");
}
