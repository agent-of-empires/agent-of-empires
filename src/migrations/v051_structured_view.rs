//! Migration v049: fold `acp.offer_structured_in_new_session` and
//! `acp.default_new_session_view` into `acp.structured_view` in the global
//! config and every profile config. An explicit `offer = false` becomes `off`;
//! otherwise a `terminal` view stays `terminal`, and anything else (including
//! `auto`) becomes `structured`. A file with neither key is left alone, and a
//! profile with one key inherits the other from the global file.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

const OFFER: &str = "offer_structured_in_new_session";
const VIEW: &str = "default_new_session_view";

/// The two legacy keys as found in one config file.
#[derive(Default)]
struct Legacy {
    offer: Option<toml::Value>,
    view: Option<toml::Value>,
}

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

/// Profiles go first and inherit the global file's legacy values for a key
/// they leave out, as they did at runtime. The global file goes last, so an
/// interrupted run still finds its keys on the next start.
fn run_in(app_dir: &Path) -> Result<()> {
    let global_path = app_dir.join("config.toml");
    let global = legacy_in(&global_path);
    for path in config_file::profile_configs(app_dir)? {
        migrate_config_file(&path, &global)?;
    }
    migrate_config_file(&global_path, &Legacy::default())
}

fn legacy_in(path: &Path) -> Legacy {
    let doc = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok());
    let acp = doc
        .as_ref()
        .and_then(|doc| doc.get("acp"))
        .and_then(toml::Value::as_table);
    Legacy {
        offer: acp.and_then(|acp| acp.get(OFFER)).cloned(),
        view: acp.and_then(|acp| acp.get(VIEW)).cloned(),
    }
}

fn migrate_config_file(path: &Path, inherited: &Legacy) -> Result<()> {
    config_file::rewrite_strict(path, "v049", |doc| {
        let Some(acp) = doc.get_mut("acp").and_then(toml::Value::as_table_mut) else {
            return false;
        };
        let own = Legacy {
            offer: acp.remove(OFFER),
            view: acp.remove(VIEW),
        };
        if own.offer.is_none() && own.view.is_none() {
            return false;
        }
        let offer = own.offer.or_else(|| inherited.offer.clone());
        let view = own.view.or_else(|| inherited.view.clone());
        let migrated = if matches!(offer, Some(toml::Value::Boolean(false))) {
            "off"
        } else if view.as_ref().and_then(toml::Value::as_str) == Some("terminal") {
            "terminal"
        } else {
            "structured"
        };
        // An existing `structured_view` (a prior run, or a manual edit) wins.
        acp.entry("structured_view".to_string())
            .or_insert_with(|| toml::Value::String(migrated.to_string()));
        info!(
            "v049: migrated acp structured view to {migrated} in {}",
            path.display()
        );
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;

    fn acp(body: &str) -> Option<String> {
        Some(format!("[acp]\n{body}"))
    }

    #[test]
    fn folds_legacy_keys_into_structured_view() {
        let cases: &[(&str, &str)] = &[
            ("offer_structured_in_new_session = false\n", "off"),
            (
                "offer_structured_in_new_session = false\ndefault_new_session_view = \"structured\"\n",
                "off",
            ),
            (
                "offer_structured_in_new_session = false\ndefault_new_session_view = \"terminal\"\n",
                "off",
            ),
            ("offer_structured_in_new_session = true\n", "structured"),
            (
                "offer_structured_in_new_session = true\ndefault_new_session_view = \"terminal\"\n",
                "terminal",
            ),
            ("default_new_session_view = \"terminal\"\n", "terminal"),
            ("default_new_session_view = \"structured\"\n", "structured"),
            ("default_new_session_view = \"auto\"\n", "structured"),
            // Not a TOML boolean false, not the exact string "terminal".
            ("offer_structured_in_new_session = \"false\"\n", "structured"),
            ("default_new_session_view = \"TERMINAL\"\n", "structured"),
            // A value already under the new key outranks the legacy ones.
            (
                "offer_structured_in_new_session = false\nstructured_view = \"terminal\"\n",
                "terminal",
            ),
        ];
        let rows: Vec<(Option<String>, Option<String>)> = cases
            .iter()
            .map(|(old, new)| (acp(old), acp(&format!("structured_view = \"{new}\"\n"))))
            .collect();
        let rows: Vec<(Option<&str>, Option<&str>)> = rows
            .iter()
            .map(|(a, b)| (a.as_deref(), b.as_deref()))
            .collect();
        assert_rewrites(
            "config.toml",
            |path| migrate_config_file(path, &Legacy::default()),
            &rows,
        );
    }

    #[test]
    fn leaves_configs_without_legacy_keys_untouched() {
        let untouched = "[acp]\nmax_concurrent_workers = 4\n";
        let migrated = "[acp]\nstructured_view = \"off\"\n";
        assert_rewrites(
            "config.toml",
            |path| migrate_config_file(path, &Legacy::default()),
            &[
                (Some(untouched), Some(untouched)),
                (
                    Some("[session]\nsmart_rename = true\n"),
                    Some("[session]\nsmart_rename = true\n"),
                ),
                (Some(migrated), Some(migrated)),
            ],
        );
    }

    #[test]
    fn a_profile_inherits_the_legacy_key_it_leaves_out() {
        // (global, profile, migrated global, migrated profile)
        let cases: &[(&str, &str, &str, &str)] = &[
            // The profile's terminal view must not turn on what the global hid.
            (
                "offer_structured_in_new_session = false\n",
                "default_new_session_view = \"terminal\"\n",
                "off",
                "off",
            ),
            // The profile's opt-in must not override the global terminal view.
            (
                "default_new_session_view = \"terminal\"\n",
                "offer_structured_in_new_session = true\n",
                "terminal",
                "terminal",
            ),
            // A profile that sets both keys ignores the global ones.
            (
                "offer_structured_in_new_session = false\n",
                "offer_structured_in_new_session = true\ndefault_new_session_view = \"structured\"\n",
                "off",
                "structured",
            ),
        ];
        for (global, profile, want_global, want_profile) in cases {
            let dir = tempfile::tempdir().unwrap();
            let profile_dir = dir.path().join("profiles").join("work");
            std::fs::create_dir_all(&profile_dir).unwrap();
            std::fs::write(dir.path().join("config.toml"), format!("[acp]\n{global}")).unwrap();
            std::fs::write(profile_dir.join("config.toml"), format!("[acp]\n{profile}")).unwrap();

            run_in(dir.path()).unwrap();

            let read = |path: std::path::PathBuf| {
                std::fs::read_to_string(path)
                    .unwrap()
                    .parse::<toml::Table>()
                    .unwrap()
            };
            let expect = |want: &str| {
                format!("[acp]\nstructured_view = \"{want}\"\n")
                    .parse::<toml::Table>()
                    .unwrap()
            };
            assert_eq!(read(dir.path().join("config.toml")), expect(want_global));
            assert_eq!(read(profile_dir.join("config.toml")), expect(want_profile));
        }
    }
}
