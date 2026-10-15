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
    use crate::migrations::test_cases::assert_rewrites;

    /// A missing or unparsable config is skipped: this migration corrects a
    /// default, so it must never abort startup.
    #[test]
    fn rewrites_only_the_seeded_aoe_agent() {
        let unchanged = |toml: &'static str| (Some(toml), Some(toml));
        assert_rewrites(
            "config.toml",
            run_in,
            &[
                (
                    Some("[acp]\ndefault_agent = \"aoe-agent\"\nmax_concurrent_workers = 5\n\n[theme]\nname = \"rose-pine\"\n"),
                    Some("[acp]\ndefault_agent = \"claude-code\"\nmax_concurrent_workers = 5\n\n[theme]\nname = \"rose-pine\"\n"),
                ),
                // A deliberate choice of any other agent survives.
                unchanged("[acp]\ndefault_agent = \"codex\"\n"),
                unchanged("[acp]\ndefault_agent = \"claude-code\"\n"),
                // An absent key already resolves to the new default.
                unchanged("[acp]\nreplay_events = 0\n"),
                unchanged("[theme]\nname = \"empire\"\n"),
                unchanged("this is not = = toml"),
                (None, None),
            ],
        );
    }
}
