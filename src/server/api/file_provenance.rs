//! Provenance-based confinement for the session file-read endpoint (#3088).
//!
//! A read is allowed only when the canonicalized target is under one of the
//! session's project roots, or is a path the session's agent actually touched,
//! recovered from the ACP event log. Provenance, not a directory allowlist, is
//! the boundary: the dashboard can open exactly what the agent already worked
//! with, and nothing it never touched.
//!
//! The final read opens the file beneath a `cap_std` capability directory, which
//! refuses `..` and escaping symlinks, so a component swapped between the
//! containment check and the open cannot escape the intended root.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use axum::http::StatusCode;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, OpenOptions, OpenOptionsExt};

use crate::acp::state::Event;

/// Keys under which the various agents stash a file path in a tool call's
/// `args_preview` JSON. Mirrors `pickStr` in `web/src/components/acp/ToolCards.tsx`.
const PATH_KEYS: [&str; 4] = ["file_path", "path", "filePath", "filename"];

/// Pull a file path out of a tool call's `args_preview` JSON. `None` when the
/// blob does not parse (it is capped at 16 KB at ingest) or carries no path key;
/// callers treat that as "path unknown", never "denied", and fall back to the
/// structured `diffs[].path`.
fn extract_path_from_args_preview(args_preview: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(args_preview).ok()?;
    let obj = value.as_object()?;
    for key in PATH_KEYS {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

/// Build the set of paths a session's agent touched, from its ACP event log.
///
/// Folds both `ToolCallStarted` and `ToolCallUpdated`: claude-agent-acp ships
/// the initial call with an empty `raw_input` and fills the path in on a later
/// update. Paths are collected exactly as the agent emitted them, so entries are
/// directly usable as absolute allow-set keys.
pub fn collect_touched_paths(events: &[(u64, Event)]) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let mut add = |s: &str| {
        if !s.is_empty() {
            out.insert(PathBuf::from(s));
        }
    };
    for (_seq, event) in events {
        match event {
            Event::ToolCallStarted { tool_call } => {
                if let Some(p) = extract_path_from_args_preview(&tool_call.args_preview) {
                    add(&p);
                }
                for d in &tool_call.diffs {
                    add(&d.path);
                }
                if let Some(mr) = &tool_call.memory_recall {
                    for p in &mr.paths {
                        add(p);
                    }
                }
            }
            Event::ToolCallUpdated {
                args_preview,
                diffs,
                ..
            } => {
                if let Some(ap) = args_preview {
                    if let Some(p) = extract_path_from_args_preview(ap) {
                        add(&p);
                    }
                }
                if let Some(diffs) = diffs {
                    for d in diffs {
                        add(&d.path);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Reject a relative request that tries to climb out of its root. Absolute
/// requests skip this (they are matched against roots / provenance instead).
fn has_traversal(requested: &Path) -> bool {
    requested.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

/// A read that passed confinement: the canonical target plus the directory it
/// must be opened beneath (a project root, or the file's own parent for a
/// provenance hit).
pub struct Confined {
    pub canonical: PathBuf,
    pub root: PathBuf,
}

/// Resolve and confine a requested path.
///
/// `project_roots` must already be canonicalized. `touched` is invoked only when
/// the target is not under a project root, because recovering the set replays
/// the whole session event log.
///
/// Security invariants (#3088): the path is canonicalized before any containment
/// check, and containment uses component-aware `Path::starts_with`, so
/// `/repo-evil` is not under `/repo`. The confined readers open the target
/// beneath the returned `root`, which they treat as a capability boundary.
pub fn confine_path(
    project_roots: &[PathBuf],
    touched: impl FnOnce() -> HashSet<PathBuf>,
    requested: &Path,
) -> Result<Confined, (StatusCode, &'static str)> {
    if requested.as_os_str().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "empty path"));
    }

    let canonical = if requested.is_absolute() {
        requested
            .canonicalize()
            .map_err(|_| (StatusCode::NOT_FOUND, "file not found"))?
    } else {
        if has_traversal(requested) {
            return Err((StatusCode::BAD_REQUEST, "path escapes project"));
        }
        // A relative request always resolves against the primary project root.
        // Multi-repo members are reached by their absolute worktree path.
        let root = project_roots
            .first()
            .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "no project root"))?;
        root.join(requested)
            .canonicalize()
            .map_err(|_| (StatusCode::NOT_FOUND, "file not found"))?
    };

    // Under a project root: open beneath that root (any file; the workspace).
    if let Some(root) = project_roots.iter().find(|r| canonical.starts_with(r)) {
        return Ok(Confined {
            canonical,
            root: root.clone(),
        });
    }

    // Outside every project root, so fall back to provenance. Compare canonical
    // forms so a symlinked-but-touched path still matches, and open it beneath
    // its own parent.
    let touched_hit = touched()
        .iter()
        .any(|t| t.canonicalize().map(|ct| ct == canonical).unwrap_or(false));
    if touched_hit {
        let root = canonical
            .parent()
            .ok_or((StatusCode::FORBIDDEN, "path has no parent"))?
            .to_path_buf();
        return Ok(Confined { canonical, root });
    }

    Err((StatusCode::FORBIDDEN, "path not readable for this session"))
}

/// Open a confined target beneath a `cap_std` capability directory, so the open
/// is race-safe against a component swapped after [`confine_path`] validated
/// containment.
///
/// Rejects non-regular files, so a blocking or endless special file cannot
/// stall or OOM the server. The open is non-blocking, so a FIFO cannot stall
/// it before the check.
fn open_confined(
    confined: &Confined,
) -> Result<(cap_std::fs::File, cap_std::fs::Metadata), (StatusCode, &'static str)> {
    let dir = Dir::open_ambient_dir(&confined.root, ambient_authority())
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "open root failed"))?;
    let rel = confined
        .canonical
        .strip_prefix(&confined.root)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "path not beneath root"))?;
    // cap_std refuses `..` and escaping symlinks, so this open stays beneath
    // `root` regardless of what changed since the canonicalize check.
    let file = dir
        .open_with(
            rel,
            OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NONBLOCK),
        )
        .map_err(|_| (StatusCode::NOT_FOUND, "file not found"))?;
    let meta = file
        .metadata()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "stat failed"))?;
    if !meta.is_file() {
        return Err((StatusCode::BAD_REQUEST, "not a regular file"));
    }
    Ok((file, meta))
}

/// Read a confined target as text with a byte cap, reading at most `cap + 1`
/// bytes to detect truncation. Binary content yields an empty string, matching
/// the diff endpoint.
pub fn read_confined(
    confined: &Confined,
    cap: usize,
) -> Result<(String, bool, bool), (StatusCode, &'static str)> {
    let (mut file, _) = open_confined(confined)?;

    let mut bytes = Vec::new();
    file.by_ref()
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "read failed"))?;

    let truncated = bytes.len() > cap;
    if truncated {
        bytes.truncate(cap);
    }
    let is_binary = bytes.contains(&0);
    let content = if is_binary {
        String::new()
    } else {
        String::from_utf8_lossy(&bytes).into_owned()
    };
    Ok((content, is_binary, truncated))
}

/// Read a confined target's raw bytes, refusing a file over `cap` bytes rather
/// than truncating it.
pub fn read_confined_bytes(
    confined: &Confined,
    cap: u64,
) -> Result<Vec<u8>, (StatusCode, &'static str)> {
    let too_large = (StatusCode::PAYLOAD_TOO_LARGE, "file too large");
    let (file, meta) = open_confined(confined)?;
    if meta.len() > cap {
        return Err(too_large);
    }
    let mut bytes = Vec::new();
    // The file can grow after the stat, so the read is bounded too.
    file.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "read failed"))?;
    if bytes.len() as u64 > cap {
        return Err(too_large);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::{DiffPreview, MemoryRecall, ToolCall};
    use std::fs;

    fn tool_call(args_preview: &str, diffs: Vec<&str>, recall: Vec<&str>) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: "Write".into(),
            kind: "edit".into(),
            args_preview: args_preview.into(),
            started_at: chrono::Utc::now(),
            parent_tool_call_id: None,
            memory_recall: if recall.is_empty() {
                None
            } else {
                Some(MemoryRecall {
                    mode: "recall".into(),
                    paths: recall.into_iter().map(String::from).collect(),
                    synthesized_text: None,
                })
            },
            diffs: diffs
                .into_iter()
                .map(|p| DiffPreview {
                    path: p.into(),
                    old_text: None,
                    new_text: None,
                    created_at: chrono::Utc::now(),
                })
                .collect(),
        }
    }

    #[test]
    fn collects_paths_from_all_channels() {
        let events = vec![
            (
                1u64,
                Event::ToolCallStarted {
                    tool_call: tool_call(r#"{"file_path":"/tmp/plan.md"}"#, vec![], vec![]),
                },
            ),
            (
                2,
                Event::ToolCallStarted {
                    tool_call: tool_call(
                        "{}",
                        vec!["/tmp/patched.rs"],
                        vec!["/home/u/.claude/mem.md"],
                    ),
                },
            ),
            (
                3,
                Event::ToolCallUpdated {
                    tool_call_id: "t3".into(),
                    title: None,
                    args_preview: Some(r#"{"path":"/tmp/late.txt"}"#.into()),
                    started_at: None,
                    diffs: Some(vec![DiffPreview {
                        path: "/tmp/diff-late.rs".into(),
                        old_text: None,
                        new_text: None,
                        created_at: chrono::Utc::now(),
                    }]),
                },
            ),
            // A 16 KB-truncated args_preview may be invalid JSON; that must not
            // panic or fabricate a path, and the diffs fallback still lands.
            (
                4,
                Event::ToolCallStarted {
                    tool_call: tool_call(r#"{"content":"unterminated"#, vec!["/tmp/x.rs"], vec![]),
                },
            ),
        ];
        let set = collect_touched_paths(&events);
        let expected: HashSet<PathBuf> = [
            "/tmp/plan.md",
            "/tmp/patched.rs",
            "/home/u/.claude/mem.md",
            "/tmp/late.txt",
            "/tmp/diff-late.rs",
            "/tmp/x.rs",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(set, expected);
    }

    #[test]
    fn extract_prefers_key_order() {
        assert_eq!(
            extract_path_from_args_preview(r#"{"filename":"b","file_path":"a"}"#),
            Some("a".into())
        );
        assert_eq!(extract_path_from_args_preview(r#"{"nope":"x"}"#), None);
        assert_eq!(extract_path_from_args_preview("not json"), None);
    }

    // Read a confined request end to end (confine then cap_std-backed read).
    fn read(
        roots: &[PathBuf],
        touched: &HashSet<PathBuf>,
        requested: &Path,
    ) -> Result<(String, bool, bool), (StatusCode, &'static str)> {
        let confined = confine_path(roots, || touched.clone(), requested)?;
        read_confined(&confined, 5_000_000)
    }

    #[test]
    fn allows_relative_in_project_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("a.md"), "# hi").unwrap();
        let roots = vec![root.clone()];
        let touched = HashSet::new();

        assert_eq!(read(&roots, &touched, Path::new("a.md")).unwrap().0, "# hi");
        assert_eq!(
            read(&roots, &touched, Path::new("../etc/passwd"))
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            read(&roots, &touched, Path::new("")).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn absolute_requires_project_or_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_root = outside.path().canonicalize().unwrap();

        let in_project = root.join("in.md");
        fs::write(&in_project, "in").unwrap();
        let touched_file = outside_root.join("plan.md");
        fs::write(&touched_file, "plan").unwrap();
        let untouched_file = outside_root.join("secret.md");
        fs::write(&untouched_file, "secret").unwrap();

        let roots = vec![root.clone()];
        let mut touched = HashSet::new();
        touched.insert(touched_file.clone());

        assert_eq!(read(&roots, &touched, &in_project).unwrap().0, "in");
        assert_eq!(read(&roots, &touched, &touched_file).unwrap().0, "plan");
        assert_eq!(
            read(&roots, &touched, &untouched_file).unwrap_err().0,
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn rejects_symlink_escape_and_sibling_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let secret_dir = tempfile::tempdir().unwrap();
        let secret = secret_dir.path().canonicalize().unwrap().join("id_rsa");
        fs::write(&secret, "KEY").unwrap();

        #[cfg(unix)]
        {
            let link = root.join("link.md");
            std::os::unix::fs::symlink(&secret, &link).unwrap();
            // canonicalize resolves the link to the secret, outside every root.
            assert_eq!(
                read(std::slice::from_ref(&root), &HashSet::new(), &link)
                    .unwrap_err()
                    .0,
                StatusCode::FORBIDDEN
            );
        }

        // A sibling dir sharing a string prefix is NOT under the root:
        // component-aware starts_with, not string prefix.
        let evil = PathBuf::from(format!("{}-evil", root.display()));
        fs::create_dir_all(&evil).ok();
        let evil_file = evil.join("x.md");
        fs::write(&evil_file, "x").unwrap();
        assert_eq!(
            read(std::slice::from_ref(&root), &HashSet::new(), &evil_file)
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        fs::remove_dir_all(&evil).ok();
    }

    #[test]
    fn binary_dir_and_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let roots = vec![root.clone()];
        let touched = HashSet::new();

        fs::write(root.join("b.bin"), [0u8, 1, 2, 3]).unwrap();
        let (content, is_binary, _) = read(&roots, &touched, Path::new("b.bin")).unwrap();
        assert!(is_binary && content.is_empty());

        fs::write(root.join("big.md"), "abcdef").unwrap();
        let confined = confine_path(&roots, || touched.clone(), Path::new("big.md")).unwrap();
        let (content, _, truncated) = read_confined(&confined, 3).unwrap();
        assert!(truncated && content.len() == 3);

        // A directory is not a regular file.
        fs::create_dir(root.join("sub")).unwrap();
        assert_eq!(
            read(&roots, &touched, Path::new("sub")).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn read_confined_bytes_is_lossless_and_refuses_over_cap() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let bytes = [0u8, 0xff, b'%', b'\n'];
        fs::write(root.join("b.bin"), bytes).unwrap();
        let confined = confine_path(
            std::slice::from_ref(&root),
            HashSet::new,
            Path::new("b.bin"),
        )
        .unwrap();

        assert_eq!(read_confined_bytes(&confined, 4).unwrap(), bytes);
        assert_eq!(
            read_confined_bytes(&confined, 3).unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    /// A blocking open of a FIFO waits for a writer, so the confined open must
    /// be non-blocking for the FIFO to reach the regular-file check.
    #[cfg(unix)]
    #[test]
    fn refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        nix::unistd::mkfifo(&root.join("pipe"), nix::sys::stat::Mode::S_IRWXU).unwrap();
        let confined =
            confine_path(std::slice::from_ref(&root), HashSet::new, Path::new("pipe")).unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read_confined_bytes(&confined, 1024).map(|_| ()));
        });
        let result = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("read blocked on the FIFO");
        assert_eq!(result.unwrap_err().0, StatusCode::BAD_REQUEST);
    }

    /// The capability open is the TOCTOU defense, so provoke it directly: hand
    /// `read_confined` a target that escapes `root` via a symlink, the shape a
    /// component swapped after `confine_path` would produce. `cap_std` re-checks
    /// every component at open time; a plain `File::open(canonical)` would
    /// follow the link and leak the outside file.
    #[cfg(unix)]
    #[test]
    fn read_confined_refuses_symlink_escaping_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().canonicalize().unwrap().join("id_rsa");
        fs::write(&secret, "KEY").unwrap();

        // A file, and a sibling symlink pointing out of the root.
        fs::write(root.join("ok.md"), "fine").unwrap();
        std::os::unix::fs::symlink(&secret, root.join("swapped.md")).unwrap();

        // Sanity: a real in-root file opens through the same path.
        let good = Confined {
            canonical: root.join("ok.md"),
            root: root.clone(),
        };
        assert_eq!(read_confined(&good, 5_000_000).unwrap().0, "fine");

        // The escaping symlink is refused at open time, not followed.
        let swapped = Confined {
            canonical: root.join("swapped.md"),
            root: root.clone(),
        };
        // The outside file's bytes never reach the caller: the open is refused
        // rather than followed.
        let result = read_confined(&swapped, 5_000_000);
        assert!(
            !matches!(&result, Ok((content, _, _)) if content == "KEY"),
            "capability open leaked the outside file"
        );
        assert_eq!(result.unwrap_err().0, StatusCode::NOT_FOUND);
    }

    /// The provenance set means replaying the whole event log, so it must not be
    /// built when the target resolves under a project root (the common case).
    #[test]
    fn provenance_is_not_built_for_a_project_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("a.md"), "x").unwrap();
        let mut called = false;

        let confined = confine_path(
            std::slice::from_ref(&root),
            || {
                called = true;
                HashSet::new()
            },
            Path::new("a.md"),
        )
        .unwrap();

        assert_eq!(confined.root, root);
        assert!(!called, "event log replayed for an in-project file");
    }
}
