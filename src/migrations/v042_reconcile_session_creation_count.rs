//! Seed creation counts for installs that already advanced past upstream v036.

pub fn run() -> anyhow::Result<()> {
    super::v036_session_creation_count::run()
}

#[cfg(test)]
mod tests {
    use crate::session::config::{AppStateConfig, GroupByMode, SortOrder};

    #[test]
    #[serial_test::serial]
    fn upstream_and_native_upgrades_preserve_state_and_initialize_purge_ownership(
    ) -> anyhow::Result<()> {
        for (version, seeded, count, expected_count) in
            [(0, false, 0, 2), (36, true, 19, 19), (40, false, 0, 2)]
        {
            let temp = tempfile::tempdir()?;
            let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
            let _probes =
                crate::migrations::v033_isolate_sandbox_content::install_test_reconcile_probes(
                    |_| Ok(false),
                    |_| Ok(true),
                    |_| Ok(Vec::new()),
                );
            let app = crate::session::get_app_dir()?;
            std::fs::write(app.join(".schema_version"), version.to_string())?;
            let profile = app.join("profiles/default");
            std::fs::create_dir_all(&profile)?;
            // From schema 0 a sandboxed row makes v033 resolve the profile's Config.
            let retained = if version == 0 {
                r#"{"id":"retained","tool":"claude","sandbox_info":{"enabled":true,"image":"fixture","container_name":"aoe-sandbox-fixture"}}"#
            } else {
                r#"{"id":"retained"}"#
            };
            std::fs::write(
                profile.join("sessions.json"),
                format!(r#"[{retained},{{"id":"trashed","trashed_at":"2026-01-01"}}]"#),
            )?;
            // Values this build no longer decodes must survive until v067 rewrites them. Before
            // v021 the state was the `[app_state]` table of config.toml.
            let state = format!(
                "sort_order = \"a_z\"\ngroup_by = \"manual\"\nsessions_created_seeded = {seeded}\nsessions_created = {count}\ntips_seen = [\"retained-tip\"]\n"
            );
            if version < 21 {
                std::fs::write(app.join("config.toml"), format!("[app_state]\n{state}"))?;
            } else {
                std::fs::write(app.join("state.toml"), state)?;
            }
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
            assert_eq!(state.sort_order, Some(SortOrder::Name));
            assert_eq!(state.group_by, Some(GroupByMode::Group));
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
