//! Status file I/O for hooks-based agent status detection.
//!
//! Public reader/writer surface that delegates to `dir_guard` for every
//! file operation. The four readers (`read_hook_status`, `read_hook_session_id`,
//! `read_hook_urgent`, `cleanup_hook_status_dir`) and `hook_status_dir` are
//! the stable contract; their internals all ride `*at`-anchored I/O on a
//! verified host base directory (`/tmp/aoe-hooks-<euid>`).

use std::os::fd::AsFd;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;

use crate::session::Status;

use super::dir_guard;

/// Maximum age before a sidecar `session_id` file is considered stale.
pub(crate) const SESSION_ID_SIDECAR_MAX_AGE: Duration = Duration::from_secs(5 * 60);

/// Cap used when reading a status file. The legitimate values are short
/// tokens; an attacker-planted larger payload is irrelevant either way.
const STATUS_FILE_READ_CAP: usize = 64;
const SESSION_ID_FILE_MAX_PAYLOAD: usize = crate::session::capture::MAX_SESSION_ID_LEN + 1;
const SESSION_ID_FILE_READ_CAP: usize = SESSION_ID_FILE_MAX_PAYLOAD + 1;
const ATTENTION_FILE_READ_CAP: usize = 16 * 1024;

/// `<host base>/<instance_id>`. The base is the per-user directory
/// `/tmp/aoe-hooks-<euid>` resolved by `dir_guard::hook_base_path()`.
/// `Err` if `instance_id` fails `validate_instance_id`.
///
/// The path is informational (tests); production I/O goes
/// through `dir_guard` and never path-joins. The sandbox bind-mount source is
/// `dir_guard::ensure_instance_dir_path`, canonically resolved since #3240,
/// so on symlinked prefixes it spells differently from this lexical value.
pub fn hook_status_dir(instance_id: &str) -> Result<PathBuf> {
    crate::session::validate_instance_id(instance_id)?;
    Ok(dir_guard::hook_base_path().join(instance_id))
}

/// Read the hook-written status file for the given instance.
///
/// Returns `None` if the file doesn't exist, the symlink is forbidden, or
/// initialization of the per-user base failed (squatted or wrong-mode dir).
pub fn read_hook_status(instance_id: &str) -> Option<Status> {
    let dir = dir_guard::open_instance_dir_read_only(instance_id).ok()??;
    let bytes = dir_guard::read_file_at(dir.as_fd(), "status", STATUS_FILE_READ_CAP).ok()??;
    parse_status(&bytes)
}

/// Time since the hook status file was last written, i.e. how long the current
/// value has been standing.
///
/// The running-mapped hooks (`PreToolUse`, `UserPromptSubmit`, `ElicitationResult`)
/// rewrite the file on every fire, so a fresh mtime means the last write is
/// recent. The detection manifests' hook freshness bounds use this to tell a
/// genuinely fresh `running` (a turn that just started, spinner not yet
/// rendered) from a stale one that a missed idle hook left standing after the
/// turn ended.
///
/// Returns `None` when the file is absent or its mtime can't be read.
pub fn read_hook_status_age(instance_id: &str) -> Option<std::time::Duration> {
    let dir = dir_guard::open_instance_dir_read_only(instance_id).ok()??;
    let meta = dir_guard::metadata_at(dir.as_fd(), "status").ok()??;
    meta.modified().ok()?.elapsed().ok()
}

fn parse_status(bytes: &[u8]) -> Option<Status> {
    let trimmed = std::str::from_utf8(bytes).ok()?.trim();
    match trimmed {
        "running" => Some(Status::Running),
        "waiting" => Some(Status::Waiting),
        "idle" => Some(Status::Idle),
        "error" => Some(Status::Error),
        other => {
            tracing::warn!(target: "hooks.status", "Unexpected hook status value: {:?}", other);
            None
        }
    }
}

/// Read the transcript path a Pi pane published beside its `session_id`.
///
/// Pi indexes sessions by the cwd they started in, so a managed worktree that
/// moves leaves the transcript behind: the id alone would resolve to nothing
/// in the new directory and `--session-id` would create an empty conversation
/// under it. The absolute path still resolves. Absent, unreadable, oversized,
/// or relative values give `None`; no age check, since a path does not go
/// stale the way a fresh-conversation id does.
pub fn read_hook_session_path(instance_id: &str) -> Option<String> {
    let dir = dir_guard::open_instance_dir_read_only(instance_id).ok()??;
    let bytes =
        dir_guard::read_file_at(dir.as_fd(), "session_path", SESSION_PATH_FILE_READ_CAP).ok()??;
    let path = std::str::from_utf8(&bytes).ok()?.trim().to_string();
    (path.starts_with('/') && !path.contains('\n')).then_some(path)
}

/// Cap on the published transcript path, generous next to `PATH_MAX` and far
/// below anything worth reading into memory.
const SESSION_PATH_FILE_READ_CAP: usize = 8 * 1024;

/// Whether a `session_id` sidecar exists for this instance, whatever its age.
///
/// [`read_hook_session_id`] answers "is there a fresh id to adopt"; this
/// answers "does this pane publish its own id at all", which stays true while
/// a pane sits idle for longer than `SESSION_ID_SIDECAR_MAX_AGE`.
pub fn session_id_sidecar_exists(instance_id: &str) -> bool {
    (|| {
        let dir = dir_guard::open_instance_dir_read_only(instance_id).ok()??;
        dir_guard::metadata_at(dir.as_fd(), "session_id").ok()?
    })()
    .is_some()
}

/// Read an opaque native session ID from the hook-written sidecar.
///
/// Returns `None` when the file is absent, malformed, too large, or older than
/// `SESSION_ID_SIDECAR_MAX_AGE`.
pub fn read_hook_session_id(instance_id: &str) -> Option<String> {
    read_hook_session_id_within(instance_id, "session_id", Some(SESSION_ID_SIDECAR_MAX_AGE))
}

/// [`read_hook_session_id`] without the freshness window.
///
/// The window exists so a resume does not adopt an id from some earlier run,
/// which matters when the sidecar competes with other evidence. It has no
/// place in a final flush at stop: the pane published that id, nothing else
/// will, and the instance directory is about to be deleted. An idle pane whose
/// `/new` is older than the window would otherwise lose it.
pub fn read_hook_session_id_any_age(instance_id: &str) -> Option<String> {
    read_hook_session_id_within(instance_id, "session_id", None)
}

pub(crate) fn read_hook_sidecar_at(
    instance_id: &str,
    directory: &std::path::Path,
    leaf: &str,
    cap: usize,
    max_age: Option<std::time::Duration>,
) -> Option<Vec<u8>> {
    let dir = dir_guard::open_recorded_instance_dir(instance_id, directory).ok()??;
    let metadata = dir_guard::metadata_at(dir.as_fd(), leaf).ok()??;
    if metadata.len() > cap as u64 {
        return None;
    }
    if let Some(age) = max_age {
        if metadata.modified().ok()?.elapsed().ok()? > age {
            return None;
        }
    }
    let bytes = dir_guard::read_file_at(dir.as_fd(), leaf, cap.saturating_add(1)).ok()??;
    (bytes.len() <= cap).then_some(bytes)
}

pub(crate) fn read_hook_session_id_within(
    instance_id: &str,
    leaf: &str,
    max_age: Option<std::time::Duration>,
) -> Option<String> {
    let dir = dir_guard::open_instance_dir_read_only(instance_id).ok()??;
    let meta = dir_guard::metadata_at(dir.as_fd(), leaf).ok()??;
    // The one extra byte permits a conventional trailing newline. Checking the
    // metadata before the bounded read prevents a longer valid prefix from
    // being accepted as a different, truncated identity.
    if meta.len() > SESSION_ID_FILE_MAX_PAYLOAD as u64 {
        return None;
    }
    if let Some(max_age) = max_age {
        let mtime = meta.modified().ok()?;
        if mtime.elapsed().ok()? > max_age {
            return None;
        }
    }
    let bytes = dir_guard::read_file_at(dir.as_fd(), leaf, SESSION_ID_FILE_READ_CAP).ok()??;
    // Recheck the opened file, not only the earlier path metadata: the hook
    // writer publishes by atomic rename, so the leaf can change between them.
    if bytes.len() > SESSION_ID_FILE_MAX_PAYLOAD {
        return None;
    }
    let id = std::str::from_utf8(&bytes).ok()?.trim().to_string();
    crate::session::capture::is_valid_session_id(&id).then_some(id)
}

/// Read the urgent flag from the hook-written `attention.json`.
///
/// See the `attention-urgent` cx-script for the writer contract: `urgent`
/// boolean plus optional `urgent_expires_at` epoch seconds.
pub fn read_hook_urgent(instance_id: &str) -> bool {
    let Ok(Some(dir)) = dir_guard::open_instance_dir_read_only(instance_id) else {
        return false;
    };
    let Ok(Some(bytes)) =
        dir_guard::read_file_at(dir.as_fd(), "attention.json", ATTENTION_FILE_READ_CAP)
    else {
        return false;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    if !value
        .get("urgent")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return false;
    }
    if let Some(exp) = value.get("urgent_expires_at").and_then(|v| v.as_i64()) {
        let now = crate::util::now_secs() as i64;
        if now > exp {
            return false;
        }
    }
    true
}

/// Remove the hook status directory for a given instance (cleanup on stop/delete).
/// Symlink-safe via `dir_guard::remove_instance_dir` (`unlinkat` walk).
pub fn cleanup_hook_status_dir(instance_id: &str) {
    if let Err(e) = dir_guard::remove_instance_dir(instance_id) {
        tracing::warn!(target: "hooks.status",
            "Failed to cleanup hook status dir for {}: {}", instance_id, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::BaseGuard;
    use std::os::fd::AsFd;
    use std::time::Duration;

    fn write_status_via_guard(instance_id: &str, content: &str) {
        let dir = dir_guard::open_instance_dir(instance_id).unwrap();
        dir_guard::write_short(dir.as_fd(), "status", content.as_bytes()).unwrap();
    }

    /// Every status token the hook snippets write, a trailing newline, an
    /// unrecognised token, and no file at all.
    #[test]
    #[serial_test::serial(hook_base)]
    fn read_hook_status_maps_only_the_known_tokens() {
        let (_g, _, _tmp) = BaseGuard::ready();
        let cases = [
            ("read_running", Some("running"), Some(Status::Running)),
            ("read_waiting", Some("waiting"), Some(Status::Waiting)),
            ("read_idle", Some("idle"), Some(Status::Idle)),
            ("read_err", Some("error"), Some(Status::Error)),
            ("read_nl", Some("waiting\n"), Some(Status::Waiting)),
            ("read_unexpected", Some("something_else"), None),
            ("read_absent", None, None),
        ];
        for (id, written, expected) in cases {
            if let Some(content) = written {
                write_status_via_guard(id, content);
            }
            assert_eq!(read_hook_status(id), expected, "{id}");
        }
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_status_age_fresh_after_write() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("age_fresh", "running");
        let age = read_hook_status_age("age_fresh").expect("age present after write");
        assert!(
            age < Duration::from_secs(5),
            "just-written status should be fresh, got {age:?}"
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_status_age_none_when_absent() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert_eq!(read_hook_status_age("age_absent_instance"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_cleanup_existing_dir() {
        let (_g, base, _tmp) = BaseGuard::ready();
        write_status_via_guard("cleanup_existing", "running");
        let dir = base.join("cleanup_existing");
        assert!(dir.exists());
        cleanup_hook_status_dir("cleanup_existing");
        assert!(!dir.exists());
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_ensure_instance_dir_path_resolves_symlinked_prefix() {
        // #3240: podman machine shares host paths under their real
        // paths (/private, not /tmp) inside the VM, so the bind-mount source
        // handed to the runtime must be canonically resolved.
        use tempfile::TempDir;
        let cases = [
            ("symlinked base prefix resolves to the real path", true),
            (
                "already-real base prefix is passed through unchanged",
                false,
            ),
        ];
        for (label, symlinked) in cases {
            let tmp = TempDir::new().unwrap();
            let real_parent = tmp.path().join("real-parent");
            std::fs::create_dir(&real_parent).unwrap();
            let base = if symlinked {
                let link_parent = tmp.path().join("via-symlink");
                std::os::unix::fs::symlink(&real_parent, &link_parent).unwrap();
                link_parent.join("aoe-hooks")
            } else {
                real_parent.join("aoe-hooks")
            };
            let _g = BaseGuard::with_base(base.clone());
            let got = crate::hooks::ensure_instance_dir_path("pathres")
                .expect("instance dir must verify-and-create");
            let want = std::fs::canonicalize(&base).unwrap().join("pathres");
            assert_eq!(got, want, "{label}");
        }
    }

    fn write_attention_json(instance_id: &str, body: &str) {
        let dir = dir_guard::open_instance_dir(instance_id).unwrap();
        dir_guard::write_short(dir.as_fd(), "attention.json", body.as_bytes()).unwrap();
    }

    /// The flag holds only while it is set and unexpired; an absent or
    /// unparseable `attention.json` reads as not urgent.
    #[test]
    #[serial_test::serial(hook_base)]
    fn read_hook_urgent_holds_only_for_a_live_flag() {
        let (_g, _, _tmp) = BaseGuard::ready();
        let future = format!(
            r#"{{"urgent":true,"urgent_expires_at":{}}}"#,
            crate::util::now_secs() + 3600
        );
        let cases = [
            (
                "urgent_true",
                Some(r#"{"urgent":true,"urgent_reason":"x"}"#),
                true,
            ),
            ("urgent_future", Some(future.as_str()), true),
            ("urgent_missing", Some(r#"{"tier":0}"#), false),
            ("urgent_bad_json", Some("{ this is not json"), false),
            (
                "urgent_expired",
                Some(r#"{"urgent":true,"urgent_expires_at":1}"#),
                false,
            ),
            ("urgent_no_file", None, false),
        ];
        for (id, body, expected) in cases {
            if let Some(body) = body {
                write_attention_json(id, body);
            }
            assert_eq!(read_hook_urgent(id), expected, "{id}");
        }
    }

    fn write_session_id_sidecar(instance_id: &str, content: &str) {
        let dir = dir_guard::open_instance_dir(instance_id).unwrap();
        dir_guard::write_atomic(dir.as_fd(), "session_id", content.as_bytes()).unwrap();
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_session_id() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let maximum = "x".repeat(crate::session::capture::MAX_SESSION_ID_LEN);
        let too_long = "x".repeat(crate::session::capture::MAX_SESSION_ID_LEN + 1);
        let oversized_with_whitespace = format!("{maximum}  ");
        // Past the old 128-byte read window, so a truncated prefix would pass (#3678).
        let untruncated = format!("{}suffix", "x".repeat(128));
        let cases: [(&str, Option<String>, Option<&str>); 9] = [
            ("sid_fresh", Some(uuid.into()), Some(uuid)),
            ("sid_absent", None, None),
            ("sid_trim", Some(format!("{uuid}\n")), Some(uuid)),
            (
                "sid_opaque",
                Some("conversation_opaque.123".into()),
                Some("conversation_opaque.123"),
            ),
            ("sid_maximum", Some(maximum.clone()), Some(maximum.as_str())),
            ("sid_too_long", Some(too_long), None),
            ("sid_oversized_ws", Some(oversized_with_whitespace), None),
            (
                "sid_no_truncation",
                Some(untruncated.clone()),
                Some(untruncated.as_str()),
            ),
            ("sid_garbage", Some("unsafe id;".into()), None),
        ];
        for (id, written, expected) in cases {
            if let Some(content) = written {
                write_session_id_sidecar(id, &content);
            }
            assert_eq!(read_hook_session_id(id).as_deref(), expected, "{id}");
        }

        write_session_id_sidecar("sid_stale", uuid);
        let stale = std::time::SystemTime::now() - Duration::from_secs(10 * 60);
        std::fs::File::options()
            .write(true)
            .open(base.join("sid_stale").join("session_id"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stale))
            .unwrap();
        assert_eq!(read_hook_session_id("sid_stale"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn unsafe_ids_never_reach_the_filesystem() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let escape = base.join("..").join("etc_probe");
        std::fs::create_dir_all(&escape).unwrap();
        std::fs::write(escape.join("status"), "running").unwrap();
        for id in ["../etc_probe", "", "foo/bar"] {
            assert!(hook_status_dir(id).is_err(), "{id:?}");
            assert_eq!(read_hook_status(id), None, "{id:?}");
            assert_eq!(read_hook_session_id(id), None, "{id:?}");
            assert!(!read_hook_urgent(id), "{id:?}");
            cleanup_hook_status_dir(id);
        }
        assert!(escape.join("status").exists(), "cleanup must not escape");
        std::fs::remove_dir_all(&escape).unwrap();
    }
}
