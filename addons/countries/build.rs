//! Embeds this app's metadata packages (`packages/<name>/...`) into the crate.

use std::fmt::Write;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = read.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().to_string();
        let rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        if e.path().is_dir() {
            walk(&e.path(), &rel, out);
        } else {
            out.push((rel, e.path()));
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("packages");
    // Relative to the crate, like the paths below: checkouts at other paths can share a target
    // directory (Cargo hashes path crates relative to their workspace) without mixing up files.
    println!("cargo:rerun-if-changed=packages");
    let mut code = String::from("// (package, path inside the package, content)\npub static EMBEDDED: &[(&str, &str, &str)] = &[\n");
    let mut packages: Vec<_> =
        std::fs::read_dir(&root).expect("packages/").filter_map(|e| e.ok()).filter(|e| e.path().join("package.yaml").is_file()).collect();
    packages.sort_by_key(|e| e.file_name());
    for pkg in packages {
        let name = pkg.file_name().to_string_lossy().to_string();
        let mut files = Vec::new();
        walk(&pkg.path(), "", &mut files);
        for (rel, _) in files {
            let file = format!("/packages/{name}/{rel}");
            writeln!(code, "    ({name:?}, {rel:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {file:?}))),").expect("write");
        }
    }
    code.push_str("];\n");
    std::fs::write(Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("packages.rs"), code).expect("write packages.rs");
    data_apps();
}

/// A module per country described as data (`data/<code>.json`, docs/countries.md): the same entry points a coded country's
/// `app!` gives, around a `COUNTRY` read from the file once; and `data_countries()` listing them.
fn data_apps() {
    println!("cargo:rerun-if-changed=data");
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("data");
    let mut codes: Vec<String> = std::fs::read_dir(&dir)
        .map(|r| r.filter_map(|e| e.ok()).filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".json").map(str::to_string)).collect())
        .unwrap_or_default();
    codes.sort();
    let mut code = String::new();
    for c in &codes {
        assert!(c.len() == 2 && c.chars().all(|ch| ch.is_ascii_lowercase()), "data/{c}.json: a country's file is its two lower-case letters");
        writeln!(
            code,
            r#"pub mod r#{c} {{
    /// The app's `app.yaml`.
    pub const MANIFEST: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/apps/{c}.yaml"));
    /// The app's package.
    pub const PACKAGE: &str = "{c}";
    /// The country as its data file describes it, read once.
    pub static COUNTRY: std::sync::LazyLock<&'static crate::engine::Country> = std::sync::LazyLock::new(|| {{
        crate::engine::Country::from_json("{c}", include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/data/{c}.json")))
            .unwrap_or_else(|e| panic!("data/{c}.json: {{e}}"))
    }});
    pub fn packages() -> std::result::Result<Vec<ferrum_meta::Package>, ferrum_meta::MetaError> {{
        crate::packages_of(PACKAGE)
    }}
    pub fn register(h: &mut ferrum_kernel::HookRegistry) {{
        ferrum_app_erp::localization::add(std::sync::Arc::new(crate::engine::National(*COUNTRY)));
        h.scoped(&[PACKAGE.to_string()], |h| {{
            for entity in ["Company", "Customer", "Supplier"] {{
                h.register(entity, std::sync::Arc::new(crate::engine::NumberHook(*COUNTRY)));
            }}
        }});
    }}
    pub fn reports() -> Vec<std::sync::Arc<dyn ferrum_domain::reports::ReportSource>> {{
        Vec::new()
    }}
    pub async fn maintenance(_k: &ferrum_kernel::Kernel, _tenant: uuid::Uuid) -> ferrum_kernel::Result<()> {{
        Ok(())
    }}
}}"#
        )
        .expect("write");
    }
    code.push_str("\n/// The countries described as data files.\npub fn data_countries() -> Vec<(&'static str, &'static engine::Country)> {\n    vec![");
    for c in &codes {
        write!(code, "(\"{c}\", *r#{c}::COUNTRY), ").expect("write");
    }
    code.push_str("]\n}\n");
    std::fs::write(Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("data_apps.rs"), code).expect("write data_apps.rs");
}
