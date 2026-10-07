//! Display helpers for session-scoped file paths.

#[derive(Debug, Clone)]
pub struct SessionPathRoots {
    pub id: String,
    pub project_path: String,
    pub main_repo_path: Option<String>,
    pub workspace_repos: Vec<WorkspaceRepoRoot>,
}

/// Session metadata the native structured view needs alongside its path roots,
/// projected from one `/api/sessions` row so a single fetch hydrates both the
/// friendly header and repo-relative tool paths.
#[derive(Debug, Clone)]
pub struct SessionViewInfo {
    pub title: String,
    pub tool: String,
    pub acp_agent: Option<String>,
    pub paths: SessionPathRoots,
}

impl SessionViewInfo {
    pub fn agent_label(&self) -> String {
        self.acp_agent.clone().unwrap_or_else(|| self.tool.clone())
    }
}

impl From<crate::daemon::SessionResponse> for SessionViewInfo {
    fn from(session: crate::daemon::SessionResponse) -> Self {
        Self {
            title: session.title,
            tool: session.tool,
            acp_agent: session.acp_agent,
            paths: SessionPathRoots {
                id: session.id,
                project_path: session.project_path,
                main_repo_path: session.main_repo_path,
                workspace_repos: session
                    .workspace_repos
                    .into_iter()
                    .map(|repo| WorkspaceRepoRoot {
                        name: repo.name,
                        source_path: repo.source_path,
                    })
                    .collect(),
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceRepoRoot {
    pub name: String,
    pub source_path: String,
}

struct ResolvedPath {
    relative_path: String,
    repo_name: Option<String>,
}

/// Display form of a tool-call path, matching the web structured view's
/// `relativeDisplayPath` helper.
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

    fn roots(project: &str, main_repo: Option<&str>, repos: &[(&str, &str)]) -> SessionPathRoots {
        SessionPathRoots {
            id: "s-1".into(),
            project_path: project.into(),
            main_repo_path: main_repo.map(Into::into),
            workspace_repos: repos
                .iter()
                .map(|(name, source_path)| WorkspaceRepoRoot {
                    name: (*name).into(),
                    source_path: (*source_path).into(),
                })
                .collect(),
        }
    }

    #[test]
    fn relative_display_path_strips_the_longest_known_root() {
        let worktree = roots("/Users/me/.aoe/worktrees/feat", Some("/Users/me/repo"), &[]);
        // (roots, raw path, displayed path)
        let cases = [
            (
                &worktree,
                "/Users/me/.aoe/worktrees/feat/src/hooks/mod.rs",
                "src/hooks/mod.rs",
            ),
            (&worktree, "/Users/me/repo/src/app.ts", "src/app.ts"),
            // A sibling sharing the root's prefix is not under it.
            (
                &worktree,
                "/Users/me/repo_old/src/app.ts",
                "/Users/me/repo_old/src/app.ts",
            ),
            (&worktree, "src/app.ts", "src/app.ts"),
            (&worktree, "./src/app.ts", "src/app.ts"),
            (&worktree, "/etc/hosts", "/etc/hosts"),
        ];
        for (roots, raw, want) in cases {
            assert_eq!(relative_display_path(raw, Some(roots)), want, "{raw}");
        }

        let windows = roots("C:\\Users\\me\\repo", None, &[]);
        assert_eq!(
            relative_display_path("c:\\Users\\me\\repo\\src\\app.ts", Some(&windows)),
            "src/app.ts",
            "drive letters match case-insensitively"
        );

        let workspace = roots(
            "/Users/me/.aoe/worktrees/ws",
            None,
            &[("api", "/Users/me/api")],
        );
        assert_eq!(
            relative_display_path("/Users/me/api/src/h.ts", Some(&workspace)),
            "api/src/h.ts",
            "a workspace repo keeps its name as the prefix"
        );

        assert_eq!(relative_display_path("/tmp/a.rs", None), "/tmp/a.rs");
    }
}
