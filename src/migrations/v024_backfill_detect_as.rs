//! Migration v024: backfill the `detect_as` status-detection alias onto
//! sessions created while their tool had no `[session.agent_detect_as]` entry.
//!
//! `Instance::detect_as` is resolved once at session build and persisted. A
//! session created before its custom agent was added to
//! `[session.agent_detect_as]` therefore stores an empty alias forever, and
//! nothing re-resolved it. With no alias, `status_rules::detection_tool`
//! reports the custom tool name, which has neither configured rules nor a
//! built-in detector, so `detect_status_from_content_in` returns its
//! `Status::Idle` fallback on every pane capture: the session's status freezes
//! at Idle and never moves to Running again.
//!
//! `status_rules::effective_detect_as` now consults the live config when the
//! stored field is empty, so detection is already correct without this
//! migration. This exists because `detect_as` has consumers beyond status
//! detection (sandbox agent selection, hook install, container config), and
//! leaving a field wrong on disk means every future reader has to know to
//! distrust it. Per `AGENTS.md > Data Migrations` the stored data gets fixed
//! rather than accumulating read-side shims.
//!
//! The runtime fallback is not made redundant by this: this is a one-shot, and
//! a session built after it runs but before its tool is added to
//! `[session.agent_detect_as]` lands in exactly the same state. The migration
//! fixes the rows that exist; the fallback keeps the next ones correct.
//!
//! The converse is also true: because a non-empty stored alias wins over the
//! registry, backfilling a row converts a value the fallback was tracking live
//! into a fixed pin, so a later retarget or removal of the entry no longer
//! reaches it. That is deliberate: it is the same state a session built with the
//! entry in place would have had.
//!
//! ## Failure policy
//!
//! Per `AGENTS.md > Data Migrations`, a returned `Err` aborts boot. A
//! sessions.json that fails to read or parse is logged and skipped: an
//! unreadable or corrupt file must not block boot or spam every launch, and
//! this backfill is best-effort, since the runtime fallback covers detection
//! either way. Only `get_app_dir` and directory-read failures propagate.

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

    fn aliases(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn backfills_only_unaliased_rows_with_a_mapped_tool() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(
            &path,
            r#"[
                {"id":"a","tool":"claude-personal"},
                {"id":"b","tool":"claude-personal","detect_as":""},
                {"id":"c","tool":"claude-personal","detect_as":"codex"},
                {"id":"d","tool":"codex-company"},
                {"id":"e","tool":"claude"},
                {"id":"f"}
            ]"#,
        )
        .unwrap();

        backfill(&path, &aliases(&[("claude-personal", "claude")])).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let arr = v.as_array().unwrap();
        // absent alias + mapped tool -> filled (the bug footprint)
        assert_eq!(arr[0]["detect_as"], "claude");
        // empty alias is the same state as absent -> filled
        assert_eq!(arr[1]["detect_as"], "claude");
        // an alias already stored is a deliberate pin -> untouched
        assert_eq!(arr[2]["detect_as"], "codex");
        // tool with no config entry -> left for the runtime fallback to miss too
        assert!(arr[3].get("detect_as").is_none());
        // built-in tool -> never aliased
        assert!(arr[4].get("detect_as").is_none());
        // row without a tool -> untouched, not a panic
        assert!(arr[5].get("detect_as").is_none());
    }

    #[test]
    fn is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(&path, r#"[{"id":"a","tool":"claude-personal"}]"#).unwrap();
        let map = aliases(&[("claude-personal", "claude")]);
        backfill(&path, &map).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        backfill(&path, &map).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
    }

    #[test]
    fn skips_unreadable_and_absent_and_unmapped() {
        let dir = tempfile::tempdir().unwrap();
        // Missing file.
        backfill(&dir.path().join("nope.json"), &aliases(&[("a", "claude")])).unwrap();

        // Corrupt file is left exactly as found.
        let corrupt = dir.path().join("corrupt.json");
        fs::write(&corrupt, "{ not valid json").unwrap();
        backfill(&corrupt, &aliases(&[("a", "claude")])).unwrap();
        assert_eq!(fs::read_to_string(&corrupt).unwrap(), "{ not valid json");

        // No aliases configured means no rewrite at all.
        let path = dir.path().join("sessions.json");
        let row = r#"[{"id":"a","tool":"claude-personal"}]"#;
        fs::write(&path, row).unwrap();
        backfill(&path, &HashMap::new()).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), row);
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
