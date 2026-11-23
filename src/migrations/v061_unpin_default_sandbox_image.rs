//! Migration v061: unpin a stored `[sandbox] default_image` that is exactly the
//! old built-in default, so it resolves to the image tagged with the running
//! AoE version. Earlier builds wrote that default on every save, and a local
//! `:latest` is never re-pulled. Any other value is a user choice and stays.
//!
//! The global config drops the key. A profile gets `""` instead, which also
//! means automatic: removing it would make the profile inherit a custom global
//! image it had overridden.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

const OLD_DEFAULT_IMAGE: &str = "ghcr.io/agent-of-empires/aoe-sandbox:latest";

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    migrate_config_file(&app_dir.join("config.toml"), Scope::Global)?;
    for path in config_file::profile_configs(app_dir)? {
        migrate_config_file(&path, Scope::Profile)?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Scope {
    Global,
    Profile,
}

fn migrate_config_file(path: &Path, scope: Scope) -> Result<()> {
    config_file::rewrite(path, |doc| {
        let Some(sandbox) = doc.get_mut("sandbox").and_then(toml::Value::as_table_mut) else {
            return false;
        };
        if sandbox.get("default_image").and_then(toml::Value::as_str) != Some(OLD_DEFAULT_IMAGE) {
            return false;
        }
        match scope {
            Scope::Global => {
                sandbox.remove("default_image");
            }
            Scope::Profile => {
                sandbox.insert("default_image".into(), String::new().into());
            }
        }
        info!(
            target: "migrations.v061",
            "unpinned sandbox default_image {OLD_DEFAULT_IMAGE} in {}",
            path.display()
        );
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;
    use crate::session::config::profile_config::{merge_configs, ProfileConfig};
    use crate::session::Config;
    use std::fs;

    const OLD: &str =
        "[sandbox]\ndefault_image = \"ghcr.io/agent-of-empires/aoe-sandbox:latest\"\n";

    #[test]
    fn unpins_only_the_old_default_image() {
        let unchanged = |toml: &'static str| (Some(toml), Some(toml));
        let kept = [
            unchanged(
                "[sandbox]\ndefault_image = \"ghcr.io/agent-of-empires/aoe-sandbox:1.18.0\"\n",
            ),
            unchanged(
                "[sandbox]\ndefault_image = \"ghcr.io/agent-of-empires/aoe-dev-sandbox:latest\"\n",
            ),
            unchanged("[sandbox]\ndefault_image = \"ghcr.io/njbrake/aoe-sandbox:latest\"\n"),
            unchanged("[sandbox]\ndefault_image = \"docker.io/example/aoe-sandbox:latest\"\n"),
            unchanged("[sandbox]\ndefault_image = \"my-sandbox:latest\"\n"),
            unchanged("[sandbox]\ndefault_image = \"\"\n"),
            unchanged("[sandbox]\nenabled_by_default = true\n"),
            unchanged("[session]\ndefault_tool = \"claude\"\n"),
            (None, None),
        ];
        for (scope, unpinned) in [
            (Scope::Global, "[sandbox]\nmount_ssh = true\n"),
            (
                Scope::Profile,
                "[sandbox]\ndefault_image = \"\"\nmount_ssh = true\n",
            ),
        ] {
            let mut cases = vec![(
                Some("[sandbox]\ndefault_image = \"ghcr.io/agent-of-empires/aoe-sandbox:latest\"\nmount_ssh = true\n"),
                Some(unpinned),
            )];
            cases.extend(kept);
            assert_rewrites(
                "config.toml",
                |path| migrate_config_file(path, scope),
                &cases,
            );
        }
    }

    /// What each profile resolves to must not change, except that the old
    /// default becomes the automatic image.
    #[test]
    fn profiles_resolve_as_before_with_the_old_default_unpinned() {
        let automatic = crate::containers::sandbox_image::automatic_sandbox_image();
        let custom = "[sandbox]\ndefault_image = \"ghcr.io/example/custom:1\"\n";
        let other = "[sandbox]\ndefault_image = \"ghcr.io/example/profile:2\"\n";
        for (global, profile, expected) in [
            (custom, OLD, automatic),
            (OLD, OLD, automatic),
            (OLD, "", automatic),
            (custom, "", "ghcr.io/example/custom:1"),
            (OLD, other, "ghcr.io/example/profile:2"),
            (custom, other, "ghcr.io/example/profile:2"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let profile_path = dir.path().join("profiles/work/config.toml");
            fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
            fs::write(dir.path().join("config.toml"), global).unwrap();
            fs::write(&profile_path, profile).unwrap();

            for _ in 0..2 {
                run_in(dir.path()).unwrap();
                let migrated_global: Config =
                    toml::from_str(&fs::read_to_string(dir.path().join("config.toml")).unwrap())
                        .unwrap();
                let migrated_profile: ProfileConfig =
                    toml::from_str(&fs::read_to_string(&profile_path).unwrap()).unwrap();
                assert_eq!(
                    merge_configs(migrated_global, &migrated_profile)
                        .sandbox
                        .effective_image(),
                    expected,
                    "global {global:?}, profile {profile:?}"
                );
            }
        }
    }
}
