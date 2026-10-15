//! Table-driven checks for migrations that rewrite one TOML or JSON file.

use std::path::Path;

/// Each row writes `before` to `file` in a fresh directory (no file when
/// `None`), runs the migration on that path, then compares the result against
/// `after` (no file when `None`), parsed as JSON for a `.json` file and as TOML
/// otherwise. A row whose `after` is textually `before` must leave the bytes
/// untouched, and a second run must change nothing.
pub(crate) fn assert_rewrites(
    file: &str,
    run: impl Fn(&Path) -> anyhow::Result<()>,
    cases: &[(Option<&str>, Option<&str>)],
) {
    let parse = |s: &str| -> serde_json::Value {
        if file.ends_with(".json") {
            serde_json::from_str(s).unwrap()
        } else {
            serde_json::to_value(s.parse::<toml::Table>().unwrap()).unwrap()
        }
    };
    for &(before, after) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file);
        if let Some(before) = before {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, before).unwrap();
        }
        run(&path).unwrap_or_else(|e| panic!("{before:?}: {e:#}"));
        let got = std::fs::read_to_string(&path).ok();
        run(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).ok(),
            got,
            "second run changed {before:?}"
        );
        if before.is_some() && before == after {
            assert_eq!(got.as_deref(), after, "unchanged: {before:?}");
        } else {
            assert_eq!(
                got.as_deref().map(parse),
                after.map(parse),
                "{before:?} -> {got:?}"
            );
        }
    }
}
