//! Display helpers for session-scoped file paths.

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SessionPathRoots {
    pub id: String,
    pub project_path: String,
    pub main_repo_path: Option<String>,
    #[serde(default)]
    pub workspace_repos: Vec<WorkspaceRepoRoot>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct WorkspaceRepoRoot {
    pub name: String,
    pub source_path: String,
}

struct ResolvedPath {
    relative_path: String,
    repo_name: Option<String>,
}

/// Display form of a tool-call path, matching the web structured view's
/// `relativeDisplayPath` helper. Paths under a workspace repo are prefixed
/// with the repo name, paths under the session worktree or main repo are shown
/// bare relative, and paths outside every known root stay unchanged.
pub fn relative_display_path(raw: &str, roots: Option<&SessionPathRoots>) -> String {
    let Some(roots) = roots else {
        return raw.to_string();
    };
    if raw.is_empty() {
        return raw.to_string();
    }

    match resolve_to_repo_relative(raw, roots) {
        Some(ResolvedPath {
            relative_path,
            repo_name: Some(repo_name),
        }) => format!("{repo_name}/{relative_path}"),
        Some(ResolvedPath {
            relative_path,
            repo_name: None,
        }) => relative_path,
        None => raw.to_string(),
    }
}

fn resolve_to_repo_relative(path: &str, roots: &SessionPathRoots) -> Option<ResolvedPath> {
    let target = normalize_path_for_match(path);
    let is_absolute = target.starts_with('/') || is_windows_absolute(&target);

    if !is_absolute {
        let rel = target.strip_prefix("./").unwrap_or(&target);
        return (!rel.is_empty()).then(|| ResolvedPath {
            relative_path: rel.to_string(),
            repo_name: None,
        });
    }

    for repo in &roots.workspace_repos {
        let root = normalize_root(&repo.source_path);
        if let Some(rel) = target.strip_prefix(&root) {
            return Some(ResolvedPath {
                relative_path: rel.to_string(),
                repo_name: Some(repo.name.clone()),
            });
        }
    }

    let root = normalize_root(&roots.project_path);
    if let Some(rel) = target.strip_prefix(&root) {
        return Some(ResolvedPath {
            relative_path: rel.to_string(),
            repo_name: None,
        });
    }

    if let Some(main_repo_path) = &roots.main_repo_path {
        let root = normalize_root(main_repo_path);
        if let Some(rel) = target.strip_prefix(&root) {
            return Some(ResolvedPath {
                relative_path: rel.to_string(),
                repo_name: None,
            });
        }
    }

    None
}

fn normalize_path_for_match(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    let bytes = normalized.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        let drive = normalized[0..1].to_ascii_lowercase();
        normalized.replace_range(0..1, &drive);
    }
    normalized
}

fn normalize_root(root: &str) -> String {
    let mut normalized = normalize_path_for_match(root);
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    normalized
}

fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots() -> SessionPathRoots {
        SessionPathRoots {
            id: "s-1".into(),
            project_path: "/Users/me/.aoe/worktrees/feat".into(),
            main_repo_path: Some("/Users/me/repo".into()),
            workspace_repos: Vec::new(),
        }
    }

    #[test]
    fn strips_worktree_root_to_relative_path() {
        assert_eq!(
            relative_display_path(
                "/Users/me/.aoe/worktrees/feat/src/hooks/mod.rs",
                Some(&roots())
            ),
            "src/hooks/mod.rs"
        );
    }

    #[test]
    fn falls_back_to_main_repo_root() {
        assert_eq!(
            relative_display_path("/Users/me/repo/src/app.ts", Some(&roots())),
            "src/app.ts"
        );
    }

    #[test]
    fn does_not_match_sibling_with_shared_prefix() {
        assert_eq!(
            relative_display_path("/Users/me/repo_old/src/app.ts", Some(&roots())),
            "/Users/me/repo_old/src/app.ts"
        );
    }

    #[test]
    fn treats_relative_path_as_already_relative() {
        assert_eq!(
            relative_display_path("src/app.ts", Some(&roots())),
            "src/app.ts"
        );
        assert_eq!(
            relative_display_path("./src/app.ts", Some(&roots())),
            "src/app.ts"
        );
    }

    #[test]
    fn matches_windows_drive_root_case_insensitively() {
        let roots = SessionPathRoots {
            id: "s-1".into(),
            project_path: "C:\\Users\\me\\repo".into(),
            main_repo_path: None,
            workspace_repos: Vec::new(),
        };
        assert_eq!(
            relative_display_path("c:\\Users\\me\\repo\\src\\app.ts", Some(&roots)),
            "src/app.ts"
        );
    }

    #[test]
    fn prefixes_workspace_repo_name() {
        let roots = SessionPathRoots {
            id: "s-1".into(),
            project_path: "/Users/me/.aoe/worktrees/ws".into(),
            main_repo_path: None,
            workspace_repos: vec![WorkspaceRepoRoot {
                name: "api".into(),
                source_path: "/Users/me/api".into(),
            }],
        };
        assert_eq!(
            relative_display_path("/Users/me/api/src/h.ts", Some(&roots)),
            "api/src/h.ts"
        );
    }

    #[test]
    fn returns_raw_path_outside_known_roots() {
        assert_eq!(
            relative_display_path("/etc/hosts", Some(&roots())),
            "/etc/hosts"
        );
    }

    #[test]
    fn returns_raw_path_without_roots() {
        assert_eq!(relative_display_path("/tmp/a.rs", None), "/tmp/a.rs");
    }
}
