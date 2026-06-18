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
use uuid::Uuid;

use crate::session::Status;

use super::dir_guard;

/// Maximum age before a sidecar `session_id` file is considered stale.
pub(crate) const SESSION_ID_SIDECAR_MAX_AGE: Duration = Duration::from_secs(5 * 60);

/// Cap used when reading a status file. The legitimate values are short
/// tokens; an attacker-planted larger payload is irrelevant either way.
const STATUS_FILE_READ_CAP: usize = 64;
const SESSION_ID_FILE_READ_CAP: usize = 128;
const ATTENTION_FILE_READ_CAP: usize = 16 * 1024;

/// `<host base>/<instance_id>`. The base is the per-user directory
/// `/tmp/aoe-hooks-<euid>` resolved by `dir_guard::hook_base_path()`.
/// `Err` if `instance_id` fails `validate_instance_id`.
///
/// The path is informational (used by the sandbox bind-mount source string and
/// by debug logs); production I/O goes through `dir_guard` and never path-joins.
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

/// Read a Claude session UUID from the hook-written `session_id` sidecar.
///
/// Returns `None` when the file is absent, malformed (non-UUID), or older
/// than [`SESSION_ID_SIDECAR_MAX_AGE`].
pub fn read_hook_session_id(instance_id: &str) -> Option<String> {
    let dir = dir_guard::open_instance_dir_read_only(instance_id).ok()??;
    let meta = dir_guard::metadata_at(dir.as_fd(), "session_id").ok()??;
    let mtime = meta.modified().ok()?;
    if mtime.elapsed().ok()? > SESSION_ID_SIDECAR_MAX_AGE {
        return None;
    }
    let bytes =
        dir_guard::read_file_at(dir.as_fd(), "session_id", SESSION_ID_FILE_READ_CAP).ok()??;
    let id = std::str::from_utf8(&bytes).ok()?.trim().to_string();
    if Uuid::parse_str(&id).is_ok() {
        Some(id)
    } else {
        None
    }
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
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

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_running_status() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_running", "running");
        assert_eq!(read_hook_status("read_running"), Some(Status::Running));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_waiting_status() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_waiting", "waiting");
        assert_eq!(read_hook_status("read_waiting"), Some(Status::Waiting));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_idle_status() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_idle", "idle");
        assert_eq!(read_hook_status("read_idle"), Some(Status::Idle));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_error_status() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_err", "error");
        assert_eq!(read_hook_status("read_err"), Some(Status::Error));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_waiting_with_newline() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_nl", "waiting\n");
        assert_eq!(read_hook_status("read_nl"), Some(Status::Waiting));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_missing_file() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert_eq!(read_hook_status("nonexistent_instance_id"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_dangling_symlink() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let dir = dir_guard::open_instance_dir("dangling").unwrap();
        drop(dir);
        std::os::unix::fs::symlink("/nonexistent/target", base.join("dangling").join("status"))
            .unwrap();
        assert_eq!(read_hook_status("dangling"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_unexpected_content() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_status_via_guard("read_unexpected", "something_else");
        assert_eq!(read_hook_status("read_unexpected"), None);
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
    fn test_cleanup_nonexistent_dir() {
        let (_g, _, _tmp) = BaseGuard::ready();
        cleanup_hook_status_dir("nonexistent_cleanup_test");
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_hook_status_dir_path() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let dir = hook_status_dir("abc123").expect("test id must be allowlist-safe");
        assert_eq!(dir, base.join("abc123"));
    }

    fn write_attention_json(instance_id: &str, body: &str) {
        let dir = dir_guard::open_instance_dir(instance_id).unwrap();
        dir_guard::write_short(dir.as_fd(), "attention.json", body.as_bytes()).unwrap();
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_true() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json("urgent_true", r#"{"urgent":true,"urgent_reason":"x"}"#);
        assert!(read_hook_urgent("urgent_true"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_false_when_flag_missing() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json("urgent_missing", r#"{"tier":0}"#);
        assert!(!read_hook_urgent("urgent_missing"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_false_when_file_absent() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert!(!read_hook_urgent("urgent_no_file"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_false_when_malformed_json() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json("urgent_bad_json", "{ this is not json");
        assert!(!read_hook_urgent("urgent_bad_json"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_false_when_expires_passed() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_attention_json("urgent_expired", r#"{"urgent":true,"urgent_expires_at":1}"#);
        assert!(!read_hook_urgent("urgent_expired"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_urgent_true_when_expires_future() {
        let (_g, _, _tmp) = BaseGuard::ready();
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let body = format!(r#"{{"urgent":true,"urgent_expires_at":{}}}"#, future);
        write_attention_json("urgent_future", &body);
        assert!(read_hook_urgent("urgent_future"));
    }

    fn write_session_id_sidecar(instance_id: &str, content: &str) {
        let dir = dir_guard::open_instance_dir(instance_id).unwrap();
        dir_guard::write_atomic(dir.as_fd(), "session_id", content.as_bytes()).unwrap();
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_session_id_returns_some_when_fresh_uuid() {
        let (_g, _, _tmp) = BaseGuard::ready();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        write_session_id_sidecar("session_id_fresh", uuid);
        assert_eq!(
            read_hook_session_id("session_id_fresh").as_deref(),
            Some(uuid)
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_session_id_returns_none_when_absent() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert_eq!(
            read_hook_session_id("nonexistent_session_id_instance"),
            None
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_session_id_rejects_non_uuid() {
        let (_g, _, _tmp) = BaseGuard::ready();
        write_session_id_sidecar("session_id_garbage", "not-a-uuid");
        assert_eq!(read_hook_session_id("session_id_garbage"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_session_id_rejects_stale_file() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        write_session_id_sidecar("session_id_stale", uuid);
        let stale = std::time::SystemTime::now() - Duration::from_secs(10 * 60);
        std::fs::File::options()
            .write(true)
            .open(base.join("session_id_stale").join("session_id"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(stale))
            .unwrap();
        assert_eq!(read_hook_session_id("session_id_stale"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn test_read_hook_session_id_trims_trailing_whitespace() {
        let (_g, _, _tmp) = BaseGuard::ready();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        write_session_id_sidecar("session_id_trim", &format!("{uuid}\n"));
        assert_eq!(
            read_hook_session_id("session_id_trim").as_deref(),
            Some(uuid)
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn hook_status_dir_returns_err_for_unsafe_id() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert!(hook_status_dir("../etc").is_err());
        assert!(hook_status_dir("").is_err());
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn read_hook_status_returns_none_for_unsafe_id() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert_eq!(read_hook_status("../etc"), None);
        assert_eq!(read_hook_status(""), None);
        assert_eq!(read_hook_status("foo/bar"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn read_hook_session_id_returns_none_for_unsafe_id() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert_eq!(read_hook_session_id("../etc"), None);
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn read_hook_urgent_returns_false_for_unsafe_id() {
        let (_g, _, _tmp) = BaseGuard::ready();
        assert!(!read_hook_urgent("../etc"));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn cleanup_hook_status_dir_is_noop_for_unsafe_id() {
        let (_g, _, _tmp) = BaseGuard::ready();
        cleanup_hook_status_dir("../etc");
        cleanup_hook_status_dir("");
    }
}
