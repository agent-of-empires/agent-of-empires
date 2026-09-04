//! Migration v026: repoint a persisted `acp.default_agent = "aoe-agent"`.
//!
//! `aoe-agent` is not packaged: nothing builds the registry's
//! `${aoe_data_dir}/acp-worker/dist/aoe-agent` command (#3553). It was
//! nonetheless the compiled default, v005 seeded it into `[cockpit]` (renamed
//! to `[acp]` by v012), and `update_config` re-serializes the whole `Config` on
//! every write, so nearly every existing install carries an explicit
//! `default_agent = "aoe-agent"` in its `config.toml`. Now that the setting
//! actually selects the spawned agent, that persisted value would outrank the
//! new `claude-code` default forever and pick an agent that cannot start.
//!
//! Only the seeded value is rewritten; any other name is a real choice.
//!
//! Profile configs are deliberately untouched. They are sparse, holding only
//! the keys a user actually overrode, so an `aoe-agent` there is a decision.
//! Repo configs cannot carry `[acp]` at all (see `repo_config`).

use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir.join("config.toml"))
}

/// Inner body so the test can drive the migration end-to-end against a temp
/// file instead of inlining a near-copy of the production logic.
pub(crate) fn run_in(path: &Path) -> Result<()> {
    if !path.exists() {
        debug!("No global config.toml, nothing to repoint for v026");
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    // A config that does not parse is skipped rather than aborting startup:
    // this migration is a default correction, not a load-bearing relocation.
    let mut doc: toml::Table = match content.parse() {
        Ok(table) => table,
        Err(e) => {
            debug!("failed to parse {}: {e}, skipping", path.display());
            return Ok(());
        }
    };

    let Some(acp) = doc.get_mut("acp").and_then(|s| s.as_table_mut()) else {
        return Ok(());
    };
    if acp.get("default_agent").and_then(|v| v.as_str()) != Some("aoe-agent") {
        return Ok(());
    }
    acp.insert(
        "default_agent".into(),
        crate::session::config::DEFAULT_ACP_AGENT.into(),
    );

    info!(
        "v026: repointed acp.default_agent away from the unpackaged aoe-agent in {}",
        path.display()
    );
    let new_content = toml::to_string_pretty(&doc)?;
    crate::session::atomic_write(path, new_content.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    fn default_agent_after(content: &str) -> Option<String> {
        let (_dir, path) = write(content);
        run_in(&path).unwrap();
        let doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        Some(
            doc.get("acp")?
                .as_table()?
                .get("default_agent")?
                .as_str()?
                .to_string(),
        )
    }

    #[test]
    fn rewrites_only_the_seeded_aoe_agent() {
        let cases = [
            // The serialized old default: the case this migration exists for.
            (
                "[acp]\ndefault_agent = \"aoe-agent\"\n",
                Some("claude-code"),
            ),
            // A deliberate choice of any other agent survives.
            ("[acp]\ndefault_agent = \"codex\"\n", Some("codex")),
            // Already repointed, by an earlier run or a fresh install.
            (
                "[acp]\ndefault_agent = \"claude-code\"\n",
                Some("claude-code"),
            ),
            // An absent key already resolves to the new default.
            ("[acp]\nreplay_events = 0\n", None),
            // No [acp] table at all.
            ("[theme]\nname = \"empire\"\n", None),
        ];
        for (content, expected) in cases {
            assert_eq!(
                default_agent_after(content).as_deref(),
                expected,
                "{content:?}"
            );
        }
    }

    #[test]
    fn preserves_other_settings_and_is_idempotent() {
        let (_dir, path) = write(
            "[acp]\ndefault_agent = \"aoe-agent\"\nmax_concurrent_workers = 5\n\n\
             [theme]\nname = \"rose-pine\"\n",
        );

        run_in(&path).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        let doc: toml::Table = first.parse().unwrap();
        let acp = doc.get("acp").unwrap().as_table().unwrap();
        assert_eq!(
            acp.get("default_agent").and_then(|v| v.as_str()),
            Some("claude-code")
        );
        assert_eq!(
            acp.get("max_concurrent_workers")
                .and_then(|v| v.as_integer()),
            Some(5),
            "sibling acp settings must survive the rewrite"
        );
        assert_eq!(
            doc.get("theme")
                .and_then(|t| t.as_table())
                .and_then(|t| t.get("name"))
                .and_then(|v| v.as_str()),
            Some("rose-pine"),
            "unrelated sections must survive the rewrite"
        );

        run_in(&path).unwrap();
        assert_eq!(
            first,
            fs::read_to_string(&path).unwrap(),
            "a second run must not rewrite the file"
        );
    }

    /// A missing or unparsable config is skipped, never a startup-aborting
    /// error: this migration corrects a default, so it must not brick boot.
    #[test]
    fn unusable_config_is_a_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(run_in(&dir.path().join("nope.toml")).is_ok());

        let (_dir, path) = write("this is not = = toml");
        assert!(run_in(&path).is_ok());
    }
}
