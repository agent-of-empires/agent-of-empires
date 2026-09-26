//! Claude Code transcript lookup.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::canonicalize_or_raw;

/// Claude's per-project directory name: every char other than ASCII alphanumerics and `-` becomes `-`.
pub(crate) fn encode_claude_project_path(project_path: &str) -> String {
    project_path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The Claude config root the launched pane will read and write.
///
/// A declared `session.agent_config_dir` wins outright, matching
/// [`crate::hooks::trust_host_project`]: the wrappers that setting exists for
/// export the variable themselves, after AoE has handed the launch its
/// environment, so it is absent from `host_env` in exactly that case. Without
/// one, precedence follows [`crate::hooks::agent_settings_path_in`]: the
/// session's host environment, then AoE's own env, then `~/.claude`.
pub(crate) fn claude_home_for_host_environment(
    declared: Option<&Path>,
    host_env: &[String],
) -> Result<PathBuf> {
    if let Some(dir) = declared {
        return Ok(dir.to_path_buf());
    }
    match crate::hooks::resolve_config_dir_override("CLAUDE_CONFIG_DIR", host_env) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
            .join(".claude")),
    }
}

/// Whether `store` is Claude's built-in `<home>/.claude`. Exporting
/// `CLAUDE_CONFIG_DIR` for it is not a no-op: Claude then reads its global
/// config from `<home>/.claude/.claude.json` instead of `<home>/.claude.json`.
pub(crate) fn is_default_claude_store(store: &Path, home: &Path) -> bool {
    let identity = |path: &Path| {
        super::canonicalize_allowing_missing_leaf(path).unwrap_or_else(|| path.to_path_buf())
    };
    identity(store) == identity(&home.join(".claude"))
}

/// The store a host Claude worker is pinned to by its selected conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeStorePin {
    pub store: PathBuf,
    /// The conversation's launch exported `CLAUDE_CONFIG_DIR` even for the default store.
    pub explicit: bool,
}

impl ClaudeStorePin {
    pub(crate) fn of(execution: &crate::session::ExecutionBinding) -> Option<Self> {
        Some(Self {
            store: execution.stores.first()?.clone(),
            explicit: execution.exported_default_store,
        })
    }
}

/// True only when Claude's home resolves and `<config>/projects/<cwd>/<id>.jsonl`
/// is missing, so a never-prompted pinned id can launch fresh instead of failing
/// `--resume`. The config dir resolves as the launch does (see
/// [`claude_home_for_host_environment`]); probing the wrong tree would downgrade
/// real conversations. Existence only, so an idle conversation still counts as
/// present.
pub(crate) fn claude_host_transcript_confirmed_absent(
    project_path: &str,
    session_id: &str,
    host_env: &[String],
    declared_config_dir: Option<&Path>,
) -> bool {
    let Ok(claude_home) = claude_home_for_host_environment(declared_config_dir, host_env) else {
        return false;
    };
    let canonical = canonicalize_or_raw(project_path);
    let transcript = claude_home
        .join("projects")
        .join(encode_claude_project_path(&canonical.to_string_lossy()))
        .join(format!("{session_id}.jsonl"));
    !transcript.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn encode_claude_project_path_replaces_non_alphanumerics() {
        for (input, expected) in [
            ("/Users/foo/bar", "-Users-foo-bar"),
            ("my-project-123", "my-project-123"),
            (
                "/home/user/my project (copy)",
                "-home-user-my-project--copy-",
            ),
        ] {
            assert_eq!(encode_claude_project_path(input), expected);
        }
    }

    /// Absence is existence-only (an hour-old transcript still counts), and a declared
    /// `session.agent_config_dir` wins over the host environment: the wrappers it exists for export
    /// `CLAUDE_CONFIG_DIR` themselves after launch, so probing the environment's directory would
    /// report every real conversation absent and downgrade a good `--resume` to a `--session-id`
    /// the agent rejects.
    #[test]
    fn transcript_absence_is_existence_only_in_the_declared_store() {
        let store = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let project_dir = store.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();
        let present = "11111111-2222-3333-4444-555555555555";
        let missing = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let file = project_dir.join(format!("{present}.jsonl"));
        std::fs::write(&file, "data\n").unwrap();
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(hour_ago))
            .unwrap();
        // (CLAUDE_CONFIG_DIR, declared dir, project, sid, confirmed absent)
        for (env, declared, project, sid, absent) in [
            (&store, None, "/tmp/myproject", present, false),
            (&store, None, "/tmp/myproject", missing, true),
            (&store, None, "/tmp/never-opened-project", present, true),
            (&empty, None, "/tmp/myproject", present, true),
            (&empty, Some(store.path()), "/tmp/myproject", present, false),
        ] {
            let _env =
                crate::session::test_support::EnvGuard::set(&[("CLAUDE_CONFIG_DIR", env.path())]);
            assert_eq!(
                claude_host_transcript_confirmed_absent(project, sid, &[], declared),
                absent,
                "{project} {sid} declared={declared:?}"
            );
        }
    }
}
