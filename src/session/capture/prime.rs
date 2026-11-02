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

/// A private print store must contain exactly one eligible root, never merely the newest.
fn select_prime_agent_session(
    sessions: Vec<PrimeAgentSession>,
    project_path: &str,
    exclusion: &HashSet<String>,
    launch_time_ms: f64,
) -> Result<String> {
    let canonical_match = canonicalize_or_raw(project_path);
    let mut candidates = sessions
        .into_iter()
        .filter(|session| !exclusion.contains(&session.id))
        .filter(|session| canonicalize_or_raw(&session.cwd) == canonical_match)
        .filter(|session| {
            (session.mtime_ms as f64) + PRIME_AGENT_MTIME_FLOOR_SLACK_MS >= launch_time_ms
        });
    let candidate = candidates
        .next()
        .ok_or_else(|| anyhow::anyhow!("No attributable Prime print session"))?;
    anyhow::ensure!(
        candidates.next().is_none(),
        "Ambiguous Prime print sessions"
    );
    Ok(candidate.id)
}

/// Caps one handed-over transcript on disk; the copy itself streams.
const PRIME_AGENT_HANDOFF_MAX_BYTES: usize = 512 * 1024 * 1024;

/// Open `relative` as a root session transcript of `id` recorded in `cwd`,
/// leaving the reader just past its header line so the whole file can be
/// copied from the one open that was checked.
fn open_root_transcript(
    root: &crate::session::AnchoredDir,
    relative: &Path,
    id: &str,
    cwd: &str,
) -> Option<(std::io::BufReader<std::fs::File>, Vec<u8>)> {
    let file = root
        .open_regular(relative, PRIME_AGENT_HANDOFF_MAX_BYTES)
        .ok()??;
    let mut reader = std::io::BufReader::new(file);
    let mut header = Vec::with_capacity(4096);
    (&mut reader)
        .take(PRIME_AGENT_HEADER_SCAN_BYTES.saturating_add(1))
        .read_until(b'\n', &mut header)
        .ok()?;
    let (header_id, header_cwd) = root_session_header(&header)?;
    (header_id == id && canonicalize_or_raw(&header_cwd) == canonicalize_or_raw(cwd))
        .then_some((reader, header))
}

fn open_unique_root_transcript(
    parent_sessions: &Path,
    parent_id: &str,
    cwd: &str,
) -> Result<(
    std::ffi::OsString,
    std::io::BufReader<std::fs::File>,
    Vec<u8>,
)> {
    use anyhow::Context as _;
    let parent = crate::session::AnchoredDir::open(parent_sessions)
        .context("opening the parent's Prime sessions")?;
    let names = parent
        .read_dir(
            Path::new(""),
            PRIME_AGENT_MAX_SESSION_FILES.saturating_add(1),
        )
        .context("listing the parent's Prime sessions")?;
    anyhow::ensure!(
        names.len() <= PRIME_AGENT_MAX_SESSION_FILES,
        "the parent's Prime store holds too many sessions to search"
    );
    let mut matches = names
        .into_iter()
        .filter(|name| Path::new(name).extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .filter(|name| open_root_transcript(&parent, Path::new(name), parent_id, cwd).is_some());
    let name = matches
        .next()
        .context("the parent's Prime transcript was not found in its store")?;
    anyhow::ensure!(
        matches.next().is_none(),
        "more than one Prime transcript names the parent conversation"
    );
    let (reader, header) = open_root_transcript(&parent, Path::new(&name), parent_id, cwd)
        .context("the parent's Prime transcript changed during the handoff")?;
    Ok((name, reader, header))
}

pub(crate) fn validate_root_transcript(
    parent_sessions: &Path,
    parent_id: &str,
    cwd: &str,
) -> Result<()> {
    open_unique_root_transcript(parent_sessions, parent_id, cwd).map(|_| ())
}

/// Copy the root transcript of `parent_id` from the parent's sessions directory
/// into `session_dir` of the child's private store, so `prime-agent --fork
/// <parent_id>` finds it there. Exactly one
/// transcript must carry that id and `cwd`; a copy already in place for the same
/// conversation is kept, any other entry at its name is refused, and the
/// parent's store is only read, never shared.
pub(crate) fn hand_off_root_transcript(
    parent_sessions: &Path,
    child_store: &Path,
    session_dir: &Path,
    parent_id: &str,
    cwd: &str,
) -> Result<()> {
    use anyhow::Context as _;
    use std::os::unix::fs::PermissionsExt as _;

    let (name, mut reader, header) = open_unique_root_transcript(parent_sessions, parent_id, cwd)?;
    let child = crate::session::AnchoredDir::open(child_store)
        .context("opening the child's Prime store")?
        .create_child(session_dir)?;
    let leaf = Path::new(&name);
    let published = child.publish_file(
        leaf,
        &mut header.as_slice().chain(&mut reader),
        std::fs::Permissions::from_mode(0o600),
        false,
        None,
    )?;
    anyhow::ensure!(
        published || open_root_transcript(&child, leaf, parent_id, cwd).is_some(),
        "another Prime session already occupies {} in the child's store",
        leaf.display()
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrimeRootPublication {
    Ready(String),
    /// A root publication whose transcript is positively absent, not unreadable.
    Pending(String),
}

/// A pending root preserves an empty-conversation boundary instead of scanning older history.
pub(crate) fn prime_agent_poll_fn_sandboxed(
    preferred_root: Option<Box<dyn Fn() -> Option<PrimeRootPublication> + Send + 'static>>,
    plan: crate::session::instance::PrimeAgentCapturePlan,
    instance_id: String,
    launch_time_ms: f64,
    extra_excludes: HashSet<crate::session::ConversationBinding>,
    source: Option<crate::session::ExecutionBinding>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let exclusion = super::compose_exclusion(&instance_id, &extra_excludes, source.as_ref());
        if let Some(root) = &preferred_root {
            return match root() {
                Some(PrimeRootPublication::Ready(id)) if !exclusion.contains(&id) => Some(id),
                _ => None,
            };
        }
        select_prime_agent_session(
                scan_prime_agent_sessions(&plan.store, &plan.session_dir),
                &plan.container_cwd,
                &exclusion,
                launch_time_ms,
            )
            .map_err(|error| {
                tracing::debug!(target: "session.capture", "sandbox Prime Agent capture failed: {error}")
            })
            .ok()
            .and_then(super::validated_session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::set_mtime_ms;
    use super::*;
    use std::io::Write as _;
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
    /// Two private stores, each with a `sessions` directory.
    fn stores() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::TempDir::new().unwrap();
        let (parent, child) = (tmp.path().join("parent"), tmp.path().join("child"));
        std::fs::create_dir_all(parent.join("sessions")).unwrap();
        std::fs::create_dir_all(&child).unwrap();
        (tmp, parent, child)
    }

    fn hand_off(parent: &Path, child: &Path, id: &str, cwd: &str) -> Result<()> {
        hand_off_root_transcript(
            &parent.join("sessions"),
            child,
            Path::new("sessions"),
            id,
            cwd,
        )
    }

    #[test]
    fn a_handoff_copies_only_the_parents_transcript_and_is_repeatable() {
        let (_tmp, parent, child) = stores();
        let sessions = parent.join("sessions");
        let source = write_prime_session(&sessions, "root.jsonl", "parent-id", "/workspace");
        write_prime_session(&sessions, "peer.jsonl", "peer-id", "/workspace");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&source)
            .unwrap()
            .write_all(b"{\"type\":\"message\"}\n")
            .unwrap();
        let before = std::fs::read(&source).unwrap();

        hand_off(&parent, &child, "parent-id", "/workspace").unwrap();
        let copied = child.join("sessions/root.jsonl");
        assert_eq!(std::fs::read(&copied).unwrap(), before);
        assert!(!child.join("sessions/peer.jsonl").exists());
        assert_eq!(
            std::fs::read(&source).unwrap(),
            before,
            "the parent is only read"
        );

        hand_off(&parent, &child, "parent-id", "/workspace").unwrap();
        assert_eq!(
            std::fs::read(&copied).unwrap(),
            before,
            "a retry keeps the copy"
        );
    }

    #[test]
    fn a_handoff_refuses_a_transcript_it_cannot_attribute_to_the_parent() {
        let (_tmp, parent, child) = stores();
        let sessions = parent.join("sessions");
        assert!(
            hand_off(&parent, &child, "parent-id", "/workspace").is_err(),
            "no transcript"
        );

        write_prime_session(&sessions, "one.jsonl", "parent-id", "/workspace");
        assert!(
            hand_off(&parent, &child, "parent-id", "/elsewhere").is_err(),
            "other cwd"
        );
        assert!(
            hand_off(&parent, &child, "other-id", "/workspace").is_err(),
            "other id"
        );

        write_prime_session(&sessions, "two.jsonl", "parent-id", "/workspace");
        assert!(
            hand_off(&parent, &child, "parent-id", "/workspace").is_err(),
            "ambiguous"
        );
        assert!(
            !child.join("sessions").exists()
                || std::fs::read_dir(child.join("sessions"))
                    .unwrap()
                    .next()
                    .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_handoff_refuses_a_symlinked_source_and_a_foreign_occupant() {
        use std::os::unix::fs::symlink;

        let (tmp, parent, child) = stores();
        let outside = write_prime_session(tmp.path(), "outside.jsonl", "parent-id", "/workspace");
        symlink(&outside, parent.join("sessions/link.jsonl")).unwrap();
        assert!(
            hand_off(&parent, &child, "parent-id", "/workspace").is_err(),
            "symlink"
        );

        write_prime_session(
            &parent.join("sessions"),
            "root.jsonl",
            "parent-id",
            "/workspace",
        );
        std::fs::create_dir_all(child.join("sessions")).unwrap();
        write_prime_session(
            &child.join("sessions"),
            "root.jsonl",
            "peer-id",
            "/workspace",
        );
        assert!(
            hand_off(&parent, &child, "parent-id", "/workspace").is_err(),
            "foreign occupant"
        );
    }

    #[test]
    fn print_capture_rejects_ambiguity_instead_of_resuming_the_newest_peer() {
        let temp = tempfile::tempdir().unwrap();
        let sessions = temp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let one = write_prime_session(&sessions, "one.jsonl", "root-one", "/workspace");
        set_mtime_ms(&one, 10_000);
        let choose = |exclude: &HashSet<String>| {
            select_prime_agent_session(
                scan_prime_agent_sessions(temp.path(), Path::new("sessions")),
                "/workspace",
                exclude,
                10_000.0,
            )
        };
        assert_eq!(choose(&HashSet::new()).unwrap(), "root-one");
        let two = write_prime_session(&sessions, "two.jsonl", "root-two", "/workspace");
        set_mtime_ms(&two, 20_000);
        assert!(choose(&HashSet::new()).is_err());
        assert_eq!(
            choose(&HashSet::from(["root-one".into()])).unwrap(),
            "root-two"
        );
        assert!(choose(&HashSet::from(["root-one".into(), "root-two".into()])).is_err());
    }
}
