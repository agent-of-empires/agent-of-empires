//! Migration v022: prune removed tuning settings from saved configs.
//!
//! A batch of internal tuning knobs left the settings schema (their values
//! are now fixed constants), and several overlapping settings were
//! consolidated into one:
//!
//! - `session.new_session_attach_mode` merged into
//!   `session.default_attach_mode` (one "Attach Mode" setting covering both
//!   the post-create attach and Enter/double-click activation).
//! - `session.conversation_summary_agent` merged into
//!   `session.smart_rename_agent` (one utility-agent setting for every
//!   one-shot call).
//!
//! Serde already ignores unknown keys on load, so this migration exists to
//! (a) carry a user's merged-away value over to the surviving key when the
//! surviving key was never set, and (b) tidy the dropped keys out of
//! config.toml so they don't linger as dead weight. Removals:
//!
//! - `[session]`: `new_session_attach_mode`, `conversation_summary_agent`,
//!   `smart_rename_timing`, `session_id_poller_max_threads`
//! - `[updates]`: `check_interval_hours`, `notify_in_cli`,
//!   `web_poll_interval_minutes`
//! - `[acp]`: `replay_bytes`, `max_concurrent_resumes`,
//!   `force_end_turn_threshold_secs`, `silent_orphan_fast_grace_secs`,
//!   `rate_limit_auto_resume_grace_secs`, `queue_drain_mode`
//! - `[status_hooks]`: `debounce_ms`
//! - `[sound]`: `mode` (per-state files + random fallback replace it)

use anyhow::{Context, Result};
use std::fs;
use std::path::PathBuf;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;

    let global_config = app_dir.join("config.toml");
    migrate_config_file(&global_config)?;

    let profiles_dir = app_dir.join("profiles");
    if profiles_dir.exists() {
        for entry in fs::read_dir(&profiles_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                let profile_config = entry.path().join("config.toml");
                migrate_config_file(&profile_config)?;
            }
        }
    }

    Ok(())
}

/// Keys dropped without a replacement, per section.
const DROPPED: &[(&str, &[&str])] = &[
    (
        "session",
        &["smart_rename_timing", "session_id_poller_max_threads"],
    ),
    (
        "updates",
        &[
            "check_interval_hours",
            "notify_in_cli",
            "web_poll_interval_minutes",
        ],
    ),
    (
        "acp",
        &[
            "replay_bytes",
            "max_concurrent_resumes",
            "force_end_turn_threshold_secs",
            "silent_orphan_fast_grace_secs",
            "rate_limit_auto_resume_grace_secs",
            "queue_drain_mode",
        ],
    ),
    ("status_hooks", &["debounce_ms"]),
    ("sound", &["mode"]),
];

/// Merged keys: (section, old key, surviving key). The old value is copied
/// to the surviving key only when the surviving key is absent (a value the
/// user explicitly set on the surviving key always wins), then removed.
const MERGED: &[(&str, &str, &str)] = &[
    ("session", "new_session_attach_mode", "default_attach_mode"),
    (
        "session",
        "conversation_summary_agent",
        "smart_rename_agent",
    ),
];

fn migrate_config_file(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        debug!("Config file {} does not exist, skipping", path.display());
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    let mut doc: toml::Table = content
        .parse()
        .with_context(|| format!("Failed to parse {} during v022 migration", path.display()))?;

    let mut changed = false;

    for (section, old_key, new_key) in MERGED {
        let Some(table) = doc.get_mut(*section).and_then(|s| s.as_table_mut()) else {
            continue;
        };
        let Some(old_value) = table.remove(*old_key) else {
            continue;
        };
        changed = true;
        // An empty-string utility agent means "same as session", the
        // default; carrying it over would be noise.
        let carries_meaning = old_value.as_str().map(|s| !s.is_empty()).unwrap_or(true);
        if !table.contains_key(*new_key) && carries_meaning {
            info!(
                "Carrying {section}.{old_key} = {old_value} over to {section}.{new_key} in {}",
                path.display()
            );
            table.insert((*new_key).to_string(), old_value);
        } else {
            info!(
                "Dropping superseded {section}.{old_key} from {}",
                path.display()
            );
        }
    }

    for (section, keys) in DROPPED {
        let Some(table) = doc.get_mut(*section).and_then(|s| s.as_table_mut()) else {
            continue;
        };
        for key in *keys {
            if table.remove(*key).is_some() {
                info!("Dropping removed {section}.{key} from {}", path.display());
                changed = true;
            }
        }
    }

    if !changed {
        debug!("No pruned settings in {}, skipping", path.display());
        return Ok(());
    }

    let new_content = toml::to_string_pretty(&doc)?;
    crate::session::atomic_write(path, new_content.as_bytes())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    fn migrated(content: &str) -> toml::Table {
        let (_dir, path) = write(content);
        migrate_config_file(&path).unwrap();
        fs::read_to_string(&path).unwrap().parse().unwrap()
    }

    #[test]
    fn attach_mode_carries_over_when_survivor_absent() {
        let result = migrated(
            r#"
[session]
new_session_attach_mode = "live_send"
"#,
        );
        let session = result["session"].as_table().unwrap();
        assert!(session.get("new_session_attach_mode").is_none());
        assert_eq!(
            session.get("default_attach_mode").and_then(|v| v.as_str()),
            Some("live_send")
        );
    }

    #[test]
    fn attach_mode_survivor_wins_over_merged_away_key() {
        let result = migrated(
            r#"
[session]
new_session_attach_mode = "live_send"
default_attach_mode = "tmux"
"#,
        );
        let session = result["session"].as_table().unwrap();
        assert!(session.get("new_session_attach_mode").is_none());
        assert_eq!(
            session.get("default_attach_mode").and_then(|v| v.as_str()),
            Some("tmux")
        );
    }

    #[test]
    fn summary_agent_carries_over_when_survivor_absent() {
        let result = migrated(
            r#"
[session]
conversation_summary_agent = "opencode"
"#,
        );
        let session = result["session"].as_table().unwrap();
        assert!(session.get("conversation_summary_agent").is_none());
        assert_eq!(
            session.get("smart_rename_agent").and_then(|v| v.as_str()),
            Some("opencode")
        );
    }

    #[test]
    fn empty_summary_agent_is_dropped_not_carried() {
        let result = migrated(
            r#"
[session]
conversation_summary_agent = ""
"#,
        );
        let session = result["session"].as_table().unwrap();
        assert!(session.get("conversation_summary_agent").is_none());
        assert!(session.get("smart_rename_agent").is_none());
    }

    #[test]
    fn dropped_keys_are_removed_and_others_kept() {
        let result = migrated(
            r#"
[session]
smart_rename_timing = "prompt_start"
session_id_poller_max_threads = 42
default_tool = "claude"

[updates]
update_check_mode = "off"
check_interval_hours = 12
notify_in_cli = false
web_poll_interval_minutes = 30

[acp]
default_agent = "claude-code"
replay_bytes = 1024
max_concurrent_resumes = 8
force_end_turn_threshold_secs = 45
silent_orphan_fast_grace_secs = 10
rate_limit_auto_resume_grace_secs = 20
queue_drain_mode = "serial"

[status_hooks]
enabled = true
debounce_ms = 500

[sound]
enabled = true
mode = { specific = "wololo" }
"#,
        );
        let session = result["session"].as_table().unwrap();
        assert!(session.get("smart_rename_timing").is_none());
        assert!(session.get("session_id_poller_max_threads").is_none());
        assert_eq!(
            session.get("default_tool").and_then(|v| v.as_str()),
            Some("claude")
        );

        let updates = result["updates"].as_table().unwrap();
        assert!(updates.get("check_interval_hours").is_none());
        assert!(updates.get("notify_in_cli").is_none());
        assert!(updates.get("web_poll_interval_minutes").is_none());
        assert_eq!(
            updates.get("update_check_mode").and_then(|v| v.as_str()),
            Some("off")
        );

        let acp = result["acp"].as_table().unwrap();
        for key in [
            "replay_bytes",
            "max_concurrent_resumes",
            "force_end_turn_threshold_secs",
            "silent_orphan_fast_grace_secs",
            "rate_limit_auto_resume_grace_secs",
            "queue_drain_mode",
        ] {
            assert!(acp.get(key).is_none(), "acp.{key} should be dropped");
        }
        assert_eq!(
            acp.get("default_agent").and_then(|v| v.as_str()),
            Some("claude-code")
        );

        let status_hooks = result["status_hooks"].as_table().unwrap();
        assert!(status_hooks.get("debounce_ms").is_none());
        assert_eq!(
            status_hooks.get("enabled").and_then(|v| v.as_bool()),
            Some(true)
        );

        let sound = result["sound"].as_table().unwrap();
        assert!(sound.get("mode").is_none());
        assert_eq!(sound.get("enabled").and_then(|v| v.as_bool()), Some(true));
    }

    #[test]
    fn clean_config_is_untouched() {
        let (_dir, path) = write(
            r#"
[session]
default_tool = "claude"
"#,
        );
        let before = fs::read_to_string(&path).unwrap();
        migrate_config_file(&path).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(
            before, after,
            "an already-clean config must not be rewritten"
        );
    }

    #[test]
    fn nonexistent_file_is_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("nonexistent.toml");
        migrate_config_file(&path).unwrap();
    }

    #[test]
    fn migration_is_idempotent() {
        let (_dir, path) = write(
            r#"
[session]
new_session_attach_mode = "live_send"
smart_rename_timing = "turn_end"
"#,
        );
        migrate_config_file(&path).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        migrate_config_file(&path).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
    }
}
