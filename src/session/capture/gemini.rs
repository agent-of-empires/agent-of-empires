//! Gemini CLI chat capture from a mounted sandbox store.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha2::{Digest, Sha256};

use super::MAX_SESSION_ID_LEN;
use crate::session::AnchoredDir;

pub(crate) const GEMINI_SESSION_MAX_BYTES: usize = 8 * 1024 * 1024;
const GEMINI_METADATA_MAX_BYTES: usize = 128 * 1024;
pub(crate) const GEMINI_SCAN_MAX_CANDIDATES: usize = 4 * 1024;
const GEMINI_PROJECT_HASH_MAX_BYTES: usize = 128;

/// `(sessionId, projectHash)` from a whole JSON file or, for JSONL, its first line.
pub(crate) fn parse_gemini_session_json(content: &str) -> Option<(Option<String>, Option<String>)> {
    if content.len() > GEMINI_SESSION_MAX_BYTES {
        return None;
    }
    let extract = |v: &serde_json::Value, key: &str, max: usize| {
        v.get(key)
            .and_then(|x| x.as_str())
            .filter(|value| value.len() <= max)
            .map(String::from)
    };
    let parsed = match serde_json::from_str::<serde_json::Value>(content) {
        Ok(parsed) => parsed,
        Err(_) => {
            let first_line = content.lines().next()?;
            if first_line.len() > GEMINI_METADATA_MAX_BYTES {
                return None;
            }
            serde_json::from_str(first_line).ok()?
        }
    };
    Some((
        extract(&parsed, "sessionId", MAX_SESSION_ID_LEN),
        extract(&parsed, "projectHash", GEMINI_PROJECT_HASH_MAX_BYTES),
    ))
}

/// Session id falls back to the file stem.
fn extract_gemini_fields_anchored(
    root: &AnchoredDir,
    relative: &Path,
) -> Option<(Option<String>, Option<String>)> {
    let content = root
        .read_regular(relative, GEMINI_SESSION_MAX_BYTES)
        .ok()??;
    let content = String::from_utf8(content).ok()?;
    let (session_id, project_hash) = parse_gemini_session_json(&content)?;
    let session_id = session_id.or_else(|| {
        relative
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|value| value.len() <= MAX_SESSION_ID_LEN)
            .map(String::from)
    });
    Some((session_id, project_hash))
}

pub(crate) fn project_hash(cwd: &str) -> String {
    Sha256::digest(cwd.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Polls the mounted Gemini store for the newest post-launch chat in the cwd's project-hash directory.
pub(crate) fn gemini_poll_fn_sandboxed_store(
    store: PathBuf,
    container_cwd: String,
    instance_id: String,
    capture_floor: SystemTime,
    extra_excludes: HashSet<crate::session::ConversationBinding>,
    source: Option<crate::session::ExecutionBinding>,
) -> impl Fn() -> Option<String> + Send + 'static {
    let expected_hash = project_hash(&container_cwd);
    move || {
        let root = AnchoredDir::open(&store).ok()?;
        let chats = Path::new("tmp").join(&expected_hash).join("chats");
        let mut candidates = root
            .read_dir(&chats, GEMINI_SCAN_MAX_CANDIDATES)
            .ok()?
            .into_iter()
            .filter_map(|name| {
                let path = chats.join(name);
                let modified = root.regular_modified(&path).ok().flatten()?;
                let valid_name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("session-"));
                let valid_extension = matches!(
                    path.extension().and_then(|extension| extension.to_str()),
                    Some("json" | "jsonl")
                );
                (modified > capture_floor && valid_name && valid_extension)
                    .then_some((path, modified))
            })
            .collect::<Vec<_>>();
        candidates.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
        let exclusion = super::compose_exclusion(&instance_id, &extra_excludes, source.as_ref());
        candidates.into_iter().find_map(|(path, _)| {
            let (id, project_hash) = extract_gemini_fields_anchored(&root, &path)?;
            let id = id?;
            (project_hash.as_deref() == Some(expected_hash.as_str()) && !exclusion.contains(&id))
                .then_some(id)
                .and_then(super::validated_session_id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{capture_floor, set_mtime_ms};
    use super::*;

    fn poll(store: &Path, floor: u64) -> impl Fn() -> Option<String> {
        gemini_poll_fn_sandboxed_store(
            store.to_path_buf(),
            "/workspace".to_string(),
            "current".to_string(),
            capture_floor(floor),
            HashSet::new(),
            None,
        )
    }

    fn chats_dir(store: &Path) -> PathBuf {
        let chats = store
            .join("tmp")
            .join(project_hash("/workspace"))
            .join("chats");
        std::fs::create_dir_all(&chats).unwrap();
        chats
    }

    #[test]
    fn session_fields_from_json_jsonl_and_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let root = AnchoredDir::open(tmp.path()).unwrap();
        let jsonl = "{\"sessionId\":\"abc-123\",\"projectHash\":\"deadbeef\",\"kind\":\"main\"}\n{\"role\":\"user\"}\n";
        for (content, expected) in [
            (
                r#"{"sessionId": "abc-123", "projectHash": "deadbeef"}"#,
                Some((Some("abc-123"), Some("deadbeef"))),
            ),
            (jsonl, Some((Some("abc-123"), Some("deadbeef")))),
            (
                r#"{"projectHash": "deadbeef"}"#,
                Some((Some("session-42"), Some("deadbeef"))),
            ),
            ("not json", None),
        ] {
            std::fs::write(tmp.path().join("session-42.json"), content).unwrap();
            let fields = extract_gemini_fields_anchored(&root, Path::new("session-42.json"));
            let fields = fields
                .as_ref()
                .map(|(id, hash)| (id.as_deref(), hash.as_deref()));
            assert_eq!(fields, expected, "{content}");
        }
    }

    #[test]
    fn poller_claims_only_post_launch_matching_chat() {
        let tmp = tempfile::tempdir().unwrap();
        let chats = chats_dir(tmp.path());
        let hash = project_hash("/workspace");
        for (name, id, project_hash, mtime) in [
            ("stale", "gemini_stale", hash.as_str(), 1_000),
            ("wrong", "gemini_wrong", "wrong", 4_000),
            ("fresh", "gemini_fresh", hash.as_str(), 3_000),
        ] {
            let path = chats.join(format!("session-{name}.json"));
            std::fs::write(
                &path,
                format!(r#"{{"sessionId":"{id}","projectHash":"{project_hash}"}}"#),
            )
            .unwrap();
            set_mtime_ms(&path, mtime * 1000);
        }
        assert_eq!(poll(tmp.path(), 2_000)().as_deref(), Some("gemini_fresh"));
    }

    #[cfg(unix)]
    #[test]
    fn poller_skips_symlinks_fifo_and_oversized_json() {
        use super::super::test_support::{make_fifo, open_fifo_guard};
        use std::os::unix::fs::symlink;
        use std::time::{Duration, Instant};

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let chats = chats_dir(tmp.path());
        let hash = project_hash("/workspace");
        let good = chats.join("session-good.json");
        std::fs::write(
            &good,
            format!(r#"{{"sessionId":"gemini_good","projectHash":"{hash}"}}"#),
        )
        .unwrap();
        let outside_file = outside.path().join("outside.json");
        std::fs::write(
            &outside_file,
            format!(r#"{{"sessionId":"gemini_linked","projectHash":"{hash}"}}"#),
        )
        .unwrap();
        symlink(&outside_file, chats.join("session-linked.json")).unwrap();
        let fifo = chats.join("session-pipe.json");
        make_fifo(&fifo);
        let _fifo_guard = open_fifo_guard(&fifo);
        std::fs::write(
            chats.join("session-large.json"),
            vec![b' '; GEMINI_SESSION_MAX_BYTES + 1],
        )
        .unwrap();

        let store_poll = poll(tmp.path(), 100);
        let started = Instant::now();
        assert_eq!(store_poll().as_deref(), Some("gemini_good"));
        std::fs::remove_file(good).unwrap();
        assert_eq!(store_poll(), None);
        assert!(started.elapsed() < Duration::from_secs(2));

        let intermediate = tempfile::tempdir().unwrap();
        symlink(outside.path(), intermediate.path().join("tmp")).unwrap();
        assert_eq!(poll(intermediate.path(), 100)(), None);
    }
}
