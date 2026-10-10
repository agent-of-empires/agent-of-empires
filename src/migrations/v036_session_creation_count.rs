//! Seed the local creation counter from retained sessions on upgrade.

use std::collections::HashSet;

use anyhow::Result;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    let mut ids = HashSet::new();
    for path in super::sessions_file::session_files(&app_dir)? {
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let rows: Vec<serde_json::Value> = match serde_json::from_str(&content) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "Skipping unreadable sessions while seeding tip count");
                continue;
            }
        };
        for row in rows {
            if row.get("status").and_then(|s| s.as_str()) == Some("creating") {
                continue;
            }
            if let Some(id) = row.get("id").and_then(|id| id.as_str()) {
                if !id.is_empty() {
                    ids.insert(id.to_owned());
                }
            }
        }
    }
    // Archives and trash retain their rows; purged history cannot be recovered.
    let seeded = crate::session::config::update_app_state(|state| {
        if state.sessions_created_seeded {
            return false;
        }
        state.sessions_created = state.sessions_created.max(ids.len() as u64);
        state.sessions_created_seeded = true;
        true
    })?;
    if seeded {
        tracing::info!(
            "v036: seeded session creation count from {} retained sessions",
            ids.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::config::{update_app_state, AppStateConfig};

    #[test]
    #[serial_test::serial]
    fn seeds_all_profiles_without_counting_placeholders_or_duplicates() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app_dir = crate::session::get_app_dir().unwrap();
        for (profile, rows) in [
            (
                "default",
                r#"[{"id":"active"},{"id":"archive","archived_at":"2026-01-01"}]"#,
            ),
            (
                "work",
                r#"[{"id":"trash","trashed_at":"2026-01-01"},{"id":"active"},{"id":"stub","status":"creating"}]"#,
            ),
        ] {
            let dir = app_dir.join("profiles").join(profile);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("sessions.json"), rows).unwrap();
        }
        update_app_state(|state| state.tips_seen.push("existing-tip".into())).unwrap();
        run().unwrap();
        run().unwrap();
        let state = AppStateConfig::load().unwrap();
        assert_eq!(state.sessions_created, 3);
        assert_eq!(state.tips_seen, ["existing-tip"]);
        // A late migrator can observe a newly persisted row before its counter increment.
        std::fs::write(
            app_dir.join("profiles/work/sessions.json"),
            r#"[{"id":"trash"},{"id":"active"},{"id":"new"}]"#,
        )
        .unwrap();
        run().unwrap();
        assert_eq!(AppStateConfig::load().unwrap().sessions_created, 3);
        crate::tips::record_session_creations(1);
        assert_eq!(AppStateConfig::load().unwrap().sessions_created, 4);
        update_app_state(|state| state.sessions_created = 40).unwrap();
        run().unwrap();
        assert_eq!(AppStateConfig::load().unwrap().sessions_created, 40);
    }
}
