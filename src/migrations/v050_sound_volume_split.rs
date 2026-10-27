//! Migration v050: split `[sound] volume` into `server_volume` and
//! `browser_volume`. The browser plays at the old value capped at 1.0, as it
//! always did, so playback does not change.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    for path in config_file::all_configs(&app_dir)? {
        split_volume(&path)?;
    }
    Ok(())
}

fn split_volume(path: &Path) -> Result<()> {
    config_file::rewrite_strict(path, "v050", |doc| {
        let Some(sound) = doc.get_mut("sound").and_then(|s| s.as_table_mut()) else {
            return false;
        };
        let Some(old) = sound.remove("volume") else {
            return false;
        };
        let volume = old
            .as_float()
            .or_else(|| old.as_integer().map(|i| i as f64));
        // A destination key already present wins over the migrated value.
        if let Some(v) = volume {
            sound
                .entry("server_volume")
                .or_insert(toml::Value::Float(v));
            sound
                .entry("browser_volume")
                .or_insert(toml::Value::Float(v.min(1.0)));
        }
        info!(
            "Splitting sound volume into server and browser in {}",
            path.display()
        );
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn splits_volume_and_keeps_existing_destinations() {
        assert_rewrites(
            "config.toml",
            split_volume,
            &[
                (
                    Some("[sound]\nenabled = true\nvolume = 1.2\n"),
                    Some("[sound]\nenabled = true\nserver_volume = 1.2\nbrowser_volume = 1.0\n"),
                ),
                (
                    Some("[sound]\nvolume = 0.4\n"),
                    Some("[sound]\nserver_volume = 0.4\nbrowser_volume = 0.4\n"),
                ),
                (
                    Some("[sound]\nvolume = 0.4\nbrowser_volume = 0.9\n"),
                    Some("[sound]\nserver_volume = 0.4\nbrowser_volume = 0.9\n"),
                ),
                (
                    Some("[sound]\nenabled = true\n"),
                    Some("[sound]\nenabled = true\n"),
                ),
                (
                    Some("[session]\ndefault_tool = \"claude\"\n"),
                    Some("[session]\ndefault_tool = \"claude\"\n"),
                ),
                (None, None),
            ],
        );
    }
}
