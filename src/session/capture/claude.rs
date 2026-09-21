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

    #[test]
    fn transcript_absence_is_existence_only() {
        let tmp = tempfile::tempdir().unwrap();
        let project_dir = tmp.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();
        let present = "11111111-2222-3333-4444-555555555555";
        let missing = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let file = project_dir.join(format!("{present}.jsonl"));
        std::fs::write(&file, "data\n").unwrap();
        // An old transcript must still count as present.
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(hour_ago))
            .unwrap();
        let _env =
            crate::session::test_support::EnvGuard::set(&[("CLAUDE_CONFIG_DIR", tmp.path())]);

        assert!(!claude_host_transcript_confirmed_absent(
            "/tmp/myproject",
            present,
            &[],
            None
        ));
        assert!(claude_host_transcript_confirmed_absent(
            "/tmp/myproject",
            missing,
            &[],
            None
        ));
        assert!(claude_host_transcript_confirmed_absent(
            "/tmp/never-opened-project",
            present,
            &[],
            None
        ));
    }

    /// Nothing puts `CLAUDE_CONFIG_DIR` in the host environment for a session
    /// pinned with `session.agent_config_dir`: the wrappers it exists for
    /// export it themselves, after the launch has its environment. Probing the
    /// environment's answer would report every real conversation absent and
    /// downgrade a good `--resume` to a `--session-id` the agent rejects.
    #[test]
    fn declared_config_dir_wins_over_the_host_environment() {
        let declared = tempfile::tempdir().unwrap();
        let from_env = tempfile::tempdir().unwrap();
        let sid = "11111111-2222-3333-4444-555555555555";
        let project_dir = declared.path().join("projects").join("-tmp-myproject");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join(format!("{sid}.jsonl")), "data\n").unwrap();

        let _env =
            crate::session::test_support::EnvGuard::set(&[("CLAUDE_CONFIG_DIR", from_env.path())]);

        assert!(
            claude_host_transcript_confirmed_absent("/tmp/myproject", sid, &[], None),
            "pre-condition: the environment's directory does not hold it"
        );
        assert!(
            !claude_host_transcript_confirmed_absent(
                "/tmp/myproject",
                sid,
                &[],
                Some(declared.path())
            ),
            "the declared directory is the one the agent actually opens"
        );
    }
}
