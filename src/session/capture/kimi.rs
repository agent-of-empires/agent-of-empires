//! Kimi Code session capture from a mounted sandbox store.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{canonicalize_or_raw, MAX_SESSION_ID_LEN};
use crate::session::AnchoredDir;

struct KimiSession {
    id: String,
    session_dir: String,
    work_dir: String,
}

const KIMI_INDEX_MAX_BYTES: usize = 8 * 1024 * 1024;
const KIMI_INDEX_MAX_LINE_BYTES: usize = 64 * 1024;
const KIMI_INDEX_MAX_LINES: usize = 32 * 1024;
const KIMI_INDEX_MAX_LIVE_SESSIONS: usize = 8 * 1024;
const KIMI_PATH_MAX_BYTES: usize = 16 * 1024;

/// Live sessions from the append-only index: later records win, `deleted` tombstones, bad lines are skipped.
fn read_kimi_session_index(root: &AnchoredDir, relative: &Path) -> Result<Vec<KimiSession>> {
    let content = root
        .read_regular(relative, KIMI_INDEX_MAX_BYTES)?
        .ok_or_else(|| {
            anyhow::anyhow!("Kimi session index is missing or not a bounded regular file")
        })?;

    let mut live: HashMap<String, (String, String)> = HashMap::new();
    for (index, raw_line) in content.split(|byte| *byte == b'\n').enumerate() {
        if index >= KIMI_INDEX_MAX_LINES {
            anyhow::bail!("Kimi session index exceeds the line limit");
        }
        if raw_line.len() > KIMI_INDEX_MAX_LINE_BYTES {
            continue;
        }
        let Some(value) = std::str::from_utf8(raw_line)
            .ok()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        else {
            continue;
        };
        let str_field = |key: &str, max: usize| {
            value
                .get(key)
                .and_then(|v| v.as_str())
                .filter(|value| value.len() <= max)
        };
        let Some(session_id) = str_field("sessionId", MAX_SESSION_ID_LEN) else {
            continue;
        };
        if value.get("deleted").and_then(|v| v.as_bool()) == Some(true) {
            live.remove(session_id);
            continue;
        }
        let (Some(session_dir), Some(work_dir)) = (
            str_field("sessionDir", KIMI_PATH_MAX_BYTES),
            str_field("workDir", KIMI_PATH_MAX_BYTES),
        ) else {
            continue;
        };
        if !live.contains_key(session_id) && live.len() >= KIMI_INDEX_MAX_LIVE_SESSIONS {
            continue;
        }
        live.insert(
            session_id.to_string(),
            (session_dir.to_string(), work_dir.to_string()),
        );
    }

    Ok(live
        .into_iter()
        .map(|(id, (session_dir, work_dir))| KimiSession {
            id,
            session_dir,
            work_dir,
        })
        .collect())
}

/// Strict launch floor: timestamp uncertainty fails closed.
const KIMI_MTIME_FLOOR_SLACK_MS: f64 = 0.0;

/// Polls the mounted Kimi store for the newest post-launch session whose work dir matches the container.
pub(crate) fn kimi_poll_fn_sandboxed_store(
    store: PathBuf,
    container_workdir: String,
    instance_id: String,
    launch_time_ms: f64,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let root = AnchoredDir::open(&store).ok()?;
        let exclusion = super::compose_exclusion(&instance_id, &extra_excludes);
        let sessions = read_kimi_session_index(&root, Path::new("session_index.jsonl")).ok()?;
        let canonical_match = canonicalize_or_raw(&container_workdir);
        sessions
            .into_iter()
            .filter(|session| !exclusion.contains(&session.id))
            .filter(|session| canonicalize_or_raw(&session.work_dir) == canonical_match)
            .filter_map(|session| {
                let leaf = Path::new(&session.session_dir).file_name()?;
                let mtime = root
                    .directory_modified(&Path::new("sessions").join(leaf))
                    .ok()
                    .flatten()?;
                let mtime_ms = crate::util::system_time_to_ms(mtime);
                ((mtime_ms as f64) + KIMI_MTIME_FLOOR_SLACK_MS >= launch_time_ms)
                    .then_some((session.id, mtime_ms))
            })
            .max_by_key(|candidate| candidate.1)
            .map(|candidate| candidate.0)
            .and_then(super::validated_session_id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::set_mtime_ms;
    use super::*;

    fn poll(store: &Path, launch_ms: f64) -> impl Fn() -> Option<String> {
        kimi_poll_fn_sandboxed_store(
            store.to_path_buf(),
            "/workspace".to_string(),
            "current".to_string(),
            launch_ms,
            HashSet::new(),
        )
    }

    #[test]
    fn index_applies_deletions_and_last_wins() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = AnchoredDir::open(tmp.path()).unwrap();
        assert!(read_kimi_session_index(&root, Path::new("nope.jsonl")).is_err());
        std::fs::write(
            tmp.path().join("session_index.jsonl"),
            concat!(
                r#"{"sessionId":"session_a","sessionDir":"/s/a-old","workDir":"/p/one"}"#,
                "\n",
                r#"{"sessionId":"session_a","sessionDir":"/s/a","workDir":"/p/one"}"#,
                "\n",
                r#"{"sessionId":"session_b","sessionDir":"/s/b","workDir":"/p/two"}"#,
                "\n",
                r#"{"sessionId":"session_b","deleted":true}"#,
                "\n",
                "not json at all\n",
                r#"{"sessionId":"session_c","workDir":"/p/three"}"#,
                "\n",
            ),
        )
        .unwrap();
        let sessions = read_kimi_session_index(&root, Path::new("session_index.jsonl")).unwrap();
        let by_id: Vec<_> = sessions
            .iter()
            .map(|s| (s.id.as_str(), s.session_dir.as_str()))
            .collect();
        assert_eq!(by_id, vec![("session_a", "/s/a")]);
    }

    #[test]
    fn poller_claims_only_post_launch_matching_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let mut index = String::new();
        for (leaf, mtime, work_dir) in [
            ("stale", 1_000, "/workspace"),
            ("boundary", 2_000, "/workspace"),
            ("wrong", 4_000, "/other"),
            ("fresh", 3_000, "/workspace"),
        ] {
            let path = sessions.join(leaf);
            std::fs::create_dir(&path).unwrap();
            set_mtime_ms(&path, mtime * 1000);
            index.push_str(&format!(
                "{{\"sessionId\":\"kimi_{leaf}\",\"sessionDir\":\"/sessions/{leaf}\",\"workDir\":\"{work_dir}\"}}\n"
            ));
        }
        std::fs::write(tmp.path().join("session_index.jsonl"), index).unwrap();
        assert_eq!(
            poll(tmp.path(), 2_000_001.0)().as_deref(),
            Some("kimi_fresh")
        );
    }

    #[cfg(unix)]
    #[test]
    fn poller_skips_hostile_index_and_session_directory() {
        use super::super::test_support::{make_fifo, open_fifo_guard};
        use std::os::unix::fs::symlink;
        use std::time::{Duration, Instant};

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let good = sessions.join("good");
        std::fs::create_dir(&good).unwrap();
        std::fs::create_dir(outside.path().join("linked")).unwrap();
        symlink(outside.path().join("linked"), sessions.join("linked")).unwrap();
        let index = tmp.path().join("session_index.jsonl");
        let index_content = concat!(
            r#"{"sessionId":"kimi_linked","sessionDir":"/sessions/linked","workDir":"/workspace"}"#,
            "\n",
            r#"{"sessionId":"kimi_good","sessionDir":"/sessions/good","workDir":"/workspace"}"#,
            "\n",
        );
        std::fs::write(&index, index_content).unwrap();
        let poll = poll(tmp.path(), 0.0);
        assert_eq!(poll().as_deref(), Some("kimi_good"));
        std::fs::remove_dir(good).unwrap();
        assert_eq!(poll(), None);

        std::fs::remove_file(&index).unwrap();
        let outside_index = outside.path().join("session_index.jsonl");
        std::fs::write(&outside_index, index_content).unwrap();
        symlink(&outside_index, &index).unwrap();
        assert_eq!(poll(), None);
        std::fs::remove_file(&index).unwrap();
        make_fifo(&index);
        let fifo_guard = open_fifo_guard(&index);
        let started = Instant::now();
        assert_eq!(poll(), None);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(fifo_guard);
        std::fs::remove_file(&index).unwrap();
        std::fs::write(&index, vec![b' '; KIMI_INDEX_MAX_BYTES + 1]).unwrap();
        assert_eq!(poll(), None);
    }
}
