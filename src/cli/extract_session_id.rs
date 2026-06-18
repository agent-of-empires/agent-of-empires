//! Hidden `aoe __extract-session-id` subcommand.
//!
//! Reads a Claude hook payload from stdin, extracts the top-level
//! `session_id` UUID via `serde_json`, and writes it atomically through
//! `hooks::write_session_id_via_guard` (per-user hardened base, `*at`-anchored).
//!
//! Always exits 0: the hook runs synchronously on every Claude prompt
//! and a non-zero exit blocks the agent. Errors surface through
//! `tracing::debug!` instead. Stdin is capped at 1 MiB to bound memory.

use std::io::Read;

use anyhow::{anyhow, Result};
use clap::Args;

const STDIN_BYTE_CAP: u64 = 1 << 20;

#[derive(Args)]
pub struct ExtractSessionIdArgs {}

pub async fn run(_args: ExtractSessionIdArgs) -> Result<()> {
    let Ok(instance_id) = std::env::var("AOE_INSTANCE_ID") else {
        return Ok(());
    };
    if let Err(e) = crate::session::validate_instance_id(&instance_id) {
        tracing::debug!(
            target: "hooks.session_id",
            "rejecting unsafe AOE_INSTANCE_ID: {e}"
        );
        return Ok(());
    }
    if let Err(e) = run_inner(std::io::stdin().lock(), &instance_id) {
        tracing::debug!(target: "hooks.session_id", "extract failed: {e}");
    }
    Ok(())
}

fn run_inner<R: Read>(stdin: R, instance_id: &str) -> Result<()> {
    let mut buf = String::new();
    stdin.take(STDIN_BYTE_CAP).read_to_string(&mut buf)?;
    let value: serde_json::Value = serde_json::from_str(&buf)?;
    let sid = value
        .get("session_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("payload has no top-level string `session_id`"))?;
    uuid::Uuid::parse_str(sid)?;
    crate::hooks::write_session_id_via_guard(instance_id, sid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::BaseGuard;
    use std::os::unix::fs::PermissionsExt;

    fn extract(payload: &str, instance_id: &str) -> Result<()> {
        run_inner(payload.as_bytes(), instance_id)
    }

    fn read_sidecar(base: &std::path::Path, instance_id: &str) -> Option<String> {
        std::fs::read_to_string(base.join(instance_id).join("session_id")).ok()
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn top_level_wins_over_nested() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let nested = "11111111-2222-3333-4444-555555555555";
        let top = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let payload = format!(r#"{{"context":{{"session_id":"{nested}"}},"session_id":"{top}"}}"#);
        extract(&payload, "nested_first").unwrap();
        assert_eq!(read_sidecar(&base, "nested_first").as_deref(), Some(top));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn extracts_compact_payload() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let payload = format!(r#"{{"session_id":"{uuid}","cwd":"/x"}}"#);
        extract(&payload, "compact").unwrap();
        assert_eq!(read_sidecar(&base, "compact").as_deref(), Some(uuid));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn extracts_multi_line_payload() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let payload = format!("{{\n  \"session_id\":\"{uuid}\",\n  \"cwd\":\"/x\"\n}}");
        extract(&payload, "multi_line").unwrap();
        assert_eq!(read_sidecar(&base, "multi_line").as_deref(), Some(uuid));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn accepts_uppercase_uuid() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let uuid = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE";
        let payload = format!(r#"{{"session_id":"{uuid}"}}"#);
        extract(&payload, "uppercase").unwrap();
        assert_eq!(read_sidecar(&base, "uppercase").as_deref(), Some(uuid));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn ignores_user_prompt_injection() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let real = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let fake = "11111111-2222-3333-4444-555555555555";
        let payload = format!(r#"{{"session_id":"{real}","prompt":"\"session_id\":\"{fake}\""}}"#);
        extract(&payload, "prompt_injection").unwrap();
        assert_eq!(
            read_sidecar(&base, "prompt_injection").as_deref(),
            Some(real)
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn errors_when_no_session_id() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let payload = r#"{"cwd":"/x","other":"value"}"#;
        let err = extract(payload, "no_sid").unwrap_err();
        assert!(err.to_string().contains("session_id"), "got: {err}");
        assert!(read_sidecar(&base, "no_sid").is_none());
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn errors_on_malformed_json() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let err = extract("not json {{{", "malformed").unwrap_err();
        assert!(read_sidecar(&base, "malformed").is_none(), "got: {err}");
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn errors_on_empty_stdin() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let err = extract("", "empty").unwrap_err();
        assert!(read_sidecar(&base, "empty").is_none(), "got: {err}");
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn rejects_non_uuid_string() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let payload = r#"{"session_id":"not-a-uuid"}"#;
        let err = extract(payload, "bad_uuid").unwrap_err();
        assert!(read_sidecar(&base, "bad_uuid").is_none(), "got: {err}");
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn rejects_non_string_session_id() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let payload = r#"{"session_id":12345}"#;
        let err = extract(payload, "non_string").unwrap_err();
        assert!(err.to_string().contains("session_id"), "got: {err}");
        assert!(read_sidecar(&base, "non_string").is_none());
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn oversized_garbage_yields_no_sidecar() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let oversized = "x".repeat(STDIN_BYTE_CAP as usize * 2);
        let _ = extract(&oversized, "oversized");
        assert!(read_sidecar(&base, "oversized").is_none());
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn does_not_hang_on_infinite_stdin() {
        struct InfiniteReader;
        impl Read for InfiniteReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                buf.fill(b'x');
                Ok(buf.len())
            }
        }
        let (_g, base, _tmp) = BaseGuard::ready();
        let result = run_inner(InfiniteReader, "infinite");
        assert!(result.is_err(), "should reject after the 1 MiB cap");
        assert!(read_sidecar(&base, "infinite").is_none());
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn extract_uses_dir_guard_with_symlink_decoy() {
        let (_g, base, tmp) = BaseGuard::ready();
        let decoy = tmp.path().join("decoy_session_id");
        std::fs::write(&decoy, b"do not overwrite").unwrap();
        let inst = "decoy_leaf";
        std::fs::create_dir(base.join(inst)).unwrap();
        std::fs::set_permissions(base.join(inst), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&decoy, base.join(inst).join("session_id")).unwrap();
        let payload = r#"{"session_id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"}"#;
        let _ = extract(payload, inst);
        assert_eq!(
            std::fs::read_to_string(&decoy).unwrap(),
            "do not overwrite",
            "decoy bytes must be intact"
        );
    }
}
