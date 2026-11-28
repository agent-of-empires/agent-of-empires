//! Migration v064: drop the `[sound] on_*` file overrides. Sounds are built in,
//! so the keys are ignored on load; this only stops them from lingering.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

const RETIRED: [&str; 6] = [
    "on_start",
    "on_running",
    "on_waiting",
    "on_idle",
    "on_error",
    "on_approval",
];

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    for path in config_file::all_configs(&app_dir)? {
        drop_file_overrides(&path)?;
    }
    Ok(())
}

fn drop_file_overrides(path: &Path) -> Result<()> {
    config_file::rewrite(path, |doc| {
        let Some(sound) = doc.get_mut("sound").and_then(toml::Value::as_table_mut) else {
            return false;
        };
        let before = sound.len();
        sound.retain(|key, _| !RETIRED.contains(&key));
        if sound.len() == before {
            return false;
        }
        if sound.is_empty() {
            doc.remove("sound");
        }
        info!(target: "migrations.v064", path = %path.display(), "dropped sound file overrides");
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn drops_file_overrides_and_keeps_other_sound_keys() {
        let kept = "[sound]\nenabled = true\n";
        assert_rewrites(
            "config.toml",
            drop_file_overrides,
            &[
                (
                    Some(
                        "[sound]\nenabled = true\non_idle = \"idle.wav\"\non_approval = \"gem\"\n",
                    ),
                    Some(kept),
                ),
                (
                    Some("[sound]\non_error = \"error\"\n\n[web]\nmcp_enabled = true\n"),
                    Some("[web]\nmcp_enabled = true\n"),
                ),
                (Some(kept), Some(kept)),
                (None, None),
            ],
        );
    }
}
