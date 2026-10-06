//! Shared shape and ownership filter for native sessions offered by the import picker, whichever
//! source listed them (the Claude disk scan or ACP `session/list`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::session::Instance;

/// Cap how many sessions the picker shows.
pub const MAX_SESSIONS: usize = 200;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ImportableSession {
    pub session_id: String,
    /// The recorded working directory; the import must run here.
    pub cwd: String,
    pub title: Option<String>,
    /// RFC 3339 last activity, when the source reports one.
    pub updated_at: Option<String>,
    pub cwd_exists: bool,
}

impl ImportableSession {
    pub fn new(
        session_id: String,
        cwd: String,
        title: Option<String>,
        updated_at: Option<String>,
    ) -> Self {
        let cwd_exists = Path::new(&cwd).is_dir();
        Self {
            session_id,
            cwd,
            title,
            updated_at,
            cwd_exists,
        }
    }

    fn updated_at_parsed(&self) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        chrono::DateTime::parse_from_rfc3339(self.updated_at.as_deref()?).ok()
    }
}

/// Importable sessions, newest first, and whether more existed than [`MAX_SESSIONS`].
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ImportableList {
    pub sessions: Vec<ImportableSession>,
    pub truncated: bool,
}

/// What AoE already owns, so the picker never offers it back.
#[derive(Debug, Default)]
pub struct Owned {
    ids: HashSet<String>,
    dirs: Vec<PathBuf>,
    worktree_markers: Vec<String>,
}

impl Owned {
    /// A plain project path is not owned: a user's own agent run there is importable. Only stored
    /// ids and AoE-provisioned dirs (scratch, managed worktree, workspace) are. `worktree_markers`
    /// comes from [`worktree_dir_markers`], which loads config and so belongs off the runtime.
    pub fn new(instances: &[Instance], worktree_markers: Vec<String>) -> Self {
        let ids = instances
            .iter()
            .flat_map(|i| {
                i.acp_session_id
                    .iter()
                    .chain(i.agent_session_id.iter())
                    .cloned()
            })
            .collect();
        let dirs = instances
            .iter()
            .filter(|i| {
                i.scratch
                    || i.worktree_info.as_ref().is_some_and(|w| w.managed_by_aoe)
                    || i.workspace_info.is_some()
            })
            .map(|i| PathBuf::from(&i.project_path))
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        Self {
            ids,
            dirs,
            worktree_markers,
        }
    }

    pub fn excludes(&self, session_id: &str, cwd: &str) -> bool {
        self.ids.contains(session_id)
            || self.dirs.iter().any(|d| Path::new(cwd).starts_with(d))
            || cwd_is_aoe_scratch(cwd)
            || cwd_under_worktree(cwd, &self.worktree_markers)
    }
}

/// Drop owned sessions, sort newest first with unknown activity last, and cap at
/// [`MAX_SESSIONS`]. Capping after filtering keeps owned sessions from crowding out real ones.
/// `truncated` is true when entries were cut here or `source_truncated` says the source stopped.
pub fn retain_importable(
    mut sessions: Vec<ImportableSession>,
    owned: &Owned,
    source_truncated: bool,
) -> ImportableList {
    sessions.retain(|s| !owned.excludes(&s.session_id, &s.cwd));
    sessions.sort_by_cached_key(|s| std::cmp::Reverse(s.updated_at_parsed()));
    let truncated = source_truncated || sessions.len() > MAX_SESSIONS;
    sessions.truncate(MAX_SESSIONS);
    ImportableList {
        sessions,
        truncated,
    }
}

/// Literal directory tokens derived from the worktree path templates, e.g. `"-worktrees"` from
/// `"../{repo-name}-worktrees/{branch}"` and `"-workspace-"` from
/// `"../{branch}-workspace-{session-id}"`.
pub fn worktree_dir_markers() -> Vec<String> {
    let cfg = crate::session::Config::load_or_warn();
    let mut markers = Vec::new();
    for tmpl in [
        cfg.worktree.path_template.as_str(),
        cfg.worktree.workspace_path_template.as_str(),
    ] {
        for seg in tmpl.split('/') {
            let lit = strip_placeholders(seg);
            if lit.len() >= 3 && lit != ".." && !markers.contains(&lit) {
                markers.push(lit);
            }
        }
    }
    markers
}

/// Remove `{placeholder}` spans from a template path segment, leaving the
/// literal text (e.g. `"{repo-name}-worktrees"` -> `"-worktrees"`).
fn strip_placeholders(seg: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in seg.chars() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out
}

/// True when `cwd` is an AoE scratch directory (`<app_dir>/scratch/<id>`), regardless of namespace.
fn cwd_is_aoe_scratch(cwd: &str) -> bool {
    let comps: Vec<&str> = Path::new(cwd)
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    comps
        .windows(2)
        .any(|w| w[0].contains("agent-of-empires") && w[1] == "scratch")
}

/// True when any directory component of `cwd` contains a worktree marker.
fn cwd_under_worktree(cwd: &str, markers: &[String]) -> bool {
    if markers.is_empty() {
        return false;
    }
    Path::new(cwd).components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|name| markers.iter().any(|m| name.contains(m.as_str())))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_under_worktree_matches_worktree_and_workspace_dirs() {
        assert_eq!(strip_placeholders("{repo-name}-worktrees"), "-worktrees");
        assert_eq!(strip_placeholders("{branch}"), "");
        assert_eq!(strip_placeholders(".."), "..");
        let markers = vec!["-worktrees".to_string(), "-workspace-".to_string()];
        assert!(cwd_under_worktree(
            "/Users/me/aoe/agent-of-empires-worktrees/Saracens",
            &markers
        ));
        assert!(cwd_under_worktree(
            "/Users/me/aoe/agent-of-empires-worktrees/Saracens/sub",
            &markers
        ));
        assert!(cwd_under_worktree(
            "/Users/me/aoe/soft-close-grace-window-workspace-55406399",
            &markers
        ));
        assert!(!cwd_under_worktree("/Users/me/projects/alpha", &markers));
        assert!(!cwd_under_worktree("/Users/me/projects/alpha", &[]));
    }

    #[test]
    fn aoe_scratch_detected_in_both_namespaces() {
        assert!(cwd_is_aoe_scratch(
            "/Users/me/.agent-of-empires/scratch/5c8d250f60ec4328"
        ));
        assert!(cwd_is_aoe_scratch(
            "/Users/me/.agent-of-empires-dev/scratch/abcd"
        ));
        assert!(cwd_is_aoe_scratch(
            "/home/me/.config/agent-of-empires/scratch/abcd"
        ));
        assert!(!cwd_is_aoe_scratch("/Users/me/projects/scratch"));
        assert!(!cwd_is_aoe_scratch("/Users/me/projects/alpha"));
    }

    fn entry(id: &str, cwd: &str, updated_at: Option<&str>) -> ImportableSession {
        ImportableSession {
            session_id: id.into(),
            cwd: cwd.into(),
            title: None,
            updated_at: updated_at.map(str::to_string),
            cwd_exists: true,
        }
    }

    #[test]
    fn retain_importable_drops_owned_and_sorts_unknown_last() {
        let owned = Owned {
            ids: HashSet::from(["owned-id".to_string()]),
            dirs: vec![PathBuf::from("/p/managed")],
            worktree_markers: vec!["-worktrees".into()],
        };
        let sessions = vec![
            entry("unknown", "/p/app", None),
            entry("old", "/p/app", Some("2026-01-01T00:00:00Z")),
            entry("owned-id", "/p/app", Some("2026-09-01T00:00:00Z")),
            entry("managed", "/p/managed/sub", Some("2026-09-01T00:00:00Z")),
            entry("scratch", "/h/.agent-of-empires/scratch/x", None),
            entry("wt", "/p/app-worktrees/b", None),
            entry("new", "/p/app", Some("2026-09-01T10:00:00+02:00")),
            entry("garbled", "/p/app", Some("yesterday")),
        ];
        let kept = retain_importable(sessions, &owned, false);
        let ids: Vec<_> = kept
            .sessions
            .iter()
            .map(|s| s.session_id.as_str())
            .collect();
        assert_eq!(ids, ["new", "old", "unknown", "garbled"]);
        assert!(!kept.truncated);
    }

    #[test]
    fn retain_importable_caps_after_filtering() {
        let owned = Owned {
            ids: HashSet::from(["owned".to_string()]),
            ..Owned::default()
        };
        let mut sessions = vec![entry("owned", "/p", None)];
        sessions.extend((0..MAX_SESSIONS).map(|i| entry(&i.to_string(), "/p", None)));
        for (extra, source_truncated, expect) in
            [(0, false, false), (1, false, true), (0, true, true)]
        {
            let mut input = sessions.clone();
            input.extend((0..extra).map(|i| entry(&format!("x{i}"), "/p", None)));
            let kept = retain_importable(input, &owned, source_truncated);
            assert_eq!(kept.sessions.len(), MAX_SESSIONS);
            assert_eq!(
                kept.truncated, expect,
                "extra={extra} source={source_truncated}"
            );
        }
    }
}
