//! Migration v070: Favorite folds into Pin, the one mark every Client honors (#5896).
//!
//! A row's `favorited_at` becomes its `pinned_at` unless it already has a pin or sits in
//! a state Pin cannot coexist with: archived, ignored, or snoozed. Trash keeps a pin, so a
//! trashed favorite is pinned too. An expired snooze is cleared as `pin()` clears it.
//! `favorited_at` is then dropped in the same write, so an interrupted run retries from
//! it. The `session.favorites_first` setting is removed from every config. A user who had
//! turned it off kept favorites as marks that never moved rows, so their favorites are
//! dropped rather than pinned. A `sessions.json` that cannot be read is left untouched,
//! the setting's answer is kept in `PENDING_FILE`, and `aoe migrate` converts the list
//! once it is repaired.

use super::config_file;
use super::sessions_file::{self, SessionList};
use crate::time::Timestamp;
use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;
use tracing::info;

const FAVORITED_AT: &str = "favorited_at";
/// Whether favorites become pins, kept while a skipped `sessions.json` still owes its
/// conversion.
const PENDING_FILE: &str = ".v070_favorites_pending";

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?, Pass::Upgrade)
}

/// Convert the favorites of profiles the upgrade skipped, once they read again.
pub(super) fn recover() -> Result<()> {
    run_in(&crate::session::get_app_dir()?, Pass::Recovery)
}

#[derive(Clone, Copy)]
enum Pass {
    Upgrade,
    Recovery,
}

fn run_in(app_dir: &Path, pass: Pass) -> Result<()> {
    let now = Timestamp::now();
    let pending_path = app_dir.join(PENDING_FILE);
    let pending = read_pending(&pending_path)?;
    // A pending choice outlives the setting, which an interrupted upgrade may have removed.
    let lift_favorites = match (pending, pass) {
        (Some(lift), _) => lift,
        (None, Pass::Recovery) => return Ok(()),
        (None, Pass::Upgrade) => favorites_first(app_dir)?,
    };
    let mut skipped = false;
    for path in sessions_file::session_files(app_dir)? {
        let content = match sessions_file::read_rows(&path, "v070") {
            SessionList::Rows(content, rows)
                if rows.iter().any(|row| row.get(FAVORITED_AT).is_some()) =>
            {
                content
            }
            SessionList::Rows(..) | SessionList::Missing => continue,
            SessionList::Unreadable => {
                skipped = true;
                continue;
            }
        };
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
        info!(target: "migrations.v070", path = %path.display(), healed, pinned, "folded favorites into pins");
    }
    if skipped {
        let choice = if lift_favorites { "pin" } else { "drop" };
        crate::session::atomic_write(&pending_path, choice.as_bytes())?;
    } else if pending.is_some() {
        fs::remove_file(&pending_path)?;
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

/// The choice an earlier run left in `PENDING_FILE`: `true` to pin favorites.
fn read_pending(path: &Path) -> Result<Option<bool>> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read {}", path.display()))
        }
    };
    match content.trim() {
        "pin" => Ok(Some(true)),
        "drop" => Ok(Some(false)),
        other => bail!("{} holds {other:?}, not pin or drop", path.display()),
    }
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
        .with_context(|| format!("Failed to parse {} during v070 migration", path.display()))?;
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

        run_in(dir.path(), Pass::Upgrade).unwrap();
        let first = fs::read_to_string(dir.path().join("profiles/main/sessions.json")).unwrap();
        // A second run over migrated state changes nothing.
        run_in(dir.path(), Pass::Upgrade).unwrap();
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
        run_in(dir.path(), Pass::Upgrade).unwrap();
        let row = &rows(dir.path())[0];
        assert!(row.get(FAVORITED_AT).is_none());
        assert!(row["pinned_at"].is_null());
        let global = fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(!global.contains("favorites_first"));
    }

    /// A list without favorites is not rewritten.
    #[test]
    fn lists_without_favorites_are_left_untouched() {
        let content = r#"[{"id":"a","pinned_at":null}]"#;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles/main/sessions.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        run_in(dir.path(), Pass::Upgrade).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
        assert!(!dir.path().join(PENDING_FILE).exists());
    }

    /// A list that cannot be read is left untouched while the rest migrate, and the
    /// favorites setting it still needs is kept for recovery, which converts the
    /// repaired list.
    #[test]
    fn an_unreadable_list_is_converted_once_repaired() {
        for (broken, favorites_first, expected) in [
            ("", "true", Some(FAV)),
            ("not json", "false", None),
            ("{}", "true", Some(FAV)),
        ] {
            let dir = tempfile::tempdir().unwrap();
            seed(dir.path(), &json!([{"id": "a", "favorited_at": FAV}]));
            fs::write(
                dir.path().join("config.toml"),
                format!("[session]\nfavorites_first = {favorites_first}\n"),
            )
            .unwrap();
            let path = dir.path().join("profiles/other/sessions.json");
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            if broken.is_empty() {
                // A directory in the file's place cannot be read but is not missing.
                fs::create_dir(&path).unwrap();
            } else {
                fs::write(&path, broken).unwrap();
            }

            run_in(dir.path(), Pass::Upgrade).unwrap();
            assert_eq!(
                rows(dir.path())[0]["pinned_at"].as_str(),
                expected,
                "{broken}"
            );
            assert!(broken.is_empty() || fs::read_to_string(&path).unwrap() == broken);
            let global = fs::read_to_string(dir.path().join("config.toml")).unwrap();
            assert!(!global.contains("favorites_first"), "{broken}");
            let pending = dir.path().join(PENDING_FILE);

            // Still unreadable, recovery waits; repaired, its favorites convert.
            run_in(dir.path(), Pass::Recovery).unwrap();
            assert!(pending.exists(), "{broken}");
            if path.is_dir() {
                fs::remove_dir(&path).unwrap();
            }
            fs::write(&path, json!([{"id": "b", "favorited_at": FAV}]).to_string()).unwrap();
            run_in(dir.path(), Pass::Recovery).unwrap();
            let repaired: Vec<Value> =
                serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            assert!(repaired[0].get(FAVORITED_AT).is_none(), "{broken}");
            assert_eq!(repaired[0]["pinned_at"].as_str(), expected, "{broken}");
            assert!(!pending.exists(), "{broken}");
            run_in(dir.path(), Pass::Recovery).unwrap();
        }
    }
}
