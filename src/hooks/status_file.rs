//! Status file I/O for hooks-based agent status detection.
//!
//! Agent hooks write `running`, `waiting`, or `idle` to a well-known
//! file path so AoE can prefer hook status over tmux pane content. Callers may
//! still reconcile agent-specific hook gaps from pane text.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use uuid::Uuid;

use crate::session::Status;

use super::HOOK_STATUS_BASE;

/// Maximum age before a sidecar `session_id` file is considered stale.
pub(crate) const SESSION_ID_SIDECAR_MAX_AGE: Duration = Duration::from_secs(5 * 60);

/// `<HOOK_STATUS_BASE>/<instance_id>`. `Err` if `instance_id` fails
/// `validate_instance_id`.
pub fn hook_status_dir(instance_id: &str) -> Result<PathBuf> {
    crate::session::validate_instance_id(instance_id)?;
    Ok(PathBuf::from(HOOK_STATUS_BASE).join(instance_id))
}

/// Read the hook-written status file for the given instance.
///
/// Returns `None` if the file doesn't exist. When `Some`, the hook is
/// actively tracking the session and shell detection is unreliable
/// (wrapper scripts may keep a shell alive). Callers should still use
/// `is_pane_dead()` to detect truly dead panes.
pub fn read_hook_status(instance_id: &str) -> Option<Status> {
    let status_path = hook_status_dir(instance_id).ok()?.join("status");

    let content = std::fs::read_to_string(&status_path).ok()?;
    match content.trim() {
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
/// than [`SESSION_ID_SIDECAR_MAX_AGE`]. Filesystems that report
/// `mtime > now` (clock skew) are treated as stale.
pub fn read_hook_session_id(instance_id: &str) -> Option<String> {
    let path = hook_status_dir(instance_id).ok()?.join("session_id");
    let metadata = std::fs::metadata(&path).ok()?;
    let mtime = metadata.modified().ok()?;
    if mtime.elapsed().ok()? > SESSION_ID_SIDECAR_MAX_AGE {
        return None;
    }
    let content = std::fs::read_to_string(&path).ok()?;
    let id = content.trim().to_string();
    if Uuid::parse_str(&id).is_ok() {
        Some(id)
    } else {
        None
    }
}

/// Whether an `urgent` record's explicit `urgent_expires_at` (if present) has
/// already passed. A record with no expiry never expires here — the producer
/// (`attention-urgent`) always writes one, so legacy/expiry-less records stay
/// honored by design. Shared by [`read_hook_urgent`] and
/// [`read_hook_attention`] so the auto-expiry rule has a single source of truth.
fn urgent_expired(value: &serde_json::Value) -> bool {
    let Some(exp) = value.get("urgent_expires_at").and_then(|v| v.as_i64()) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    now > exp
}

/// Read the urgent flag from the hook-written `attention.json` for the given
/// instance. Set by the `attention-urgent` script (cx-scripts) when the agent
/// surfaces something genuinely time-sensitive (expiring device code, hard
/// deadline, blocking outage). Returns false when the file is missing,
/// malformed, missing the `urgent` flag, or when `urgent_expires_at` has
/// already passed (auto-expiry; keeps stale flags from pinning the row
/// forever after the deadline lapses).
pub fn read_hook_urgent(instance_id: &str) -> bool {
    let Ok(dir) = hook_status_dir(instance_id) else {
        return false;
    };
    let path = dir.join("attention.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };
    if !value
        .get("urgent")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return false;
    }
    !urgent_expired(&value)
}

/// Per-session activity/attention metadata parsed from `attention.json`, which
/// the Claude attention-signal hook rewrites every turn. Powers the
/// `last_activity_*` fields in `aoe list`/`status --json` so consumers can see
/// how long ago a session was active and what it last did without scraping
/// panes. Every field is optional: a session not running under the hook (or
/// whose file predates a field) simply omits it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookAttention {
    /// Attention tier (0 = most urgent). From `tier`.
    pub tier: Option<i64>,
    /// What kind of event last fired: `turn_complete`, `tool_invoke`, or
    /// `tool_error`. From `reason`.
    pub reason: Option<String>,
    /// Unix seconds of the most recent hook event = last activity. From `since`.
    pub since: Option<i64>,
    /// Tool name on the last Pre/PostToolUse event. From `tool`. After a
    /// toolless turn-complete this reflects the *prior* tool; `since` is always
    /// the true last-activity time.
    pub tool: Option<String>,
    /// Unix seconds when this attention record expires. From `expires_at`.
    pub expires_at: Option<i64>,
    /// Honored `urgent` flag — true only when set AND `urgent_expires_at` (if
    /// present) has not passed, matching [`read_hook_urgent`].
    pub urgent: bool,
}

/// Read the full attention record for an instance from `attention.json`.
///
/// Returns `None` when the file is absent or unparseable; individual fields are
/// `None`/false when their key is missing. Shares the read path and urgent
/// auto-expiry rule with [`read_hook_urgent`] — callers that need only the
/// urgent bool should keep using that lighter function (it is on the polling
/// hot path), while list/status JSON uses this to emit the richer row.
pub fn read_hook_attention(instance_id: &str) -> Option<HookAttention> {
    let dir = hook_status_dir(instance_id).ok()?;
    let path = dir.join("attention.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let value = serde_json::from_str::<serde_json::Value>(&content).ok()?;

    let urgent = value
        .get("urgent")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        && !urgent_expired(&value);

    Some(HookAttention {
        tier: value.get("tier").and_then(|v| v.as_i64()),
        reason: value
            .get("reason")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        since: value.get("since").and_then(|v| v.as_i64()),
        tool: value
            .get("tool")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        expires_at: value.get("expires_at").and_then(|v| v.as_i64()),
        urgent,
    })
}

/// Remove the hook status directory for a given instance (cleanup on stop/delete).
pub fn cleanup_hook_status_dir(instance_id: &str) {
    let dir = match hook_status_dir(instance_id) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(target: "hooks.status",
                "Skipping hook cleanup for invalid instance id: {e}");
            return;
        }
    };
    if dir.exists() {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            tracing::warn!(target: "hooks.status", "Failed to cleanup hook status dir {}: {}", dir.display(), e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn setup_status_file(instance_id: &str, content: &str) -> PathBuf {
        let dir = hook_status_dir(instance_id).expect("test id must be allowlist-safe");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("status");
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        dir
    }

    #[test]
    fn test_read_running_status() {
        let id = "test_read_running";
        let dir = setup_status_file(id, "running");
        assert_eq!(read_hook_status(id), Some(Status::Running));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_waiting_status() {
        let id = "test_read_waiting";
        let dir = setup_status_file(id, "waiting");
        assert_eq!(read_hook_status(id), Some(Status::Waiting));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_idle_status() {
        let id = "test_read_idle";
        let dir = setup_status_file(id, "idle");
        assert_eq!(read_hook_status(id), Some(Status::Idle));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_error_status() {
        let id = "test_read_error";
        let dir = setup_status_file(id, "error");
        assert_eq!(read_hook_status(id), Some(Status::Error));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_waiting_with_newline() {
        let id = "test_read_newline";
        let dir = setup_status_file(id, "waiting\n");
        assert_eq!(read_hook_status(id), Some(Status::Waiting));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_missing_file() {
        assert_eq!(read_hook_status("nonexistent_instance_id"), None);
    }

    #[test]
    fn test_read_dangling_symlink() {
        let id = "test_dangling_symlink";
        let dir = hook_status_dir(id).expect("test id must be allowlist-safe");
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("/nonexistent/target", dir.join("status")).unwrap();
        assert_eq!(read_hook_status(id), None);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_unexpected_content() {
        let id = "test_read_unexpected";
        let dir = setup_status_file(id, "something_else");
        assert_eq!(read_hook_status(id), None);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_cleanup_existing_dir() {
        let id = "test_cleanup_existing";
        let dir = setup_status_file(id, "running");
        assert!(dir.exists());
        cleanup_hook_status_dir(id);
        assert!(!dir.exists());
    }

    #[test]
    fn test_cleanup_nonexistent_dir() {
        // Should not panic
        cleanup_hook_status_dir("nonexistent_cleanup_test");
    }

    #[test]
    fn test_hook_status_dir_path() {
        let dir = hook_status_dir("abc123").expect("test id must be allowlist-safe");
        assert_eq!(dir, PathBuf::from("/tmp/aoe-hooks/abc123"));
    }

    fn write_attention_json(instance_id: &str, body: &str) -> PathBuf {
        let dir = hook_status_dir(instance_id).expect("test id must be allowlist-safe");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("attention.json");
        fs::write(&path, body).unwrap();
        dir
    }

    #[test]
    fn test_read_hook_urgent_true() {
        let id = "test_urgent_true";
        let dir = write_attention_json(id, r#"{"urgent":true,"urgent_reason":"x"}"#);
        assert!(read_hook_urgent(id));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_hook_urgent_false_when_flag_missing() {
        let id = "test_urgent_missing";
        let dir = write_attention_json(id, r#"{"tier":0}"#);
        assert!(!read_hook_urgent(id));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_hook_urgent_false_when_file_absent() {
        // No setup; directory doesn't exist
        assert!(!read_hook_urgent("test_urgent_no_file"));
    }

    #[test]
    fn test_read_hook_urgent_false_when_malformed_json() {
        let id = "test_urgent_bad_json";
        let dir = write_attention_json(id, "{ this is not json");
        assert!(!read_hook_urgent(id));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_hook_urgent_false_when_expires_passed() {
        let id = "test_urgent_expired";
        // urgent_expires_at = 1 (epoch=1970), well in the past
        let dir = write_attention_json(id, r#"{"urgent":true,"urgent_expires_at":1}"#);
        assert!(!read_hook_urgent(id));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_hook_urgent_true_when_expires_future() {
        let id = "test_urgent_future";
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let body = format!(r#"{{"urgent":true,"urgent_expires_at":{}}}"#, future);
        let dir = write_attention_json(id, &body);
        assert!(read_hook_urgent(id));
        fs::remove_dir_all(dir).ok();
    }

    fn write_session_id_sidecar(instance_id: &str, content: &str) -> PathBuf {
        let dir = hook_status_dir(instance_id).expect("test id must be allowlist-safe");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("session_id"), content).unwrap();
        dir
    }

    #[test]
    fn test_read_hook_session_id_returns_some_when_fresh_uuid() {
        let id = "test_session_id_fresh";
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let dir = write_session_id_sidecar(id, uuid);
        assert_eq!(read_hook_session_id(id), Some(uuid.to_string()));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_hook_session_id_returns_none_when_absent() {
        assert_eq!(
            read_hook_session_id("nonexistent_session_id_instance"),
            None
        );
    }

    #[test]
    fn test_read_hook_session_id_rejects_non_uuid() {
        let id = "test_session_id_garbage";
        let dir = write_session_id_sidecar(id, "not-a-uuid");
        assert_eq!(read_hook_session_id(id), None);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_hook_session_id_rejects_stale_file() {
        let id = "test_session_id_stale";
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let dir = write_session_id_sidecar(id, uuid);
        let stale = std::time::SystemTime::now() - Duration::from_secs(10 * 60);
        fs::File::options()
            .write(true)
            .open(dir.join("session_id"))
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(stale))
            .unwrap();
        assert_eq!(read_hook_session_id(id), None);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_hook_session_id_trims_trailing_whitespace() {
        let id = "test_session_id_trim";
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let dir = write_session_id_sidecar(id, &format!("{uuid}\n"));
        assert_eq!(read_hook_session_id(id), Some(uuid.to_string()));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn hook_status_dir_returns_err_for_unsafe_id() {
        assert!(hook_status_dir("../etc").is_err());
        assert!(hook_status_dir("").is_err());
    }

    #[test]
    fn read_hook_status_returns_none_for_unsafe_id() {
        assert_eq!(read_hook_status("../etc"), None);
        assert_eq!(read_hook_status(""), None);
        assert_eq!(read_hook_status("foo/bar"), None);
    }

    #[test]
    fn read_hook_session_id_returns_none_for_unsafe_id() {
        assert_eq!(read_hook_session_id("../etc"), None);
    }

    #[test]
    fn read_hook_urgent_returns_false_for_unsafe_id() {
        assert!(!read_hook_urgent("../etc"));
    }

    #[test]
    fn test_read_hook_attention_full_record() {
        let id = "test_attention_full";
        let future = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let body = format!(
            r#"{{"tier":0,"reason":"tool_invoke","since":1700000000,"tool":"Bash","expires_at":{future},"urgent":true,"urgent_expires_at":{future}}}"#
        );
        let dir = write_attention_json(id, &body);
        let att = read_hook_attention(id).expect("record should parse");
        assert_eq!(att.tier, Some(0));
        assert_eq!(att.reason.as_deref(), Some("tool_invoke"));
        assert_eq!(att.since, Some(1_700_000_000));
        assert_eq!(att.tool.as_deref(), Some("Bash"));
        assert_eq!(att.expires_at, Some(future as i64));
        assert!(att.urgent);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_hook_attention_none_when_absent() {
        assert_eq!(read_hook_attention("test_attention_no_file"), None);
    }

    #[test]
    fn test_read_hook_attention_partial_record() {
        // turn_complete with no tool: tool is None, since/reason present.
        let id = "test_attention_partial";
        let dir = write_attention_json(id, r#"{"reason":"turn_complete","since":1700000123}"#);
        let att = read_hook_attention(id).expect("record should parse");
        assert_eq!(att.reason.as_deref(), Some("turn_complete"));
        assert_eq!(att.since, Some(1_700_000_123));
        assert_eq!(att.tool, None);
        assert_eq!(att.tier, None);
        assert!(!att.urgent);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_hook_attention_urgent_respects_expiry() {
        // urgent flag set but past its expiry => urgent must be false, while
        // the rest of the record still parses.
        let id = "test_attention_urgent_expired";
        let dir = write_attention_json(
            id,
            r#"{"urgent":true,"urgent_expires_at":1,"since":1700000000}"#,
        );
        let att = read_hook_attention(id).expect("record should parse");
        assert!(!att.urgent);
        assert_eq!(att.since, Some(1_700_000_000));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn test_read_hook_attention_none_when_malformed() {
        let id = "test_attention_bad_json";
        let dir = write_attention_json(id, "{ not json");
        assert_eq!(read_hook_attention(id), None);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cleanup_hook_status_dir_is_noop_for_unsafe_id() {
        cleanup_hook_status_dir("../etc");
        cleanup_hook_status_dir("");
    }
}
