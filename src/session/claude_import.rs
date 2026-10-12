//! Discovery of existing Claude Code sessions on disk, for importing them into a structured-view
//! session via `session/load`.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// Cap how many lines we read per file when extracting metadata.
const MAX_SCAN_LINES: usize = 400;

/// A discovered Claude Code session, summarized for the import picker.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeSessionSummary {
    /// The on-disk session id (filename stem). Fed to `session/load`.
    pub session_id: String,
    pub config_dir: PathBuf,
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

/// Scan all discoverable Claude Code sessions, newest first and unfiltered; callers drop owned
/// ones with [`crate::session::import::Owned`].
pub fn scan_sessions() -> Vec<ClaudeSessionSummary> {
    let Some(config_dir) = claude_config_dir() else {
        return Vec::new();
    };
    scan_sessions_in(&config_dir)
}

/// Scan the `projects/` tree under `config_dir` (the resolved Claude config directory).
pub fn scan_sessions_in(config_dir: &Path) -> Vec<ClaudeSessionSummary> {
    let projects = config_dir.join("projects");
    let Ok(project_dirs) = fs::read_dir(&projects) else {
        return Vec::new();
    };

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
            if let Some(summary) = summarize_file(&fpath, config_dir) {
                out.push(summary);
            }
        }
    }

    out.sort_by_key(|s| std::cmp::Reverse(s.last_modified_ms));
    out
}

/// Retain sessions whose recorded `cwd` is at or under one of `roots`.
pub fn sessions_under_paths(
    sessions: Vec<ClaudeSessionSummary>,
    roots: &[PathBuf],
) -> Vec<ClaudeSessionSummary> {
    sessions
        .into_iter()
        .filter(|s| {
            let cwd = Path::new(&s.cwd);
            roots.iter().any(|r| cwd.starts_with(r))
        })
        .collect()
}

impl From<ClaudeSessionSummary> for crate::session::import::ImportableSession {
    fn from(s: ClaudeSessionSummary) -> Self {
        let updated_at = (s.last_modified_ms > 0)
            .then(|| chrono::DateTime::from_timestamp_millis(s.last_modified_ms as i64))
            .flatten()
            .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        Self {
            session_id: s.session_id,
            cwd: s.cwd,
            title: s.title,
            updated_at,
            cwd_exists: s.cwd_exists,
        }
    }
}

/// Build a summary for one `.jsonl` file. Returns `None` when the file has no
/// recoverable `cwd` (a session we could not safely resume), or no session id.
fn summarize_file(path: &Path, config_dir: &Path) -> Option<ClaudeSessionSummary> {
    let session_id = path.file_stem()?.to_str()?.to_string();

    let last_modified_ms = fs::metadata(path)
        .and_then(|m| m.modified())
        .map(crate::util::system_time_to_ms)
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
        config_dir: crate::session::capture::canonicalize_or_raw(config_dir.to_str()?),
        cwd,
        title,
        last_modified_ms,
        cwd_exists,
    })
}

/// Pull a human-readable title from a `user` record.
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

/// A user message's displayable text, or `None` for the command wrappers and caveat blocks Claude
/// Code injects (`<local-command-...>`, `<command-...>`).
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
    fn summarize_file_reads_cwd_and_title_and_flags_missing_dirs() {
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

        let s = summarize_file(&path, path.parent().unwrap()).unwrap();
        assert_eq!(s.session_id, "713b7f46-d0f2-454e-91be-a3305d35660c");
        assert_eq!(s.cwd, cwd_str);
        assert_eq!(s.title.as_deref(), Some("Fix the spinner bug please"));
        assert!(s.cwd_exists);

        let path = write_jsonl(
            tmp.path(),
            "abc",
            &[
                r#"{"type":"user","cwd":"/nonexistent/path/xyz","message":{"role":"user","content":"hi"}}"#,
            ],
        );
        let s = summarize_file(&path, path.parent().unwrap()).unwrap();
        assert_eq!(s.cwd, "/nonexistent/path/xyz");
        assert!(!s.cwd_exists, "a missing cwd is flagged, not dropped");
        assert_eq!(s.title.as_deref(), Some("hi"));

        let path = write_jsonl(
            tmp.path(),
            "nocwd",
            &[r#"{"type":"last-prompt","sessionId":"nocwd"}"#],
        );
        assert!(summarize_file(&path, path.parent().unwrap()).is_none());
    }

    #[test]
    fn title_skips_only_command_wrappers() {
        for (text, expected) in [
            ("<div> is rendering wrong", Some("<div> is rendering wrong")),
            ("<local-command-caveat>x</local-command-caveat>", None),
            ("<command-name>/foo</command-name>", None),
            ("   ", None),
        ] {
            assert_eq!(displayable_user_text(text), expected, "{text:?}");
        }
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

    fn summary(id: &str, cwd: &str) -> ClaudeSessionSummary {
        ClaudeSessionSummary {
            session_id: id.to_string(),
            config_dir: PathBuf::from("/claude-import-store"),
            cwd: cwd.to_string(),
            title: None,
            last_modified_ms: 0,
            cwd_exists: true,
        }
    }

    #[test]
    fn sessions_under_paths_is_component_aware_and_matches_any_root() {
        let sessions = vec![
            summary("a", "/home/me/app"),
            summary("b", "/home/me/app/sub/deep"),
            summary("c", "/home/me/app-v2"),
            summary("d", "/home/me/other"),
            summary("e", "/p/three/z"),
        ];
        let roots = vec![PathBuf::from("/home/me/app"), PathBuf::from("/p/three")];
        let kept: Vec<_> = sessions_under_paths(sessions, &roots)
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(kept, vec!["a", "b", "e"]);
    }

    #[test]
    fn scan_sessions_in_reads_temp_config_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("projects").join("-encoded-cwd");
        fs::create_dir_all(&project).unwrap();
        let work = tmp.path().join("work");
        fs::create_dir(&work).unwrap();
        let cwd_str = work.to_str().unwrap();
        write_jsonl(
            &project,
            "713b7f46-d0f2-454e-91be-a3305d35660c",
            &[&format!(
                r#"{{"type":"user","cwd":"{cwd_str}","message":{{"role":"user","content":"hello"}}}}"#
            )],
        );

        let found = scan_sessions_in(tmp.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session_id, "713b7f46-d0f2-454e-91be-a3305d35660c");
        assert_eq!(found[0].cwd, cwd_str);
        assert!(scan_sessions_in(&tmp.path().join("nope")).is_empty());
    }
}
