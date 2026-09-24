//! Hidden `aoe __extract-session-id` subcommand.

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
    let source = match std::env::var(crate::hooks::SESSION_SOURCE_ENV) {
        Ok(source) => Some(source),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => return Ok(()),
    };
    if let Err(e) = run_inner(
        std::io::stdin().lock(),
        &instance_id,
        args.field,
        source.as_deref(),
    ) {
        tracing::debug!(target: "hooks.session_id", "extract failed: {e}");
    }
    Ok(())
}

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
    source: Option<&str>,
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
    crate::hooks::write_session_id_via_guard(instance_id, sid, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::test_support::BaseGuard;
    use std::os::unix::fs::PermissionsExt;

    const UUID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    const OTHER: &str = "11111111-2222-3333-4444-555555555555";

    fn extract(payload: &str, instance_id: &str) -> Result<()> {
        run_inner(
            payload.as_bytes(),
            instance_id,
            crate::agents::HookIdentityField::SessionId,
            None,
        )
    }

    fn read_sidecar(base: &std::path::Path, instance_id: &str) -> Option<String> {
        std::fs::read_to_string(base.join(instance_id).join("session_id")).ok()
    }

    #[test]
    fn only_the_pane_agent_owns_the_hook() {
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
    fn accepted_payloads_write_the_top_level_session_id() {
        let (_g, base, _tmp) = BaseGuard::ready();
        const UPPER: &str = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE";
        const OPAQUE: &str = "conversation_opaque.123";
        let cases: [(&str, String, &str); 6] = [
            (
                "compact",
                format!(r#"{{"session_id":"{UUID}","cwd":"/x"}}"#),
                UUID,
            ),
            (
                "multi_line",
                format!("{{\n  \"session_id\":\"{UUID}\",\n  \"cwd\":\"/x\"\n}}"),
                UUID,
            ),
            ("uppercase", format!(r#"{{"session_id":"{UPPER}"}}"#), UPPER),
            ("opaque", format!(r#"{{"session_id":"{OPAQUE}"}}"#), OPAQUE),
            (
                "top_level_beats_nested",
                format!(r#"{{"context":{{"session_id":"{OTHER}"}},"session_id":"{UUID}"}}"#),
                UUID,
            ),
            (
                "prompt_injection",
                format!(r#"{{"session_id":"{UUID}","prompt":"\"session_id\":\"{OTHER}\""}}"#),
                UUID,
            ),
        ];
        for (name, payload, expected) in cases {
            extract(&payload, name).unwrap();
            assert_eq!(
                read_sidecar(&base, name).as_deref(),
                Some(expected),
                "{name}"
            );
        }
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn rejected_payloads_write_no_sidecar() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let oversized = "x".repeat(STDIN_BYTE_CAP as usize * 2);
        let cases: [(&str, &str, &str); 6] = [
            ("no_sid", r#"{"cwd":"/x","other":"value"}"#, "session_id"),
            ("non_string", r#"{"session_id":12345}"#, "session_id"),
            ("unsafe_id", r#"{"session_id":"unsafe id;rm"}"#, "unsafe"),
            ("malformed", "not json {{{", ""),
            ("empty", "", ""),
            ("oversized", oversized.as_str(), ""),
        ];
        for (name, payload, needle) in cases {
            let err = extract(payload, name).unwrap_err().to_string();
            assert!(err.contains(needle), "{name}: {err}");
            assert!(read_sidecar(&base, name).is_none(), "{name}: {err}");
        }
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn conversation_identity_prefers_conversation_id_and_falls_back() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let conversation = "conversation_opaque.123";
        let field = crate::agents::HookIdentityField::ConversationIdOrSessionId;

        let payload = format!(r#"{{"conversation_id":"{conversation}","session_id":"{OTHER}"}}"#);
        run_inner(payload.as_bytes(), "conversation_preferred", field, None).unwrap();
        assert_eq!(
            read_sidecar(&base, "conversation_preferred").as_deref(),
            Some(conversation)
        );

        let fallback = format!(r#"{{"session_id":"{OTHER}"}}"#);
        run_inner(fallback.as_bytes(), "conversation_fallback", field, None).unwrap();
        assert_eq!(
            read_sidecar(&base, "conversation_fallback").as_deref(),
            Some(OTHER)
        );
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
            None,
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
        let payload = format!(r#"{{"session_id":"{UUID}"}}"#);
        let _ = extract(&payload, inst);
        assert_eq!(
            std::fs::read_to_string(&decoy).unwrap(),
            "do not overwrite",
            "decoy bytes must be intact"
        );
    }

    fn read_suffixed_sidecar(
        base: &std::path::Path,
        instance_id: &str,
        source: &str,
    ) -> Option<String> {
        std::fs::read_to_string(base.join(instance_id).join(format!("session_id.{source}"))).ok()
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn canonical_source_routes_to_suffixed_leaf() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let source = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let payload = r#"{"session_id":"11111111-2222-3333-4444-555555555555"}"#;
        run_inner(
            payload.as_bytes(),
            "sourced",
            crate::agents::HookIdentityField::SessionId,
            Some(source),
        )
        .unwrap();
        assert_eq!(
            read_suffixed_sidecar(&base, "sourced", source).as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert!(
            read_sidecar(&base, "sourced").is_none(),
            "a sourced write must not touch the default leaf"
        );
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn non_canonical_source_refuses_the_write() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let payload = r#"{"session_id":"11111111-2222-3333-4444-555555555555"}"#;
        let err = run_inner(
            payload.as_bytes(),
            "bad_source",
            crate::agents::HookIdentityField::SessionId,
            Some("not-a-uuid"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("UUID"), "got: {err}");
        assert!(read_sidecar(&base, "bad_source").is_none());
    }
}
