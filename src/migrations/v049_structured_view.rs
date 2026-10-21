//! Migration v049: fold `acp.offer_structured_in_new_session` and
//! `acp.default_new_session_view` into `acp.structured_view` in the global
//! config and every profile config. An explicit `offer = false` becomes `off`;
//! otherwise a `terminal` view stays `terminal`, and anything else (including
//! `auto`) becomes `structured`. A file with neither key is left alone.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

fn run_in(app_dir: &Path) -> Result<()> {
    for path in config_file::all_configs(app_dir)? {
        migrate_config_file(&path)?;
    }
    Ok(())
}

fn migrate_config_file(path: &Path) -> Result<()> {
    config_file::rewrite_strict(path, "v049", |doc| {
        let Some(acp) = doc.get_mut("acp").and_then(toml::Value::as_table_mut) else {
            return false;
        };
        let offer = acp.remove("offer_structured_in_new_session");
        let view = acp.remove("default_new_session_view");
        if offer.is_none() && view.is_none() {
            return false;
        }
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
        assert_rewrites("config.toml", migrate_config_file, &rows);
    }

    #[test]
    fn leaves_configs_without_legacy_keys_untouched() {
        let untouched = "[acp]\nmax_concurrent_workers = 4\n";
        let migrated = "[acp]\nstructured_view = \"off\"\n";
        assert_rewrites(
            "config.toml",
            migrate_config_file,
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
}
