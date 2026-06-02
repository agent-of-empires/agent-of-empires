//! Hidden `aoe __extract-session-id` subcommand.
//!
//! Reads a Claude hook payload from stdin, extracts the top-level
//! `session_id` UUID via `serde_json`, and writes it atomically to
//! `HOOK_STATUS_BASE/$AOE_INSTANCE_ID/session_id`.
//!
//! Always exits 0: the hook runs synchronously on every Claude prompt
//! and a non-zero exit blocks the agent. Errors surface through
//! `tracing::debug!` instead. Stdin is capped at 1 MiB to bound memory.

use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, Result};
use clap::Args;

const STDIN_BYTE_CAP: u64 = 1 << 20;

#[derive(Args)]
pub struct ExtractSessionIdArgs {}

pub async fn run(_args: ExtractSessionIdArgs) -> Result<()> {
    let Ok(instance_id) = std::env::var("AOE_INSTANCE_ID") else {
        return Ok(());
    };
    if !is_safe_instance_id(&instance_id) {
        tracing::debug!(
            target: "hooks.session_id",
            "rejecting unsafe AOE_INSTANCE_ID"
        );
        return Ok(());
    }
    if let Err(e) = run_inner(
        std::io::stdin().lock(),
        crate::hooks::HOOK_STATUS_BASE,
        &instance_id,
    ) {
        tracing::debug!(target: "hooks.session_id", "extract failed: {e}");
    }
    Ok(())
}

/// Reject any `AOE_INSTANCE_ID` that is not safe to use as a single
/// path component. Production IDs are 16 lowercase hex chars (per
/// `session::instance::generate_id`); the wider `[A-Za-z0-9_-]` allowlist
/// accommodates dev / test identifiers. Modeled on `validate_session_id`
/// in `cockpit::worker_registry`, which guards an identical threat model.
fn is_safe_instance_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn run_inner<R: Read>(stdin: R, base: &str, instance_id: &str) -> Result<()> {
    let mut buf = String::new();
    stdin.take(STDIN_BYTE_CAP).read_to_string(&mut buf)?;
    let value: serde_json::Value = serde_json::from_str(&buf)?;
    let sid = value
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("payload has no top-level string `session_id`"))?;
    uuid::Uuid::parse_str(sid)?;
    let dir = Path::new(base).join(instance_id);
    // Best-effort: the hook must never block the agent. If the dir cannot
    // be created, `atomic_write` below fails, the error propagates to
    // `run`, and `run` swallows it via `tracing::debug!`.
    let _ = std::fs::create_dir_all(&dir);
    crate::session::atomic_write(&dir.join("session_id"), sid.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn extract(payload: &str, instance_id: &str, base: &Path) -> Result<()> {
        run_inner(payload.as_bytes(), base.to_str().unwrap(), instance_id)
    }

    fn read_sidecar(base: &Path, instance_id: &str) -> Option<String> {
        std::fs::read_to_string(base.join(instance_id).join("session_id")).ok()
    }

    #[test]
    fn top_level_wins_over_nested() {
        let tmp = TempDir::new().unwrap();
        let nested = "11111111-2222-3333-4444-555555555555";
        let top = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let payload = format!(r#"{{"context":{{"session_id":"{nested}"}},"session_id":"{top}"}}"#);
        extract(&payload, "nested_first", tmp.path()).unwrap();
        assert_eq!(
            read_sidecar(tmp.path(), "nested_first").as_deref(),
            Some(top)
        );
    }

    #[test]
    fn extracts_compact_payload() {
        let tmp = TempDir::new().unwrap();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let payload = format!(r#"{{"session_id":"{uuid}","cwd":"/x"}}"#);
        extract(&payload, "compact", tmp.path()).unwrap();
        assert_eq!(read_sidecar(tmp.path(), "compact").as_deref(), Some(uuid));
    }

    #[test]
    fn extracts_multi_line_payload() {
        let tmp = TempDir::new().unwrap();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let payload = format!("{{\n  \"session_id\":\"{uuid}\",\n  \"cwd\":\"/x\"\n}}");
        extract(&payload, "multi_line", tmp.path()).unwrap();
        assert_eq!(
            read_sidecar(tmp.path(), "multi_line").as_deref(),
            Some(uuid)
        );
    }

    #[test]
    fn accepts_uppercase_uuid() {
        let tmp = TempDir::new().unwrap();
        let uuid = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE";
        let payload = format!(r#"{{"session_id":"{uuid}"}}"#);
        extract(&payload, "uppercase", tmp.path()).unwrap();
        assert_eq!(read_sidecar(tmp.path(), "uppercase").as_deref(), Some(uuid));
    }

    #[test]
    fn ignores_user_prompt_injection() {
        let tmp = TempDir::new().unwrap();
        let real = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let fake = "11111111-2222-3333-4444-555555555555";
        let payload = format!(r#"{{"session_id":"{real}","prompt":"\"session_id\":\"{fake}\""}}"#);
        extract(&payload, "prompt_injection", tmp.path()).unwrap();
        assert_eq!(
            read_sidecar(tmp.path(), "prompt_injection").as_deref(),
            Some(real)
        );
    }

    #[test]
    fn errors_when_no_session_id() {
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"cwd":"/x","other":"value"}"#;
        let err = extract(payload, "no_sid", tmp.path()).unwrap_err();
        assert!(err.to_string().contains("session_id"), "got: {err}");
        assert!(read_sidecar(tmp.path(), "no_sid").is_none());
    }

    #[test]
    fn errors_on_malformed_json() {
        let tmp = TempDir::new().unwrap();
        let err = extract("not json {{{", "malformed", tmp.path()).unwrap_err();
        assert!(
            read_sidecar(tmp.path(), "malformed").is_none(),
            "got: {err}"
        );
    }

    #[test]
    fn errors_on_empty_stdin() {
        let tmp = TempDir::new().unwrap();
        let err = extract("", "empty", tmp.path()).unwrap_err();
        assert!(read_sidecar(tmp.path(), "empty").is_none(), "got: {err}");
    }

    #[test]
    fn rejects_non_uuid_string() {
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"session_id":"not-a-uuid"}"#;
        let err = extract(payload, "bad_uuid", tmp.path()).unwrap_err();
        assert!(read_sidecar(tmp.path(), "bad_uuid").is_none(), "got: {err}");
    }

    #[test]
    fn rejects_non_string_session_id() {
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"session_id":12345}"#;
        let err = extract(payload, "non_string", tmp.path()).unwrap_err();
        assert!(err.to_string().contains("session_id"), "got: {err}");
        assert!(read_sidecar(tmp.path(), "non_string").is_none());
    }

    #[test]
    fn oversized_garbage_yields_no_sidecar() {
        let tmp = TempDir::new().unwrap();
        let oversized = "x".repeat(STDIN_BYTE_CAP as usize * 2);
        let _ = extract(&oversized, "oversized", tmp.path());
        assert!(read_sidecar(tmp.path(), "oversized").is_none());
    }

    #[test]
    fn does_not_hang_on_infinite_stdin() {
        struct InfiniteReader;
        impl Read for InfiniteReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf.fill(b'x');
                Ok(buf.len())
            }
        }
        let tmp = TempDir::new().unwrap();
        let result = run_inner(InfiniteReader, tmp.path().to_str().unwrap(), "infinite");
        assert!(result.is_err(), "should reject after the 1 MiB cap");
        assert!(read_sidecar(tmp.path(), "infinite").is_none());
    }

    #[test]
    fn rejects_path_unsafe_instance_id() {
        assert!(!is_safe_instance_id(""), "empty");
        assert!(!is_safe_instance_id(".."), "parent ref");
        assert!(!is_safe_instance_id("."), "current ref");
        assert!(!is_safe_instance_id("/etc"), "absolute path");
        assert!(!is_safe_instance_id("foo/bar"), "subdir traversal");
        assert!(!is_safe_instance_id("foo\\bar"), "backslash");
        assert!(!is_safe_instance_id("foo\0bar"), "NUL byte");
        assert!(!is_safe_instance_id("foo bar"), "whitespace");
        assert!(!is_safe_instance_id(&"x".repeat(65)), "over length cap");
    }

    #[test]
    fn accepts_production_and_test_instance_ids() {
        assert!(is_safe_instance_id("a3f7c2d1e4b89012"), "production hex");
        assert!(
            is_safe_instance_id("0123456789abcdef"),
            "production hex lower"
        );
        assert!(is_safe_instance_id("compact"), "test label");
        assert!(
            is_safe_instance_id("nested_first"),
            "test label with underscore"
        );
        assert!(is_safe_instance_id("a-b-c"), "test label with hyphen");
    }
}
