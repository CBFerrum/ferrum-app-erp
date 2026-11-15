//! Every call ERP and its add-ons make to a service outside goes through the platform's vetted client, asked for the
//! workspace it is made for (`ferrum_app_erp::providers`, or `vetted_client_for` itself): a client built here would
//! skip the sandbox's check and the address checks. This reads the sources, so a new integration cannot forget it.
//! It needs no database.

use std::path::Path;

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    for e in read.flatten() {
        let p = e.path();
        let n = e.file_name().to_string_lossy().to_string();
        if p.is_dir() && !matches!(n.as_str(), "target" | "node_modules" | "ui" | ".git" | "tests") {
            rust_files(&p, out);
        } else if n.ends_with(".rs") {
            out.push(p);
        }
    }
}

#[test]
fn no_source_builds_its_own_http_client() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in ["src", "addons", "pricing", "promotions"] {
        rust_files(&root.join(dir), &mut files);
    }
    assert!(files.len() > 100, "found only {} sources", files.len());
    // Written apart, so that this file does not find itself.
    let needles: Vec<String> = [("Client::", "builder"), ("Client::", "new("), ("reqwest::", "get("), ("ClientBuilder", ""), ("vetted_client", "(")]
        .iter()
        .map(|(a, b)| format!("{a}{b}"))
        .collect();
    let mut found = Vec::new();
    for f in &files {
        let text = std::fs::read_to_string(f).unwrap();
        for (i, line) in text.lines().enumerate() {
            if needles.iter().any(|n| line.contains(n.as_str())) {
                found.push(format!("{}:{}: {}", f.strip_prefix(root).unwrap().display(), i + 1, line.trim()));
            }
        }
    }
    assert!(found.is_empty(), "calls outside that skip the sandbox's check and the platform's address checks:\n{}", found.join("\n"));
}
