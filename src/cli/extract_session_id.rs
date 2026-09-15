//! Hidden `aoe __extract-session-id` subcommand.
//!
//! Reads an agent hook payload from stdin, extracts the configured top-level
//! native identity field, validates its shell-safe storage form, and writes it
//! atomically through the hardened hook sidecar. Hooks fired by a nested agent
//! that inherited the pane's identity variables are ignored. Hook failures never
//! block agents. Stdin is capped at 1 MiB to bound memory.

use std::io::Read;

use anyhow::{anyhow, Result};
use clap::Args;

const STDIN_BYTE_CAP: u64 = 1 << 20;
const MAX_ANCESTORS: usize = 64;

#[derive(Args)]
pub struct ExtractSessionIdArgs {
    #[arg(long, value_enum, default_value = "session-id")]
    field: crate::agents::HookIdentityField,
}

pub async fn run(args: ExtractSessionIdArgs) -> Result<()> {
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
    if !fired_by_pane_agent() {
        tracing::debug!(
            target: "hooks.session_id",
            "ignoring hook from a nested agent process"
        );
        return Ok(());
    }
    if let Err(e) = run_inner(std::io::stdin().lock(), &instance_id, args.field) {
        tracing::debug!(target: "hooks.session_id", "extract failed: {e}");
    }
    Ok(())
}

/// Every process the pane agent starts inherits `AOE_AGENT_PID` and
/// `AOE_AGENT_BIN`, including another agent run from its shell tool, so the
/// hook belongs to the pane only when the walk up to the launched pid passes
/// at most one agent process. Panes launched without them keep writing.
fn fired_by_pane_agent() -> bool {
    let agent_pid = std::env::var("AOE_AGENT_PID")
        .ok()
        .and_then(|pid| pid.parse().ok());
    let agent_bin = std::env::var("AOE_AGENT_BIN")
        .ok()
        .filter(|bin| !bin.is_empty());
    let (Some(agent_pid), Some(agent_bin)) = (agent_pid, agent_bin) else {
        return true;
    };
    walk_reaches_single_agent(
        std::os::unix::process::parent_id(),
        agent_pid,
        &agent_bin,
        crate::process::parent_and_argv0,
    )
}

fn walk_reaches_single_agent(
    start: u32,
    agent_pid: u32,
    agent_bin: &str,
    parent_and_argv0: impl Fn(u32) -> Option<(u32, String)>,
) -> bool {
    let mut pid = start;
    let mut agents = 0;
    for hop in 0..MAX_ANCESTORS {
        let Some((ppid, argv0)) = parent_and_argv0(pid) else {
            // No readable process table proves nothing, so keep the write.
            return hop == 0;
        };
        if std::path::Path::new(&argv0)
            .file_name()
            .and_then(|name| name.to_str())
            == Some(agent_bin)
        {
            agents += 1;
        }
        if pid == agent_pid {
            return agents <= 1;
        }
        if ppid == 0 || ppid == pid {
            return false;
        }
        pid = ppid;
    }
    false
}

fn run_inner<R: Read>(
    stdin: R,
    instance_id: &str,
    field: crate::agents::HookIdentityField,
) -> Result<()> {
    let mut buf = String::new();
    stdin.take(STDIN_BYTE_CAP).read_to_string(&mut buf)?;
    let value: serde_json::Value = serde_json::from_str(&buf)?;
    let sid = match field {
        crate::agents::HookIdentityField::SessionId => value
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("payload has no top-level string session_id"))?,
        crate::agents::HookIdentityField::ConversationIdOrSessionId => value
            .get("conversation_id")
            .and_then(|v| v.as_str())
            .or_else(|| value.get("session_id").and_then(|v| v.as_str()))
            .ok_or_else(|| {
                anyhow!("payload has no top-level string conversation_id or session_id")
            })?,
    };
    if !crate::session::capture::is_valid_session_id(sid) {
        return Err(anyhow!("payload contains an unsafe native session id"));
    }
    crate::hooks::write_session_id_via_guard(instance_id, sid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::BaseGuard;
    use std::os::unix::fs::PermissionsExt;

    fn extract(payload: &str, instance_id: &str) -> Result<()> {
        run_inner(
            payload.as_bytes(),
            instance_id,
            crate::agents::HookIdentityField::SessionId,
        )
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
    fn only_the_pane_agent_owns_the_hook() {
        // (pid, ppid, argv0) from the hook's parent shell upward.
        type Chain = &'static [(u32, u32, &'static str)];
        let cases: [(&str, u32, Chain, bool); 7] = [
            ("direct launch", 9, &[(10, 9, "sh"), (9, 1, "claude")], true),
            (
                "wrapper runs the agent as a child",
                8,
                &[(10, 9, "sh"), (9, 8, "/opt/bin/claude"), (8, 1, "/bin/sh")],
                true,
            ),
            (
                "nested agent from the shell tool",
                7,
                &[
                    (10, 9, "sh"),
                    (9, 8, "claude"),
                    (8, 7, "bash"),
                    (7, 1, "claude"),
                ],
                false,
            ),
            (
                "nested agent under a wrapper",
                6,
                &[
                    (10, 9, "sh"),
                    (9, 8, "claude"),
                    (8, 7, "bash"),
                    (7, 6, "claude"),
                    (6, 1, "sh"),
                ],
                false,
            ),
            (
                "detached from the launched pid",
                7,
                &[(10, 9, "sh"), (9, 1, "claude"), (1, 0, "init")],
                false,
            ),
            ("unreadable ancestor", 7, &[(10, 9, "sh")], false),
            ("process table unavailable", 7, &[], true),
        ];
        for (name, agent_pid, chain, owned) in cases {
            let table: std::collections::HashMap<u32, (u32, String)> = chain
                .iter()
                .map(|(pid, ppid, argv0)| (*pid, (*ppid, argv0.to_string())))
                .collect();
            let lookup = |pid| table.get(&pid).cloned();
            assert_eq!(
                walk_reaches_single_agent(10, agent_pid, "claude", lookup),
                owned,
                "{name}"
            );
        }
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
    fn conversation_identity_prefers_conversation_id_and_falls_back() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let conversation = "conversation_opaque.123";
        let session = "11111111-2222-3333-4444-555555555555";
        let field = crate::agents::HookIdentityField::ConversationIdOrSessionId;

        let payload = format!(r#"{{"conversation_id":"{conversation}","session_id":"{session}"}}"#);
        run_inner(payload.as_bytes(), "conversation_preferred", field).unwrap();
        assert_eq!(
            read_sidecar(&base, "conversation_preferred").as_deref(),
            Some(conversation)
        );

        let fallback = format!(r#"{{"session_id":"{session}"}}"#);
        run_inner(fallback.as_bytes(), "conversation_fallback", field).unwrap();
        assert_eq!(
            read_sidecar(&base, "conversation_fallback").as_deref(),
            Some(session)
        );
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
    fn accepts_safe_opaque_session_id() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let payload = r#"{"session_id":"conversation_opaque.123"}"#;
        extract(payload, "opaque_id").unwrap();
        assert_eq!(
            read_sidecar(&base, "opaque_id").as_deref(),
            Some("conversation_opaque.123")
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn rejects_unsafe_session_id() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let payload = r#"{"session_id":"unsafe id;rm"}"#;
        let err = extract(payload, "unsafe_id").unwrap_err();
        assert!(read_sidecar(&base, "unsafe_id").is_none(), "got: {err}");
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
        let result = run_inner(
            InfiniteReader,
            "infinite",
            crate::agents::HookIdentityField::SessionId,
        );
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
