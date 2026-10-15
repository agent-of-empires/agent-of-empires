//! Migration v024: backfill `detect_as` on sessions created while their tool
//! had no `[session.agent_detect_as]` entry, which left the alias empty
//! forever and froze their status at Idle.
//!
//! `status_rules::effective_detect_as` already falls back to the live config,
//! but `detect_as` also drives sandbox agent selection, hook install and
//! container config, so the stored field is fixed rather than distrusted by
//! every reader. Backfilling pins the alias, so a later retarget of the entry
//! no longer reaches the row: that is the state a session built with the entry
//! in place would have had. A sessions.json that fails to read or parse is
//! logged and skipped.

use anyhow::Result;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

/// `[session.agent_detect_as]` for a profile, resolved exactly as the runtime
/// resolves it (global config merged with the profile's overrides) so a
/// backfilled value matches what detection would have computed.
type AliasLookup<'a> = dyn Fn(&str) -> HashMap<String, String> + 'a;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir, &|profile| {
        crate::session::config::profile_config::resolve_config_or_warn(profile)
            .session
            .agent_detect_as
    })
}

pub(crate) fn run_in(app_dir: &Path, aliases_for: &AliasLookup) -> Result<()> {
    let profiles_dir = app_dir.join("profiles");
    if profiles_dir.exists() {
        for entry in fs::read_dir(&profiles_dir)? {
            let entry = entry?;
            if !entry.path().is_dir() {
                continue;
            }
            // The directory name is the profile name; a name that is not valid
            // UTF-8 cannot be a profile, so it cannot own sessions to heal.
            let Some(profile) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            backfill(&entry.path().join("sessions.json"), &aliases_for(&profile))?;
        }
    }
    // Legacy top-level sessions.json (pre-profiles layout). An empty profile
    // name resolves to the default profile, matching `effective_profile`.
    backfill(&app_dir.join("sessions.json"), &aliases_for(""))?;
    Ok(())
}

/// Set `detect_as` on every session whose tool is aliased but whose stored
/// alias is missing or empty. Sessions with an alias already stored are left
/// alone: a non-empty value is a deliberate per-session pin, and overwriting it
/// from config would undo any session the user re-targeted by hand.
fn backfill(path: &Path, aliases: &HashMap<String, String>) -> Result<()> {
    if !path.exists() || aliases.is_empty() {
        return Ok(());
    }
    // Read failures are skipped for the same reason parse failures are: this is
    // a best-effort backfill, and a permissions hiccup or non-UTF-8 file must
    // not abort boot. `?` here would have propagated straight out of `run()`.
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            debug!("v024: failed to read {}: {e}, skipping", path.display());
            return Ok(());
        }
    };
    let mut value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            debug!("v024: failed to parse {}: {e}, skipping", path.display());
            return Ok(());
        }
    };

    let mut healed = 0usize;
    if let Some(array) = value.as_array_mut() {
        for instance in array.iter_mut() {
            let Some(obj) = instance.as_object_mut() else {
                continue;
            };
            // `detect_as` is `skip_serializing_if = "String::is_empty"`, so an
            // absent field and an empty one are the same state.
            let stored = obj.get("detect_as").and_then(|v| v.as_str()).unwrap_or("");
            if !stored.is_empty() {
                continue;
            }
            let Some(tool) = obj.get("tool").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(alias) = aliases.get(tool) else {
                continue;
            };
            obj.insert(
                "detect_as".to_string(),
                serde_json::Value::String(alias.clone()),
            );
            healed += 1;
        }
    }

    if healed > 0 {
        crate::session::atomic_write(path, serde_json::to_string_pretty(&value)?.as_bytes())?;
        info!(
            "v024: backfilled detect_as on {healed} session(s) in {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;

    fn aliases(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn backfills_only_unaliased_rows_with_a_mapped_tool() {
        let map = aliases(&[("claude-personal", "claude")]);
        let corrupt = "{ not valid json";
        assert_rewrites(
            "sessions.json",
            |path| backfill(path, &map),
            &[
                (
                    Some(
                        r#"[
                {"id":"a","tool":"claude-personal"},
                {"id":"b","tool":"claude-personal","detect_as":""},
                {"id":"c","tool":"claude-personal","detect_as":"codex"},
                {"id":"d","tool":"codex-company"},
                {"id":"e","tool":"claude"},
                {"id":"f"}
            ]"#,
                    ),
                    // Absent or empty aliases fill; a stored alias is a deliberate
                    // pin; unmapped, built-in and tool-less rows stay as they are.
                    Some(
                        r#"[
                {"id":"a","tool":"claude-personal","detect_as":"claude"},
                {"id":"b","tool":"claude-personal","detect_as":"claude"},
                {"id":"c","tool":"claude-personal","detect_as":"codex"},
                {"id":"d","tool":"codex-company"},
                {"id":"e","tool":"claude"},
                {"id":"f"}
            ]"#,
                    ),
                ),
                (Some(corrupt), Some(corrupt)),
                (None, None),
            ],
        );
        let row = r#"[{"id":"a","tool":"claude-personal"}]"#;
        assert_rewrites(
            "sessions.json",
            |path| backfill(path, &HashMap::new()),
            &[(Some(row), Some(row))],
        );
    }

    #[test]
    fn walks_profiles_and_legacy_layouts_with_per_profile_aliases() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("profiles").join("work");
        fs::create_dir_all(&work).unwrap();
        let row = r#"[{"id":"a","tool":"my-agent"}]"#;
        fs::write(work.join("sessions.json"), row).unwrap();
        fs::write(dir.path().join("sessions.json"), row).unwrap();

        // Each profile resolves its own map, and the legacy file is asked for
        // the default profile's (empty name).
        run_in(dir.path(), &|profile| match profile {
            "work" => aliases(&[("my-agent", "claude")]),
            "" => aliases(&[("my-agent", "codex")]),
            _ => HashMap::new(),
        })
        .unwrap();

        let read = |p: std::path::PathBuf| -> serde_json::Value {
            serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap()
        };
        assert_eq!(read(work.join("sessions.json"))[0]["detect_as"], "claude");
        assert_eq!(
            read(dir.path().join("sessions.json"))[0]["detect_as"],
            "codex"
        );
    }
}
