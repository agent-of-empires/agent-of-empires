//! Migration v054: `acp.max_concurrent_workers` is global-only. Move the served
//! profile's value (the default profile's if none is recorded) to the global
//! config, since that value was the effective cap, and drop every other one.

use anyhow::{Context, Result};
use std::{fs, io::ErrorKind, path::Path};

use super::v030_global_only_profile_settings::promote_fields;
use crate::session::atomic_write;

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    promote_fields(app_dir, atomic_write, served_profile(app_dir)?, |field| {
        field.path() == "acp.max_concurrent_workers"
    })
}

fn served_profile(app_dir: &Path) -> Result<Option<String>> {
    let path = app_dir.join("serve.launch");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("Read {}", path.display())),
    };
    let record: Option<serde_json::Value> = serde_json::from_slice(&bytes).ok();
    Ok(record
        .as_ref()
        .and_then(|record| record.get("profile"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned))
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

    #[test]
    fn promotes_the_effective_cap_and_drops_the_rest() {
        // (serve.launch contents, expected global cap)
        for (launch, expected) in [
            (None, 3),
            (Some(r#"{"profile":"other"}"#), 1),
            (Some(r#"{"profile":"bare"}"#), 100),
            (Some(r#"{"profile":"deleted"}"#), 3),
            (Some("{"), 3),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let app = dir.path();
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
            if let Some(launch) = launch {
                seed(app, "serve.launch", launch);
            }

            for _ in 0..2 {
                run_in(app).unwrap();
                assert_eq!(
                    read(app, "config.toml")["acp"]["max_concurrent_workers"].as_integer(),
                    Some(expected),
                    "{launch:?}"
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
