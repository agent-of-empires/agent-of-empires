//! Codex rollout capture from a mounted sandbox store.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use uuid::Uuid;

use crate::session::AnchoredDir;

const CODEX_ROLLOUT_MAX_BYTES: usize = 16 * 1024 * 1024;
const CODEX_METADATA_MAX_BYTES: usize = 64 * 1024;
const CODEX_SCAN_MAX_DEPTH: usize = 4;
const CODEX_SCAN_MAX_ENTRIES: usize = 8 * 1024;
const CODEX_SCAN_MAX_CANDIDATES: usize = 4 * 1024;

/// The cwd from a rollout's metadata line. Any declared `session_id` / `id`
/// must equal the filename UUID, so a child rollout pointing at its parent is rejected.
fn parse_codex_cwd_from_json(line: &str, filename_uuid: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(line).ok()?;
    let payload = parsed.get("payload")?;
    let filename_id = Uuid::parse_str(filename_uuid).ok()?;
    for key in ["session_id", "id"] {
        if let Some(value) = payload.get(key) {
            if Uuid::parse_str(value.as_str()?).ok()? != filename_id {
                return None;
            }
        }
    }
    payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .filter(|cwd| !cwd.is_empty())
        .map(str::to_string)
}

/// The trailing 36-char UUID of `rollout-<timestamp>-<uuid>.jsonl[.zst]`.
fn extract_codex_uuid_from_filename(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let stem = stem.strip_suffix(".jsonl").unwrap_or(stem);
    let candidate = stem.get(stem.len().checked_sub(36)?..)?;
    Uuid::parse_str(candidate).ok()?;
    Some(candidate.to_string())
}

fn is_codex_rollout(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("jsonl")
        || path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".jsonl.zst"))
}

fn read_limited_first_line(reader: impl Read, max_bytes: usize) -> Option<String> {
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    let mut limited = reader.take(max_bytes.saturating_add(1) as u64);
    std::io::BufRead::read_until(
        &mut std::io::BufReader::new(&mut limited),
        b'\n',
        &mut bytes,
    )
    .ok()?;
    if bytes.len() > max_bytes {
        return None;
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    String::from_utf8(bytes).ok()
}

fn read_codex_metadata(root: &AnchoredDir, relative: &Path) -> Option<String> {
    let file = root
        .open_regular(relative, CODEX_ROLLOUT_MAX_BYTES)
        .ok()??;
    if relative.extension().and_then(|e| e.to_str()) == Some("zst") {
        let decoder = zstd::stream::read::Decoder::new(file).ok()?;
        read_limited_first_line(decoder, CODEX_METADATA_MAX_BYTES)
    } else {
        read_limited_first_line(file, CODEX_METADATA_MAX_BYTES)
    }
}

/// Collects rollouts under numeric date directories, bounded in depth, entries, and candidates.
fn collect_codex_sessions_anchored(
    root: &AnchoredDir,
    relative: &Path,
    depth: usize,
    visited: &mut usize,
    entries: &mut Vec<(PathBuf, SystemTime)>,
) -> Result<()> {
    if depth > CODEX_SCAN_MAX_DEPTH || *visited >= CODEX_SCAN_MAX_ENTRIES {
        return Ok(());
    }
    let names = root.read_dir(relative, CODEX_SCAN_MAX_ENTRIES.saturating_sub(*visited))?;
    for name in names {
        *visited = visited.saturating_add(1);
        if *visited > CODEX_SCAN_MAX_ENTRIES || entries.len() >= CODEX_SCAN_MAX_CANDIDATES {
            break;
        }
        let path = relative.join(&name);
        let numeric = name
            .to_str()
            .is_some_and(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()));
        if numeric && depth < CODEX_SCAN_MAX_DEPTH {
            if root.directory_modified(&path).ok().flatten().is_some() {
                let _ = collect_codex_sessions_anchored(root, &path, depth + 1, visited, entries);
            }
        } else if is_codex_rollout(&path) {
            if let Some(modified) = root.regular_modified(&path).ok().flatten() {
                entries.push((path, modified));
            }
        }
    }
    Ok(())
}

/// Polls the mounted Codex store for the newest post-launch rollout whose cwd matches the container.
pub(crate) fn codex_poll_fn_sandboxed_store(
    store: PathBuf,
    container_cwd: String,
    instance_id: String,
    capture_floor: SystemTime,
    extra_excludes: HashSet<String>,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        let root = AnchoredDir::open(&store).ok()?;
        let sessions = Path::new("sessions");
        root.directory_modified(sessions).ok().flatten()?;
        let mut entries = Vec::new();
        collect_codex_sessions_anchored(&root, sessions, 0, &mut 0, &mut entries).ok()?;
        entries.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
        let exclusion = super::compose_exclusion(&instance_id, &extra_excludes);
        entries.into_iter().find_map(|(relative, modified)| {
            if modified <= capture_floor {
                return None;
            }
            let id = extract_codex_uuid_from_filename(&relative)?;
            if exclusion.contains(&id) {
                return None;
            }
            let first_line = read_codex_metadata(&root, &relative)?;
            (parse_codex_cwd_from_json(&first_line, &id).as_deref() == Some(container_cwd.as_str()))
                .then_some(id)
                .and_then(super::validated_session_id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{capture_floor, set_mtime_ms};
    use super::*;
    use std::time::{Duration, Instant};

    fn poll(store: &Path, floor: u64) -> impl Fn() -> Option<String> {
        codex_poll_fn_sandboxed_store(
            store.to_path_buf(),
            "/workspace".to_string(),
            "current".to_string(),
            capture_floor(floor),
            HashSet::new(),
        )
    }

    fn rollout_line(id: &str, cwd: &str) -> String {
        format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}}}\n")
    }

    #[test]
    fn parse_codex_cwd_validates_declared_ids() {
        let root = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
        let child = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";
        let cwd = Some("/p".to_string());
        let meta = |id: &str, session_id: &str| {
            format!(r#"{{"payload":{{"id":"{id}","session_id":"{session_id}","cwd":"/p"}}}}"#)
        };
        let cases = [
            (
                "legacy without ids",
                r#"{"payload":{"cwd":"/p"}}"#.to_string(),
                root,
                cwd.clone(),
            ),
            ("matching ids", meta(root, root), root, cwd),
            (
                "child session_id points at parent",
                meta(child, root),
                child,
                None,
            ),
            ("id differs from filename", meta(child, root), root, None),
            ("malformed session_id", meta(root, "corrupt"), root, None),
            (
                "missing cwd",
                format!(r#"{{"payload":{{"id":"{root}"}}}}"#),
                root,
                None,
            ),
            ("invalid json", "not json".to_string(), root, None),
        ];
        for (name, line, filename_uuid, expected) in cases {
            assert_eq!(
                parse_codex_cwd_from_json(&line, filename_uuid),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn extract_codex_uuid_from_filename_requires_trailing_uuid() {
        let uuid = "abcdef01-2345-6789-abcd-ef0123456789";
        let path = PathBuf::from(format!("rollout-2025-03-06T12-00-00-{uuid}.jsonl"));
        assert_eq!(
            extract_codex_uuid_from_filename(&path).as_deref(),
            Some(uuid)
        );
        assert_eq!(
            extract_codex_uuid_from_filename(Path::new("my-thread-name.jsonl")),
            None
        );
    }

    #[test]
    fn poller_claims_newest_post_launch_matching_rollout() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions/2026/08/23");
        std::fs::create_dir_all(&sessions).unwrap();
        let fresh_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        for (name, id, cwd, mtime) in [
            (
                "stale",
                "11111111-2222-4333-8444-555555555555",
                "/workspace",
                1_000,
            ),
            (
                "wrong",
                "99999999-8888-4777-8666-555555555555",
                "/other",
                4_000,
            ),
            (
                "older",
                "22222222-2222-4333-8444-555555555555",
                "/workspace",
                2_500,
            ),
            ("fresh", fresh_id, "/workspace", 3_000),
        ] {
            let path = sessions.join(format!("rollout-{name}-{id}.jsonl"));
            std::fs::write(&path, rollout_line(id, cwd)).unwrap();
            set_mtime_ms(&path, mtime * 1000);
        }
        assert_eq!(poll(tmp.path(), 2_000)().as_deref(), Some(fresh_id));
    }

    #[cfg(unix)]
    #[test]
    fn poller_skips_hostile_artifacts_without_blocking() {
        use super::super::test_support::{make_fifo, open_fifo_guard};
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let good_id = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";
        let good = sessions.join(format!("rollout-good-{good_id}.jsonl"));
        std::fs::write(&good, rollout_line(good_id, "/workspace")).unwrap();
        set_mtime_ms(&good, 3_000_000);

        let linked_id = "11111111-2222-4333-8444-555555555555";
        let outside_file = outside
            .path()
            .join(format!("rollout-linked-{linked_id}.jsonl"));
        std::fs::write(&outside_file, rollout_line(linked_id, "/workspace")).unwrap();
        symlink(
            &outside_file,
            sessions.join(outside_file.file_name().unwrap()),
        )
        .unwrap();
        std::fs::create_dir(outside.path().join("06")).unwrap();
        symlink(outside.path().join("06"), sessions.join("2026")).unwrap();

        let fifo = sessions.join("rollout-fifo-22222222-3333-4444-8555-666666666666.jsonl");
        make_fifo(&fifo);
        let _fifo_guard = open_fifo_guard(&fifo);
        let oversized_id = "33333333-4444-4555-8666-777777777777";
        let mut oversized = rollout_line(oversized_id, "/workspace").into_bytes();
        oversized.resize(CODEX_ROLLOUT_MAX_BYTES + 1, b' ');
        std::fs::write(
            sessions.join(format!("rollout-large-{oversized_id}.jsonl")),
            oversized,
        )
        .unwrap();
        let bomb = zstd::stream::encode_all(
            std::io::Cursor::new(vec![b' '; CODEX_METADATA_MAX_BYTES + 1]),
            1,
        )
        .unwrap();
        std::fs::write(
            sessions.join("rollout-bomb-44444444-5555-4666-8777-888888888888.jsonl.zst"),
            bomb,
        )
        .unwrap();

        let poll = poll(tmp.path(), 100);
        let started = Instant::now();
        assert_eq!(poll().as_deref(), Some(good_id));
        std::fs::remove_file(good).unwrap();
        assert_eq!(poll(), None);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
