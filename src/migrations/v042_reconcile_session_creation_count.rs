//! Seed creation counts for installs that already advanced past upstream v036.

pub fn run() -> anyhow::Result<()> {
    super::v036_session_creation_count::run()
}

#[cfg(test)]
mod tests {
    use crate::session::config::{update_app_state, AppStateConfig, SortOrder};

    #[test]
    #[serial_test::serial]
    fn upstream_and_native_upgrades_preserve_state_and_initialize_purge_ownership(
    ) -> anyhow::Result<()> {
        for (version, seeded, count, expected_count) in [(36, true, 19, 19), (40, false, 0, 2)] {
            let temp = tempfile::tempdir()?;
            let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
            let app = crate::session::get_app_dir()?;
            std::fs::write(app.join(".schema_version"), version.to_string())?;
            let profile = app.join("profiles/default");
            std::fs::create_dir_all(&profile)?;
            std::fs::write(
                profile.join("sessions.json"),
                r#"[{"id":"retained"},{"id":"trashed","trashed_at":"2026-01-01"}]"#,
            )?;
            update_app_state(|state| {
                state.sort_order = Some(SortOrder::Oldest);
                state.sessions_created_seeded = seeded;
                state.sessions_created = count;
                state.tips_seen.push("retained-tip".into());
            })?;
            let journal_path = app.join(crate::session::purge_owners::FILE_NAME);
            let existing_journal = if version == 40 {
                use crate::session::purge_owners::{PurgeCapture, PurgeOwner};
                crate::session::purge_owners::initialize(&app)?;
                let storage = crate::session::Storage::open_unwatched("default")?;
                let mut pending =
                    crate::session::Instance::new("pending purge", temp.path().to_str().unwrap());
                pending.source_profile = "default".into();
                pending.lifecycle_generation = 7;
                let capture = PurgeCapture::new(&pending)?;
                drop(PurgeOwner::record(&storage, &pending, capture)?);
                Some(std::fs::read(&journal_path)?)
            } else {
                None
            };

            crate::migrations::run_migrations()?;

            let state = AppStateConfig::load()?;
            assert_eq!(state.sessions_created, expected_count, "schema {version}");
            assert!(state.sessions_created_seeded);
            assert_eq!(state.sort_order, Some(SortOrder::Oldest));
            assert_eq!(state.tips_seen, ["retained-tip"]);
            let journal = std::fs::read(&journal_path)?;
            match existing_journal {
                Some(existing) => assert_eq!(journal, existing),
                None => {
                    let storage = crate::session::Storage::open_unwatched("default")?;
                    assert!(crate::session::purge_owners::protection(&storage, None)?.is_empty());
                }
            }
            let state_bytes = std::fs::read(app.join("state.toml"))?;
            crate::migrations::run_migrations()?;
            assert_eq!(std::fs::read(app.join("state.toml"))?, state_bytes);
            assert_eq!(std::fs::read(&journal_path)?, journal);
        }
        Ok(())
    }
}
