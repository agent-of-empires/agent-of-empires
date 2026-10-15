//! Migration v011: Relocate sandbox image from ghcr.io/njbrake to ghcr.io/agent-of-empires
//!
//! When the repo moved from `njbrake/agent-of-empires` to `agent-of-empires/agent-of-empires`
//! the published container images moved with it: `ghcr.io/njbrake/aoe-sandbox` and
//! `ghcr.io/njbrake/aoe-dev-sandbox` are republished as `ghcr.io/agent-of-empires/aoe-sandbox`
//! and `ghcr.io/agent-of-empires/aoe-dev-sandbox`. GHCR keeps the old paths alive as redirects
//! for now, but they should not be the canonical reference in stored config.
//!
//! This migration rewrites `[sandbox] default_image` in the global config and every profile
//! config to point at the new namespace. Idempotent: re-running on already-migrated configs
//! is a no-op.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

const OLD_NAMESPACE: &str = "ghcr.io/njbrake/";
const NEW_NAMESPACE: &str = "ghcr.io/agent-of-empires/";

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    for path in config_file::all_configs(&app_dir)? {
        migrate_config_file(&path)?;
    }
    Ok(())
}

fn migrate_config_file(path: &Path) -> Result<()> {
    config_file::rewrite(path, |doc| {
        let Some(sandbox) = doc.get_mut("sandbox").and_then(|s| s.as_table_mut()) else {
            return false;
        };
        let Some(value) = sandbox.get("default_image").and_then(|v| v.as_str()) else {
            return false;
        };
        let Some(rest) = value.strip_prefix(OLD_NAMESPACE) else {
            return false;
        };
        let new_value = format!("{NEW_NAMESPACE}{rest}");
        info!(
            "Relocating sandbox default_image: {} -> {} in {}",
            value,
            new_value,
            path.display()
        );
        sandbox.insert("default_image".to_string(), toml::Value::String(new_value));
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn relocates_only_the_old_sandbox_images() {
        let unchanged = |toml: &'static str| (Some(toml), Some(toml));
        assert_rewrites(
            "config.toml",
            migrate_config_file,
            &[
                (
                    Some("[sandbox]\ndefault_image = \"ghcr.io/njbrake/aoe-sandbox:latest\"\n"),
                    Some("[sandbox]\ndefault_image = \"ghcr.io/agent-of-empires/aoe-sandbox:latest\"\n"),
                ),
                (
                    Some("[sandbox]\ndefault_image = \"ghcr.io/njbrake/aoe-dev-sandbox:0.10\"\n"),
                    Some("[sandbox]\ndefault_image = \"ghcr.io/agent-of-empires/aoe-dev-sandbox:0.10\"\n"),
                ),
                unchanged("[sandbox]\ndefault_image = \"ghcr.io/agent-of-empires/aoe-sandbox:latest\"\n"),
                unchanged("[sandbox]\ndefault_image = \"docker.io/library/ubuntu:22.04\"\n"),
                unchanged("[session]\ndefault_tool = \"claude\"\n"),
                unchanged("[sandbox]\nenabled_by_default = true\n"),
                (None, None),
            ],
        );
    }
}
