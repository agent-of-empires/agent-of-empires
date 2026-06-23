//! Discovery of existing Claude Code sessions on disk, for importing them
//! into a structured-view session via `session/load`.
//!
//! Claude Code stores one session per file at
//! `<config>/projects/<encoded-cwd>/<sessionId>.jsonl`, where `<config>` is
//! `$CLAUDE_CONFIG_DIR` or `~/.claude`. The `<sessionId>` is the filename
//! stem and equals the id `claude-agent-acp` resumes via the SDK `resume`
//! param, so importing is just: create a structured session whose
//! `acp_session_id` is this id, with the session's recorded `cwd`.
//!
//! The encoded directory name is lossy (path separators and real hyphens
//! both render as `-`), so we read the real `cwd` from inside the file
//! rather than decoding the directory name.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Cap how many lines we read per file when extracting metadata. The `cwd`
/// and first user message live at the head of the transcript; a few hundred
/// lines is plenty without reading a multi-MB file fully.
const MAX_SCAN_LINES: usize = 400;

/// Cap how many sessions the picker shows. Newest first, so older sessions
/// past the cap are the least likely to be resumed. Applied by the endpoint
/// AFTER it filters out AoE-managed sessions, so a burst of managed sessions
/// can't squeeze real imports off the list.
pub const MAX_SESSIONS: usize = 200;

/// A discovered Claude Code session, summarized for the import picker.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClaudeSessionSummary {
    /// The on-disk session id (filename stem). Fed to `session/load`.
    pub session_id: String,
    /// The working directory recorded in the transcript. The structured
    /// session must run here for `claude --resume` to resolve the file.
    pub cwd: String,
    /// First human-authored prompt, truncated, for display. `None` when the
    /// transcript has no readable user message yet.
    pub title: Option<String>,
    /// File modification time as a unix epoch millisecond stamp, for
    /// recent-first sorting and "last used" display.
    pub last_modified_ms: u64,
    /// Whether `cwd` still exists. A resumed session needs its original cwd;
    /// the picker flags missing ones.
    pub cwd_exists: bool,
}

/// Base directory Claude Code stores config/sessions under: `$CLAUDE_CONFIG_DIR`
/// when set, else `~/.claude`. Returns `None` when neither resolves.
fn claude_config_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    dirs::home_dir().map(|h| h.join(".claude"))
}

/// Literal directory tokens derived from the worktree path templates, e.g.
/// `"-worktrees"` from `"../{repo-name}-worktrees/{branch}"` and `"-workspace-"`
/// from `"../{branch}-workspace-{session-id}"`. A cwd living under a directory
/// whose name contains one of these is an AoE worktree or workspace, so it is
/// excluded from the import picker. Derived from config so a custom template is
/// honored. See #2276.
fn worktree_dir_markers() -> Vec<String> {
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

/// True when `cwd` is an AoE scratch directory (`<app_dir>/scratch/<id>`),
/// regardless of namespace. The serving daemon's `get_app_dir()` only resolves
/// one namespace (release vs `-dev`), so a scratch session from the other
/// namespace slips past a plain `starts_with` check; match the app-dir name +
/// `scratch` component pair instead. See #2276.
fn cwd_is_aoe_scratch(cwd: &str) -> bool {
    let comps: Vec<&str> = Path::new(cwd)
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    comps
        .windows(2)
        .any(|w| w[0].contains("agent-of-empires") && w[1] == "scratch")
}

/// True when any directory component of `cwd` contains a worktree marker. Uses
/// `contains` (not a suffix match) because workspace dirs carry the marker
/// mid-name, e.g. `<branch>-workspace-<id>`.
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

/// Scan all discoverable Claude Code sessions, newest first (uncapped; the
/// endpoint applies `MAX_SESSIONS` after ownership filtering). Returns an empty
/// vec when the projects directory is absent (e.g. Claude Code was never run).
/// Unreadable files are skipped, not fatal.
///
/// AoE's own internal Claude runs are excluded: scratch sessions (cwd under
/// `<app_dir>/scratch/`, matched by layout so both release and -dev namespaces
/// are covered) and worktree / workspace sessions (cwd under a dir named by the
/// worktree path template). Sessions AoE already manages by id or project_path
/// are filtered separately by the endpoint, which has the instance list.
/// See #2276.
pub fn scan_sessions() -> Vec<ClaudeSessionSummary> {
    let Some(projects) = claude_config_dir().map(|d| d.join("projects")) else {
        return Vec::new();
    };
    let Ok(project_dirs) = fs::read_dir(&projects) else {
        return Vec::new();
    };
    let worktree_markers = worktree_dir_markers();

    let mut out = Vec::new();
    for project in project_dirs.flatten() {
        let path = project.path();
        if !path.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&path) else {
            continue;
        };
        for file in files.flatten() {
            let fpath = file.path();
            if fpath.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(summary) = summarize_file(&fpath) {
                // Scratch sessions live under `<app_dir>/scratch/<id>`. Match by
                // layout so both release and -dev namespaces are excluded the
                // same way (the feature does not discriminate between them).
                if cwd_is_aoe_scratch(&summary.cwd) {
                    continue;
                }
                // AoE creates session worktrees under a directory named by the
                // worktree path template (e.g. "<repo>-worktrees"). Any cwd
                // inside one is an AoE-managed worktree (or a one-shot AoE ran
                // there, like smart-rename), not a conversation to import.
                if cwd_under_worktree(&summary.cwd, &worktree_markers) {
                    continue;
                }
                out.push(summary);
            }
        }
    }

    out.sort_by_key(|s| std::cmp::Reverse(s.last_modified_ms));
    out
}

/// Build a summary for one `.jsonl` file. Returns `None` when the file has no
/// recoverable `cwd` (a session we could not safely resume), or no session id.
fn summarize_file(path: &Path) -> Option<ClaudeSessionSummary> {
    let session_id = path.file_stem()?.to_str()?.to_string();

    let last_modified_ms = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let file = fs::File::open(path).ok()?;
    let reader = BufReader::new(file);

    let mut cwd: Option<String> = None;
    let mut title: Option<String> = None;

    for line in reader.lines().take(MAX_SCAN_LINES).map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if cwd.is_none() {
            if let Some(c) = record.get("cwd").and_then(|v| v.as_str()) {
                if !c.is_empty() {
                    cwd = Some(c.to_string());
                }
            }
        }
        if title.is_none() {
            title = extract_user_title(&record);
        }
        if cwd.is_some() && title.is_some() {
            break;
        }
    }

    let cwd = cwd?;
    let cwd_exists = Path::new(&cwd).is_dir();
    Some(ClaudeSessionSummary {
        session_id,
        cwd,
        title,
        last_modified_ms,
        cwd_exists,
    })
}

/// Pull a human-readable title from a `user` record. Skips command wrappers
/// and caveat blocks (e.g. `<local-command-...>`, `<command-name>`) that
/// Claude Code injects, since they are noise, not the user's actual prompt.
fn extract_user_title(record: &serde_json::Value) -> Option<String> {
    if record.get("type").and_then(|v| v.as_str()) != Some("user") {
        return None;
    }
    let content = record.get("message")?.get("content")?;
    let text = match content {
        serde_json::Value::String(s) => displayable_user_text(s).map(str::to_owned),
        serde_json::Value::Array(parts) => parts.iter().find_map(|p| {
            if p.get("type").and_then(|v| v.as_str()) != Some("text") {
                return None;
            }
            let text = p.get("text").and_then(|v| v.as_str())?;
            displayable_user_text(text).map(str::to_owned)
        }),
        _ => None,
    }?;
    Some(truncate(&text, 120))
}

/// A user message's displayable text, or `None` for the command wrappers and
/// caveat blocks Claude Code injects (`<local-command-...>`, `<command-...>`).
/// Only those specific wrappers are dropped; a real prompt like `<div> is
/// rendering wrong` is kept.
fn displayable_user_text(text: &str) -> Option<&str> {
    let text = text.trim();
    if text.is_empty() || text.starts_with("<local-command-") || text.starts_with("<command-") {
        None
    } else {
        Some(text)
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let trimmed: String = s.chars().take(max_chars).collect();
    if trimmed.chars().count() < s.chars().count() {
        format!("{trimmed}…")
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_jsonl(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
        let path = dir.join(format!("{name}.jsonl"));
        let mut f = fs::File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        path
    }

    #[test]
    fn extracts_cwd_and_title_skipping_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let real_cwd = tmp.path().join("work");
        fs::create_dir(&real_cwd).unwrap();
        let cwd_str = real_cwd.to_str().unwrap();
        let path = write_jsonl(
            tmp.path(),
            "713b7f46-d0f2-454e-91be-a3305d35660c",
            &[
                r#"{"type":"queue-operation","operation":"enqueue"}"#,
                &format!(
                    r#"{{"type":"user","cwd":"{cwd_str}","message":{{"role":"user","content":"<local-command-caveat>noise</local-command-caveat>"}}}}"#
                ),
                &format!(
                    r#"{{"type":"user","cwd":"{cwd_str}","message":{{"role":"user","content":[{{"type":"text","text":"Fix the spinner bug please"}}]}}}}"#
                ),
            ],
        );

        let s = summarize_file(&path).unwrap();
        assert_eq!(s.session_id, "713b7f46-d0f2-454e-91be-a3305d35660c");
        assert_eq!(s.cwd, cwd_str);
        assert_eq!(s.title.as_deref(), Some("Fix the spinner bug please"));
        assert!(s.cwd_exists);
    }

    #[test]
    fn title_keeps_angle_bracket_prompts_and_skips_only_wrappers() {
        // A real prompt that starts with '<' is kept.
        assert_eq!(
            displayable_user_text("<div> is rendering wrong"),
            Some("<div> is rendering wrong")
        );
        // Injected command wrappers are dropped.
        assert_eq!(
            displayable_user_text("<local-command-caveat>x</local-command-caveat>"),
            None
        );
        assert_eq!(
            displayable_user_text("<command-name>/foo</command-name>"),
            None
        );
        assert_eq!(displayable_user_text("   "), None);
    }

    #[test]
    fn title_picks_first_real_text_part_after_wrapper() {
        let record = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": [
                { "type": "text", "text": "<command-name>/plan</command-name>" },
                { "type": "text", "text": "Actually fix the bug" }
            ]}
        });
        assert_eq!(
            extract_user_title(&record).as_deref(),
            Some("Actually fix the bug")
        );
    }

    #[test]
    fn missing_cwd_dir_flagged_not_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            tmp.path(),
            "abc",
            &[
                r#"{"type":"user","cwd":"/nonexistent/path/xyz","message":{"role":"user","content":"hi"}}"#,
            ],
        );
        let s = summarize_file(&path).unwrap();
        assert_eq!(s.cwd, "/nonexistent/path/xyz");
        assert!(!s.cwd_exists);
        assert_eq!(s.title.as_deref(), Some("hi"));
    }

    #[test]
    fn strip_placeholders_leaves_literal() {
        assert_eq!(strip_placeholders("{repo-name}-worktrees"), "-worktrees");
        assert_eq!(strip_placeholders("{branch}"), "");
        assert_eq!(strip_placeholders(".."), "..");
    }

    #[test]
    fn cwd_under_worktree_matches_worktree_and_workspace_dirs() {
        let markers = vec!["-worktrees".to_string(), "-workspace-".to_string()];
        assert!(cwd_under_worktree(
            "/Users/me/aoe/agent-of-empires-worktrees/Saracens",
            &markers
        ));
        assert!(cwd_under_worktree(
            "/Users/me/aoe/agent-of-empires-worktrees/Saracens/sub",
            &markers
        ));
        // Workspace dirs carry the marker mid-name (contains, not suffix).
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

    #[test]
    fn no_cwd_means_unimportable_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_jsonl(
            tmp.path(),
            "nocwd",
            &[r#"{"type":"last-prompt","sessionId":"nocwd"}"#],
        );
        assert!(summarize_file(&path).is_none());
    }
}
