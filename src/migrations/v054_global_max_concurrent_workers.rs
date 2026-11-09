//! Migration v054: `acp.max_concurrent_workers` is global-only. Move the served
//! profile's value (the default profile's if none is recorded) to the global
//! config, since that value was the effective cap, and drop every other one.

use anyhow::{Context, Result};
use std::{fs, io::ErrorKind, path::Path};

use super::v030_global_only_profile_settings::promote_fields;
use crate::session::atomic_write;

pub(super) const WORKER_CAP_FIELD: &str = "acp.max_concurrent_workers";

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    promote_fields(app_dir, atomic_write, served_profile(app_dir)?, |field| {
        field.path() == WORKER_CAP_FIELD
    })
}

fn served_profile(app_dir: &Path) -> Result<Option<String>> {
    // A restart deletes `serve.launch` before migrating but retains the rollback copy.
    for name in ["serve.launch", "serve.rollback.launch"] {
        let path = app_dir.join(name);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
        };
        let record: Option<serde_json::Value> = serde_json::from_slice(&bytes).ok();
        if let Some(profile) = record
            .as_ref()
            .and_then(|record| record.get("profile"))
            .and_then(serde_json::Value::as_str)
        {
            return Ok(Some(profile.to_owned()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::run_in;
    use std::{fs, path::Path};

    fn seed(app: &Path, path: &str, contents: &str) {
        let path = app.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn read(app: &Path, path: &str) -> toml::Table {
        fs::read_to_string(app.join(path)).unwrap().parse().unwrap()
    }

    fn seed_profiles(app: &Path) {
        seed(
            app,
            "config.toml",
            "default_profile = 'work'\n[acp]\nmax_concurrent_workers = 100\n",
        );
        seed(
            app,
            "profiles/work/config.toml",
            "[acp]\nmax_concurrent_workers = 3\nreplay_events = 9\n[theme]\nname = 'dracula'\n",
        );
        seed(
            app,
            "profiles/other/config.toml",
            "[acp]\nmax_concurrent_workers = 1\n",
        );
        seed(app, "profiles/bare/config.toml", "");
    }

    #[test]
    #[serial_test::serial]
    fn upgrade_across_v030_keeps_the_served_profile_cap() {
        // (recorded served profile, expected global cap)
        for (served, expected) in [(Some("other"), 1), (Some("bare"), 100), (None, 3)] {
            let temp = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let app = crate::session::get_app_dir().unwrap();
            let app = app.as_path();
            seed_profiles(app);
            seed(app, ".schema_version", "29");
            if let Some(profile) = served {
                let launch = serde_json::json!({
                    "schema": 2,
                    "pid": 42,
                    "instance_id": "retained-instance",
                    "profile": profile,
                    "auth_mode": "token",
                    "remote": false,
                });
                seed(app, "serve.launch", &launch.to_string());
            }

            crate::migrations::run_migrations_announced(None).unwrap();

            let global = read(app, "config.toml");
            assert_eq!(
                global["acp"]["max_concurrent_workers"].as_integer(),
                Some(expected),
                "{served:?}"
            );
            // v030 still promotes its own fields from the default profile.
            assert_eq!(global["theme"]["name"].as_str(), Some("dracula"));
            assert_eq!(
                read(app, "profiles/work/config.toml"),
                "[acp]\nreplay_events = 9\n".parse::<toml::Table>().unwrap()
            );
            assert!(read(app, "profiles/other/config.toml").is_empty());
        }
    }

    #[test]
    fn promotes_the_effective_cap_and_drops_the_rest() {
        const LIVE: &str = "serve.launch";
        const ROLLBACK: &str = "serve.rollback.launch";
        let served = |name: &str| format!(r#"{{"profile":"{name}"}}"#);
        // (launch records, expected global cap)
        for (records, expected) in [
            (vec![], 3),
            (vec![(LIVE, served("other"))], 1),
            (vec![(ROLLBACK, served("other"))], 1),
            (vec![(LIVE, served("other")), (ROLLBACK, served("work"))], 1),
            (
                vec![(LIVE, "{".to_string()), (ROLLBACK, served("other"))],
                1,
            ),
            (vec![(LIVE, served("bare"))], 100),
            (vec![(LIVE, served("deleted"))], 3),
            (vec![(LIVE, "{".to_string())], 3),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let app = dir.path();
            seed_profiles(app);
            for (name, contents) in &records {
                seed(app, name, contents);
            }

            for _ in 0..2 {
                run_in(app).unwrap();
                assert_eq!(
                    read(app, "config.toml")["acp"]["max_concurrent_workers"].as_integer(),
                    Some(expected),
                    "{records:?}"
                );
                assert_eq!(
                    read(app, "profiles/work/config.toml"),
                    "[acp]\nreplay_events = 9\n[theme]\nname = 'dracula'\n"
                        .parse::<toml::Table>()
                        .unwrap()
                );
                assert!(read(app, "profiles/other/config.toml").is_empty());
            }
        }
    }
}
