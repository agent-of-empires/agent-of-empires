//! Belt-and-suspenders: catch stale rustdoc symbol references in tree.
//!
//! After PR #1844 the closure-callback API replaces the old direct
//! `init_hook_base` getter with `with_hook_base`. Any future copy-paste
//! of the stale symbol name into a doc comment or production code would
//! confuse readers and rustdoc-cross-link tooling. This test fails CI on
//! any literal occurrence of the dead symbol inside `src/`.

use std::fs;
use std::path::{Path, PathBuf};

#[test]
fn no_stale_init_hook_base_references() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders: Vec<(PathBuf, usize, String)> = Vec::new();
    walk(&src, &mut |path, contents| {
        for (idx, line) in contents.lines().enumerate() {
            if line.contains("init_hook_base") {
                offenders.push((path.to_path_buf(), idx + 1, line.trim().to_string()));
            }
        }
    });
    assert!(
        offenders.is_empty(),
        "stale `init_hook_base` references found (replace with `with_hook_base`):\n{}",
        offenders
            .iter()
            .map(|(p, line, text)| format!("  {}:{line}: {text}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn walk(dir: &Path, visit: &mut dyn FnMut(&Path, &str)) {
    let entries = match fs::read_dir(dir) {
        Ok(it) => it,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, visit);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            if let Ok(content) = fs::read_to_string(&path) {
                visit(&path, &content);
            }
        }
    }
}
