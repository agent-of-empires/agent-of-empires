//! Migration v067: Favorite folds into Pin, the one mark every Client honors (#5896).
//!
//! A row's `favorited_at` becomes its `pinned_at` unless it already has a pin or sits in
//! a state Pin cannot coexist with: archived, ignored, or snoozed. Trash keeps a pin, so a
//! trashed favorite is pinned too. An expired snooze is cleared as `pin()` clears it.
//! `favorited_at` is then dropped in the same write, so an interrupted run retries from
//! it. The `session.favorites_first` setting is removed from every config. A user who had
//! turned it off kept favorites as marks that never moved rows, so their favorites are
//! dropped rather than pinned.

use super::{config_file, sessions_file};
use crate::time::Timestamp;
use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;
use tracing::{info, warn};

const FAVORITED_AT: &str = "favorited_at";

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    let now = Timestamp::now();
    let lift_favorites = favorites_first(app_dir)?;
    for path in sessions_file::session_files(app_dir)? {
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                warn!(target: "migrations.v067", path = %path.display(), %error, "skipping an unreadable session list");
                continue;
            }
        };
        let has_favorites = serde_json::from_str::<Value>(&content)
            .ok()
            .and_then(|document| document.as_array().cloned())
            .is_some_and(|rows| rows.iter().any(|row| row.get(FAVORITED_AT).is_some()));
        if !has_favorites {
            continue;
        }
        crate::session::backup_before_migration(&path)
            .with_context(|| format!("Failed to back up {}", path.display()))?;
        let mut pinned = 0usize;
        let healed = sessions_file::heal_rows(&path, &content, |row| {
            let Some(favorited) = row.remove(FAVORITED_AT) else {
                return false;
            };
            if lift_favorites && !favorited.is_null() && can_pin(row, now) {
                row.insert("pinned_at".into(), favorited);
                row.remove("snoozed_until");
                pinned += 1;
            }
            true
        })?;
        info!(target: "migrations.v067", path = %path.display(), healed, pinned, "folded favorites into pins");
    }
    for path in config_file::all_configs(app_dir)? {
        config_file::rewrite(&path, |doc| {
            doc.get_mut("session")
                .and_then(toml::Value::as_table_mut)
                .is_some_and(|session| session.remove("favorites_first").is_some())
        })?;
    }
    Ok(())
}

/// The global `session.favorites_first`, on unless explicitly set to false. It is
/// `global_only`, so profile configs never override it.
fn favorites_first(app_dir: &Path) -> Result<bool> {
    let path = app_dir.join("config.toml");
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    let config: toml::Table = content
        .parse()
        .with_context(|| format!("Failed to parse {} during v067 migration", path.display()))?;
    Ok(config
        .get("session")
        .and_then(|session| session.get("favorites_first"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true))
}

/// Whether Pin's rules allow pinning `row`: no pin yet, and not archived, ignored or
/// snoozed. A snooze that does not parse counts as active.
fn can_pin(row: &Map<String, Value>, now: Timestamp) -> bool {
    let set = |key: &str| row.get(key).is_some_and(|v| !v.is_null());
    let snoozed = row
        .get("snoozed_until")
        .filter(|v| !v.is_null())
        .is_some_and(|v| {
            v.as_str()
                .and_then(|raw| Timestamp::parse_rfc3339(raw).ok())
                .is_none_or(|until| until > now)
        });
    !set("pinned_at") && !sessions_file::is_archived(row) && !set("ignored_at") && !snoozed
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const FAV: &str = "2026-01-01T00:00:00Z";
    const PIN: &str = "2026-02-01T00:00:00Z";

    fn seed(dir: &Path, rows: &Value) {
        let profile = dir.join("profiles/main");
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("sessions.json"), rows.to_string()).unwrap();
    }

    fn rows(dir: &Path) -> Vec<Value> {
        let path = dir.join("profiles/main/sessions.json");
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn favorites_become_pins_where_pin_allows() {
        let future = (Timestamp::now() + crate::time::Duration::days(1)).to_rfc3339();
        // (row extras, expected pinned_at, expected snoozed_until)
        let cases: Vec<(Value, Option<&str>, Option<String>)> = vec![
            (json!({"favorited_at": FAV}), Some(FAV), None),
            (json!({"favorited_at": null}), None, None),
            (json!({}), None, None),
            (
                json!({"favorited_at": FAV, "pinned_at": PIN}),
                Some(PIN),
                None,
            ),
            (json!({"favorited_at": FAV, "archived_at": PIN}), None, None),
            (json!({"favorited_at": FAV, "ignored_at": PIN}), None, None),
            (
                json!({"favorited_at": FAV, "snoozed_until": future}),
                None,
                Some(future.clone()),
            ),
            (
                json!({"favorited_at": FAV, "snoozed_until": "2000-01-01T00:00:00Z"}),
                Some(FAV),
                None,
            ),
            (
                json!({"favorited_at": FAV, "snoozed_until": "garbled"}),
                None,
                Some("garbled".into()),
            ),
            (
                json!({"favorited_at": FAV, "trashed_at": PIN}),
                Some(FAV),
                None,
            ),
        ];
        let dir = tempfile::tempdir().unwrap();
        let rows_in: Vec<Value> = cases
            .iter()
            .enumerate()
            .map(|(i, (extra, _, _))| {
                let mut row = json!({"id": i.to_string(), "title": "t"});
                for (k, v) in extra.as_object().unwrap() {
                    row[k] = v.clone();
                }
                row
            })
            .collect();
        seed(dir.path(), &Value::Array(rows_in));
        fs::write(
            dir.path().join("config.toml"),
            "[session]\nfavorites_first = true\nunread_indicator = true\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("profiles/main/config.toml"),
            "[session]\nfavorites_first = true\n",
        )
        .unwrap();

        run_in(dir.path()).unwrap();
        let first = fs::read_to_string(dir.path().join("profiles/main/sessions.json")).unwrap();
        // A second run over migrated state changes nothing.
        run_in(dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("profiles/main/sessions.json")).unwrap(),
            first
        );

        let migrated = rows(dir.path());
        for (i, (extra, pinned, snoozed)) in cases.iter().enumerate() {
            let row = &migrated[i];
            assert!(row.get(FAVORITED_AT).is_none(), "{extra}");
            assert_eq!(row["pinned_at"].as_str(), *pinned, "{extra}");
            assert_eq!(row["snoozed_until"].as_str(), snoozed.as_deref(), "{extra}");
        }
        let global = fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(!global.contains("favorites_first"), "{global}");
        assert!(global.contains("unread_indicator"), "{global}");
        let profile = fs::read_to_string(dir.path().join("profiles/main/config.toml")).unwrap();
        assert!(!profile.contains("favorites_first"), "{profile}");
        let backups = fs::read_dir(dir.path().join("profiles/main"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("sessions.json.pre-migration-")
            })
            .count();
        assert_eq!(backups, 1, "backed up once, before the first rewrite");
    }

    /// A list without favorites is not rewritten, and one that does not parse is left for
    /// its recovery.
    /// Favorites that never moved rows stay that way: with the setting off they are
    /// dropped, not pinned, and the setting goes too.
    #[test]
    fn favorites_first_off_drops_favorites_without_pinning() {
        let dir = tempfile::tempdir().unwrap();
        seed(
            dir.path(),
            &serde_json::json!([{"id": "a", "favorited_at": FAV, "pinned_at": null}]),
        );
        fs::write(
            dir.path().join("config.toml"),
            "[session]\nfavorites_first = false\n",
        )
        .unwrap();
        run_in(dir.path()).unwrap();
        let row = &rows(dir.path())[0];
        assert!(row.get(FAVORITED_AT).is_none());
        assert!(row["pinned_at"].is_null());
        let global = fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(!global.contains("favorites_first"));
    }

    #[test]
    fn lists_without_favorites_are_left_untouched() {
        for content in [r#"[{"id":"a","pinned_at":null}]"#, "not json", "{}"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("profiles/main/sessions.json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, content).unwrap();
            run_in(dir.path()).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), content, "{content}");
        }
    }
}
