//! Migration v026: repoint a persisted `acp.default_agent = "aoe-agent"` to
//! the current default.
//!
//! Nothing packages or builds the `aoe-agent` command (#3553), but it was the
//! compiled default and `update_config` re-serializes every key, so nearly
//! every install carries it explicitly and would keep selecting an agent that
//! cannot start. Any other name is a real choice, and profile configs are
//! sparse so they stay untouched.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir.join("config.toml"))
}

/// Inner body so the test can drive the migration end-to-end against a temp
/// file instead of inlining a near-copy of the production logic.
pub(crate) fn run_in(path: &Path) -> Result<()> {
    config_file::rewrite(path, |doc| {
        let Some(acp) = doc.get_mut("acp").and_then(|s| s.as_table_mut()) else {
            return false;
        };
        if acp.get("default_agent").and_then(|v| v.as_str()) != Some("aoe-agent") {
            return false;
        }
        acp.insert(
            "default_agent".into(),
            crate::session::config::DEFAULT_ACP_AGENT.into(),
        );
        info!(
            "v026: repointed acp.default_agent away from the unpackaged aoe-agent in {}",
            path.display()
        );
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
