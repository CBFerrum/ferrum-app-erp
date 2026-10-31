//! Embeds ERP's own interface (`ui/dist`, built from `ui/src` and committed; Ferrum
//! docs/plan/apps-ui.md) into the crate.

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
    // The interface's built files, without the build's own notes (dotfiles such as .source).
    let ui = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui").join("dist");
    println!("cargo:rerun-if-changed={}", ui.display());
    let mut code = String::from("/// ERP's own interface: its built files (path under ui/dist, content).\npub static UI: &[(&str, &[u8])] = &[\n");
    let mut files = Vec::new();
    walk(&ui, "", &mut files);
    for (rel, path) in files.into_iter().filter(|(rel, _)| !rel.split('/').any(|s| s.starts_with('.'))) {
        let abs = path.canonicalize().expect("ui file");
        writeln!(code, "    ({rel:?}, include_bytes!({abs:?})),").expect("write");
    }
    code.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(out.join("ui.rs"), code).expect("write ui.rs");
}
