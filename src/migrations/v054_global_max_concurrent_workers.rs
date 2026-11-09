//! Migration v054: `acp.max_concurrent_workers` is global-only. Move the default
//! profile's value to the global config and drop it from every other profile.

use anyhow::Result;
use std::path::Path;

use super::v030_global_only_profile_settings::promote_fields;
use crate::session::atomic_write;

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    promote_fields(app_dir, atomic_write, |field| {
        field.path() == "acp.max_concurrent_workers"
    })
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
    fn promotes_the_default_profile_cap_and_drops_the_rest() {
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

        for _ in 0..2 {
            run_in(app).unwrap();
            assert_eq!(
                read(app, "config.toml")["acp"]["max_concurrent_workers"].as_integer(),
                Some(3)
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
