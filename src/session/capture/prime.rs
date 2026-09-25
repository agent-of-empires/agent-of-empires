//! Prime Agent session capture from a mounted sandbox store.

use std::collections::HashSet;
use std::io::{BufRead, Read};
use std::path::Path;

use anyhow::Result;

use super::canonicalize_or_raw;

/// Strict launch floor: timestamp uncertainty fails closed.
const PRIME_AGENT_MTIME_FLOOR_SLACK_MS: f64 = 0.0;
/// Bounds the header allocation for one hostile line.
pub(crate) const PRIME_AGENT_HEADER_SCAN_BYTES: u64 = 64 * 1024;
/// A private store holds few files; above this the scan fails closed.
pub(crate) const PRIME_AGENT_MAX_SESSION_FILES: usize = 256;

pub(crate) fn root_session_header(bytes: &[u8]) -> Option<(String, String)> {
    if bytes.is_empty() || u64::try_from(bytes.len()).ok()? > PRIME_AGENT_HEADER_SCAN_BYTES {
        return None;
    }
    let header = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    if header.get("type")?.as_str()? != "session" || header.get("rlmDepth")?.as_u64()? != 0 {
        return None;
    }
    Some((
        header.get("id")?.as_str()?.to_owned(),
        header.get("cwd")?.as_str()?.to_owned(),
    ))
}

struct PrimeAgentSession {
    id: String,
    cwd: String,
    mtime_ms: u64,
}

/// Root (`rlmDepth` 0) session headers under one anchored root. Symlinks,
/// non-regular files, oversized headers and stores above the entry cap are
/// rejected; mtime comes from the opened descriptor.
fn scan_prime_agent_sessions(store: &Path, session_dir: &Path) -> Vec<PrimeAgentSession> {
    let Ok(root) = crate::session::AnchoredDir::open(store) else {
        return Vec::new();
    };
    let Ok(names) = root.read_dir(session_dir, PRIME_AGENT_MAX_SESSION_FILES.saturating_add(1))
    else {
        return Vec::new();
    };
    if names.len() > PRIME_AGENT_MAX_SESSION_FILES {
        return Vec::new();
    }
    names
        .into_iter()
        .filter(|name| Path::new(name).extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter_map(|name| {
            let file = root
                .open_regular(&session_dir.join(&name), usize::MAX)
                .ok()??;
            let mtime_ms = file
                .metadata()
                .and_then(|metadata| metadata.modified())
                .map(crate::util::system_time_to_ms)
                .unwrap_or(0);
            let mut header = Vec::with_capacity(4096);
            std::io::BufReader::new(file)
                .take(PRIME_AGENT_HEADER_SCAN_BYTES.saturating_add(1))
                .read_until(b'\n', &mut header)
                .ok()?;
            let (id, cwd) = root_session_header(&header)?;
            Some(PrimeAgentSession { id, cwd, mtime_ms })
        })
        .collect()
}

/// Newest unexcluded session whose cwd matches `project_path` and was modified at or after the launch floor.
fn select_prime_agent_session(
    sessions: Vec<PrimeAgentSession>,
    project_path: &str,
    exclusion: &HashSet<String>,
    launch_time_ms: f64,
) -> Result<String> {
    let canonical_match = canonicalize_or_raw(project_path);
    let mut candidates: Vec<(String, u64)> = sessions
        .into_iter()
        .filter(|s| !exclusion.contains(&s.id))
        .filter(|s| canonicalize_or_raw(&s.cwd) == canonical_match)
        .filter(|s| (s.mtime_ms as f64) + PRIME_AGENT_MTIME_FLOOR_SLACK_MS >= launch_time_ms)
        .map(|s| (s.id, s.mtime_ms))
        .collect();
    candidates.sort_by_key(|c| std::cmp::Reverse(c.1));
    candidates
        .into_iter()
        .next()
        .map(|(id, _)| id)
        .ok_or_else(|| anyhow::anyhow!("No Prime Agent session found matching project path"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrimeRootPublication {
    Ready(String),
    /// A root publication whose transcript is positively absent, not unreadable.
    Pending(String),
}

/// A pending root preserves an empty-conversation boundary instead of scanning older history.
pub(crate) fn prime_agent_poll_fn_sandboxed(
    preferred_sidecar: Box<dyn Fn() -> Option<PrimeRootPublication> + Send + 'static>,
    plan: crate::session::instance::PrimeAgentCapturePlan,
    instance_id: String,
    launch_time_ms: f64,
    extra_excludes: HashSet<crate::session::ConversationBinding>,
    source: Option<crate::session::ExecutionBinding>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = super::compose_exclusion(&instance_id, &extra_excludes, source.as_ref());
        match preferred_sidecar() {
            Some(PrimeRootPublication::Ready(id)) if !exclusion.contains(&id) => Some(id),
            Some(PrimeRootPublication::Pending(id)) if !exclusion.contains(&id) => None,
            _ => select_prime_agent_session(
                scan_prime_agent_sessions(&plan.store, &plan.session_dir),
                &plan.container_cwd,
                &exclusion,
                launch_time_ms,
            )
            .map_err(|error| {
                tracing::debug!(target: "session.capture", "sandbox Prime Agent capture failed: {error}")
            })
            .ok()
            .and_then(super::validated_session_id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::set_mtime_ms;
    use super::*;
    use std::path::PathBuf;

    fn write_prime_session(dir: &Path, name: &str, id: &str, cwd: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"cwd\":\"{cwd}\",\"rlmDepth\":0}}\n"
            ),
        )
        .unwrap();
        path
    }

    fn scanned_ids(store: &Path) -> Vec<String> {
        let mut ids: Vec<_> = scan_prime_agent_sessions(store, Path::new("sessions"))
            .into_iter()
            .map(|s| s.id)
            .collect();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn scan_parses_root_headers_and_skips_noise() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        assert!(scanned_ids(tmp.path()).is_empty());
        write_prime_session(&sessions, "aaa.jsonl", "id-valid", "/tmp/proj");
        for (name, content) in [
            ("bbb.jsonl", "{\"type\":\"model_change\",\"id\":\"x\"}\n".to_string()),
            ("ccc.jsonl", "not json at all\n".to_string()),
            ("ddd.jsonl", "{\"type\":\"session\",\"id\":\"id-nocwd\",\"rlmDepth\":0}\n".to_string()),
            ("eee.txt", "{\"type\":\"session\",\"id\":\"id-txt\",\"cwd\":\"/tmp/proj\",\"rlmDepth\":0}\n".to_string()),
            (
                "big.jsonl",
                format!(
                    "{{\"type\":\"session\",\"id\":\"id-big\",\"cwd\":\"/tmp/proj\",\"rlmDepth\":0,\"pad\":\"{}\"}}\n",
                    "x".repeat(96 * 1024)
                ),
            ),
        ] {
            std::fs::write(sessions.join(name), content).unwrap();
        }
        assert!(
            scan_prime_agent_sessions(&tmp.path().join("nope"), Path::new("sessions")).is_empty()
        );
        assert_eq!(scanned_ids(tmp.path()), vec!["id-valid"]);

        for index in 0..=PRIME_AGENT_MAX_SESSION_FILES {
            std::fs::write(sessions.join(format!("{index:04}.txt")), b"noise").unwrap();
        }
        assert!(scanned_ids(tmp.path()).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn scan_rejects_fifo_symlinks_and_symlinked_directory() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        super::super::test_support::make_fifo(&sessions.join("fifo.jsonl"));
        let outside = tempfile::tempdir().unwrap();
        let target = write_prime_session(outside.path(), "peer.jsonl", "peer-id", "/workspace");
        symlink(&target, sessions.join("link.jsonl")).unwrap();
        assert!(scanned_ids(tmp.path()).is_empty());

        let store = tempfile::tempdir().unwrap();
        symlink(outside.path(), store.path().join("sessions")).unwrap();
        assert!(scanned_ids(store.path()).is_empty());
    }

    #[test]
    fn poller_filters_preferred_and_fallback_from_one_exclusion() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = PathBuf::from("custom-sessions");
        let sessions = tmp.path().join(&session_dir);
        std::fs::create_dir(&sessions).unwrap();
        for (name, id, cwd, mtime) in [
            ("stale", "prime_stale", "/workspace", 1_000),
            ("boundary", "prime_boundary", "/workspace", 2_000),
            ("wrong", "prime_wrong", "/other", 4_000),
            ("fresh", "prime_fresh", "/workspace", 3_000),
        ] {
            let path = write_prime_session(&sessions, &format!("{name}.jsonl"), id, cwd);
            set_mtime_ms(&path, mtime * 1000);
        }
        let child = sessions.join("newest-child.jsonl");
        std::fs::write(
            &child,
            "{\"type\":\"session\",\"id\":\"prime_child\",\"cwd\":\"/workspace\",\"rlmDepth\":1}\n",
        )
        .unwrap();
        set_mtime_ms(&child, 5_000_000);

        let poll = |preferred: Option<&'static str>, excluded: &[&str]| {
            prime_agent_poll_fn_sandboxed(
                Box::new(move || preferred.map(|id| PrimeRootPublication::Ready(id.to_string()))),
                crate::session::instance::PrimeAgentCapturePlan {
                    store: tmp.path().to_path_buf(),
                    session_dir: session_dir.clone(),
                    container_session_dir: "/root/.prime/agent/sessions".into(),
                    container_cwd: "/workspace".into(),
                },
                "current".to_string(),
                2_000_001.0,
                excluded
                    .iter()
                    .map(|id| crate::session::ConversationBinding::unknown(*id))
                    .collect(),
                None,
            )()
        };
        assert_eq!(poll(None, &[]).as_deref(), Some("prime_fresh"));
        assert_eq!(
            poll(Some("prime_parent"), &[]).as_deref(),
            Some("prime_parent")
        );
        assert_eq!(
            poll(Some("prime_parent"), &["prime_parent"]).as_deref(),
            Some("prime_fresh")
        );
        assert_eq!(
            poll(Some("prime_parent"), &["prime_parent", "prime_fresh"]),
            None
        );
    }
}
