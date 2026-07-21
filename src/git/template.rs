// Path template system for worktrees

use std::path::{Component, Path, PathBuf};

use super::error::Result;

pub struct TemplateVars {
    pub repo_name: String,
    pub branch: String,
    pub session_id: String,
    pub base_path: PathBuf,
}

pub fn sanitize_branch_name(branch: &str) -> String {
    branch.replace(
        ['/', '@', '#', '\\', ':', '*', '?', '"', '<', '>', '|'],
        "-",
    )
}

/// Lexically resolve `.` and `..` components without touching the filesystem
/// (the worktree does not exist yet at resolve time).
///
/// The default template (`../{repo-name}-worktrees/{branch}`) otherwise stores
/// paths like `/repos/my-repo/../my-repo-worktrees/feat` verbatim in
/// `sessions.json` as the session's `project_path`. Every site that treats
/// `project_path` as an identity — raw string comparison against a peer's
/// path, `~/.claude/projects/<encoded-cwd>` encoding of a since-deleted dir
/// that `fs::canonicalize` can no longer resolve — then sees a different
/// spelling of the same directory and misattributes session ids (#2858).
///
/// A `..` with no concrete parent to pop (relative base that begins with
/// `..`) is preserved rather than dropped.
///
/// Also used by `session::capture::canonicalize_or_raw` as the fallback when
/// `fs::canonicalize` fails (deleted directory): sessions created before the
/// normalization here may still carry a `..` spelling in `sessions.json`, and
/// identity comparisons must not depend on the directory still existing.
pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                match out.components().next_back() {
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    // `/..` is `/`; drop the component.
                    Some(Component::RootDir) => {}
                    // Nothing concrete to pop; keep the `..`.
                    _ => out.push(Component::ParentDir),
                }
            }
            other => out.push(other),
        }
    }
    out
}

pub fn resolve_template(template: &str, vars: &TemplateVars) -> Result<PathBuf> {
    let sanitized_branch = sanitize_branch_name(&vars.branch);

    let resolved = template
        .replace("{repo-name}", &vars.repo_name)
        .replace("{branch}", &sanitized_branch)
        .replace("{session-id}", &vars.session_id);

    let path = if resolved.starts_with('/') {
        PathBuf::from(resolved)
    } else {
        vars.base_path.join(&resolved)
    };
    let path = lexical_normalize(&path);

    tracing::debug!(
        target: "git.template",
        template = %template,
        repo = %vars.repo_name,
        branch = %vars.branch,
        session_id = %vars.session_id,
        resolved = %path.display(),
        "worktree template resolved",
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_template_with_repo_name() {
        let vars = TemplateVars {
            repo_name: "my-repo".to_string(),
            branch: "feat/test".to_string(),
            session_id: "abc123".to_string(),
            base_path: PathBuf::from("/home/user/repos/my-repo"),
        };

        let result = resolve_template("../{repo-name}-wt/{branch}", &vars).unwrap();
        assert!(result.to_string_lossy().contains("my-repo-wt"));
        assert!(result.to_string_lossy().contains("feat-test"));
    }

    #[test]
    fn test_sanitize_branch_name_replaces_slashes() {
        let sanitized = sanitize_branch_name("feat/my-feature");
        assert_eq!(sanitized, "feat-my-feature");
    }

    #[test]
    fn test_sanitize_branch_name_handles_special_chars() {
        let sanitized = sanitize_branch_name("feat@bug#123");
        assert!(!sanitized.contains("@"));
        assert!(!sanitized.contains("#"));
    }

    #[test]
    fn test_resolve_template_normalizes_parent_components() {
        let vars = TemplateVars {
            repo_name: "my-repo".to_string(),
            branch: "feat/test".to_string(),
            session_id: "abc123".to_string(),
            base_path: PathBuf::from("/home/user/repos/my-repo"),
        };

        let result = resolve_template("../{repo-name}-worktrees/{branch}", &vars).unwrap();
        assert_eq!(
            result,
            PathBuf::from("/home/user/repos/my-repo-worktrees/feat-test"),
            "resolved path must not retain a literal `..` component"
        );
        assert!(
            !result.components().any(|c| c == Component::ParentDir),
            "got: {}",
            result.display()
        );
    }

    #[test]
    fn test_lexical_normalize_preserves_unresolvable_parent() {
        assert_eq!(
            lexical_normalize(Path::new("../sibling/x")),
            PathBuf::from("../sibling/x")
        );
        assert_eq!(lexical_normalize(Path::new("/../x")), PathBuf::from("/x"));
        assert_eq!(
            lexical_normalize(Path::new("/a/b/./../c")),
            PathBuf::from("/a/c")
        );
    }

    #[test]
    fn test_resolve_template_with_all_variables() {
        let vars = TemplateVars {
            repo_name: "test".to_string(),
            branch: "main".to_string(),
            session_id: "xyz789".to_string(),
            base_path: PathBuf::from("/repos/test"),
        };

        let result = resolve_template("../wt/{repo-name}/{branch}/{session-id}", &vars).unwrap();

        assert!(result.to_string_lossy().contains("test"));
        assert!(result.to_string_lossy().contains("main"));
        assert!(result.to_string_lossy().contains("xyz789"));
    }
}
