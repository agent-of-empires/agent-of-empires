//! End-to-end smoke test: Rust ACP client spawns a Node ACP shim agent,
//! sends a prompt, and observes structured events come back.
//!
//! Validates the structured view's plumbing without any API keys: the shim agent
//! at `acp-worker/test-shim/shim.mjs` replays a scripted sequence of
//! `session/update` notifications.
//!
//! Skipped automatically if `node` is not on PATH (structured view feature
//! requires Node anyway, so on a real structured view-enabled build environment
//! this test runs).

use std::time::Duration;

use agent_of_empires::acp::acp_client::{AcpClient, SpawnConfig};
use agent_of_empires::acp::agent_registry::AgentSpec;
use agent_of_empires::acp::approvals::ApprovalDecision;
use agent_of_empires::acp::state::{AcpSessionId, Event};

use crate::common::{shim_path, shim_ready};

#[tokio::test]
async fn shim_agent_round_trips_prompt() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let shim = shim_path();

    let cwd = std::env::temp_dir();
    let config = SpawnConfig {
        agent_key: "claude".into(),
        spec: AgentSpec {
            command: "node".into(),
            args: vec![shim.to_string_lossy().to_string()],
            description: "test shim".into(),
            env_allowlist: None,
        },
        cwd,
        additional_dirs: vec![],
        provider_env: vec![],
        default_effort: None,
        socket_path: None,
        stored_acp_session_id: None,
        seed_history_replay: false,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
    };

    let mut client = AcpClient::spawn(config, AcpSessionId("smoke".into()))
        .await
        .expect("spawn shim agent");

    client
        .send_prompt("hello smoke", &[])
        .await
        .expect("send_prompt");

    // Drain events with a generous timeout. The shim emits 4 session/update
    // notifications + we expect a Stopped event after the prompt completes.
    let mut events: Vec<Event> = Vec::new();
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(500), client.next_event()).await {
            Ok(Some(event)) => {
                let stopped = matches!(event, Event::Stopped { .. });
                events.push(event);
                if stopped {
                    break;
                }
            }
            Ok(None) | Err(_) => continue,
        }
    }

    // The shim emits 4 ACP session/update notifications. With the typed
    // mapping in place these now arrive as: 2x AgentMessageChunk, 1x
    // ToolCallStarted, 1x ToolCallCompleted. Plus our Stopped marker
    // when the prompt round-trip completes.
    let agent_msg_count = events
        .iter()
        .filter(|e| matches!(e, Event::AgentMessageChunk { .. }))
        .count();
    let tool_started = events
        .iter()
        .filter(|e| matches!(e, Event::ToolCallStarted { .. }))
        .count();
    let tool_completed = events
        .iter()
        .filter(|e| matches!(e, Event::ToolCallCompleted { .. }))
        .count();
    let stopped_count = events
        .iter()
        .filter(|e| matches!(e, Event::Stopped { .. }))
        .count();

    let _ = client.shutdown().await;

    eprintln!(
        "smoke: collected {} events (agent_msg={}, tool_started={}, tool_completed={}, stopped={})",
        events.len(),
        agent_msg_count,
        tool_started,
        tool_completed,
        stopped_count,
    );
    for (i, event) in events.iter().enumerate() {
        eprintln!("  [{i}] {:?}", event);
    }

    assert!(
        agent_msg_count >= 2,
        "expected >= 2 AgentMessageChunk events, got {agent_msg_count}"
    );
    assert!(
        tool_started >= 1,
        "expected >= 1 ToolCallStarted event, got {tool_started}"
    );
    assert!(
        tool_completed >= 1,
        "expected >= 1 ToolCallCompleted event, got {tool_completed}"
    );
    assert!(
        stopped_count >= 1,
        "expected at least 1 Stopped event, got {stopped_count}"
    );

    // Verify the tool call name carries through the typed mapping.
    let tool_call_titles: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolCallStarted { tool_call } => Some(tool_call.name.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        tool_call_titles.iter().any(|t| t.contains("shim file")),
        "tool call title should be preserved through the mapping; got {tool_call_titles:?}"
    );
}

/// Permission round-trip: shim asks for permission, structured view resolves
/// allow, agent observes the selected option_id and reports back.
#[tokio::test]
async fn shim_agent_round_trips_approval_allow() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let shim = shim_path();

    let cwd = std::env::temp_dir();
    let config = SpawnConfig {
        agent_key: "claude".into(),
        spec: AgentSpec {
            command: "node".into(),
            args: vec![shim.to_string_lossy().to_string()],
            description: "test shim".into(),
            env_allowlist: None,
        },
        cwd,
        additional_dirs: vec![],
        provider_env: vec![],
        default_effort: None,
        socket_path: None,
        stored_acp_session_id: None,
        seed_history_replay: false,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
    };

    let mut client = AcpClient::spawn(config, AcpSessionId("approve".into()))
        .await
        .expect("spawn shim agent");

    client
        .send_prompt("REQUEST_PERMISSION please", &[])
        .await
        .expect("send_prompt");

    // Auto-resolve the approval as soon as we observe the
    // ApprovalRequested event. Drain until Stopped.
    let mut events: Vec<Event> = Vec::new();
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(500), client.next_event()).await {
            Ok(Some(event)) => {
                if let Event::ApprovalRequested { approval } = &event {
                    let nonce = approval.nonce.clone();
                    let resolve_client = &client;
                    resolve_client
                        .resolve_permission(nonce, ApprovalDecision::Allow)
                        .await
                        .expect("resolve_permission");
                }
                let stopped = matches!(event, Event::Stopped { .. });
                events.push(event);
                if stopped {
                    break;
                }
            }
            Ok(None) | Err(_) => continue,
        }
    }

    let _ = client.shutdown().await;

    let saw_request = events
        .iter()
        .any(|e| matches!(e, Event::ApprovalRequested { .. }));
    let saw_resolved = events.iter().any(|e| {
        matches!(
            e,
            Event::ApprovalResolved {
                decision: ApprovalDecision::Allow,
                ..
            }
        )
    });
    let saw_yes_outcome = events.iter().any(|e| match e {
        Event::AgentMessageChunk { text } => text.contains("permission_outcome=yes"),
        _ => false,
    });

    assert!(
        saw_request,
        "expected ApprovalRequested in events; got {events:?}"
    );
    assert!(
        saw_resolved,
        "expected ApprovalResolved Allow in events; got {events:?}"
    );
    assert!(
        saw_yes_outcome,
        "shim should have echoed permission_outcome=yes; got {events:?}"
    );
}

/// fs round-trip: shim issues writeTextFile + readTextFile against a
/// temp dir; aoe handles them via fs_handler with sandbox enforcement;
/// shim echoes the read content back so we can assert the wire works.
#[tokio::test]
async fn shim_agent_round_trips_fs() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let shim = shim_path();
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().to_path_buf();
    let config = SpawnConfig {
        agent_key: "claude".into(),
        spec: AgentSpec {
            command: "node".into(),
            args: vec![shim.to_string_lossy().to_string()],
            description: "test shim".into(),
            env_allowlist: None,
        },
        cwd: cwd.clone(),
        additional_dirs: vec![],
        provider_env: vec![],
        default_effort: None,
        socket_path: None,
        stored_acp_session_id: None,
        seed_history_replay: false,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
    };

    let mut client = AcpClient::spawn(config, AcpSessionId("fs".into()))
        .await
        .expect("spawn shim");

    client
        .send_prompt("FS_READ_WRITE please", &[])
        .await
        .expect("send_prompt");

    let mut events: Vec<Event> = Vec::new();
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(500), client.next_event()).await {
            Ok(Some(event)) => {
                let stopped = matches!(event, Event::Stopped { .. });
                events.push(event);
                if stopped {
                    break;
                }
            }
            Ok(None) | Err(_) => continue,
        }
    }

    let _ = client.shutdown().await;

    let saw_read = events.iter().any(|e| match e {
        Event::AgentMessageChunk { text } => text.starts_with("fs_read=hello from shim"),
        _ => false,
    });
    assert!(
        saw_read,
        "shim should echo fs_read=hello from shim; got {events:?}"
    );
    // And the file should actually exist on disk.
    assert!(
        cwd.join("shim-roundtrip.txt").exists(),
        "shim-roundtrip.txt should exist in session cwd"
    );
}

/// terminal round-trip: shim creates a terminal that runs `echo`, waits
/// for exit, fetches output, and reports back. Validates the fs_policy
/// + TerminalManager wiring end-to-end.
#[tokio::test]
async fn shim_agent_round_trips_terminal() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let shim = shim_path();
    let temp = tempfile::tempdir().expect("tempdir");
    let cwd = temp.path().to_path_buf();
    let config = SpawnConfig {
        agent_key: "claude".into(),
        spec: AgentSpec {
            command: "node".into(),
            args: vec![shim.to_string_lossy().to_string()],
            description: "test shim".into(),
            env_allowlist: None,
        },
        cwd: cwd.clone(),
        additional_dirs: vec![],
        provider_env: vec![],
        default_effort: None,
        socket_path: None,
        stored_acp_session_id: None,
        seed_history_replay: false,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
    };

    let mut client = AcpClient::spawn(config, AcpSessionId("term".into()))
        .await
        .expect("spawn shim");

    client
        .send_prompt("TERMINAL_RUN please", &[])
        .await
        .expect("send_prompt");

    let mut events: Vec<Event> = Vec::new();
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(15);
    while std::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(500), client.next_event()).await {
            Ok(Some(event)) => {
                let stopped = matches!(event, Event::Stopped { .. });
                events.push(event);
                if stopped {
                    break;
                }
            }
            Ok(None) | Err(_) => continue,
        }
    }

    let _ = client.shutdown().await;

    let saw_terminal = events.iter().any(|e| match e {
        Event::AgentMessageChunk { text } => {
            text.contains("terminal_output=terminal-roundtrip-ok") && text.contains("exit=0")
        }
        _ => false,
    });
    assert!(
        saw_terminal,
        "shim should echo terminal_output=...;exit=0; got {events:?}"
    );
}

// NOTE: the previous "aoe-binds, agent-connects" socket-transport test
// has been removed. In the worker-persistence redesign (issue #1037),
// the runner now binds the socket and the daemon connects, which the
// test cannot exercise without a real `aoe __acp-runner` binary on
// PATH (the test process's `current_exe()` is the test runner, not
// `aoe`). The downstream socket transport (ACP handshake, prompt
// round-trip, event mapping after the runner has bound the socket)
// is covered in `tests/acp_midturn_resume.rs` via a byte-proxy
// bridge that mimics what `__acp-runner` does in production.
// End-to-end coverage of the spawn path itself still wants a real
// `aoe` binary; that test belongs in `tests/e2e/` and is tracked as
// follow-up work.

/// set_mode round-trip: structured view dispatches `session/set_mode` to the
/// shim, observes a synthetic `CurrentModeChanged` event with the
/// requested id. Covers the structured view-side wiring that the wizard's
/// `yolo_mode_default = true` (#1142) and the post-`session/new` bypass
/// in `Supervisor::spawn` depend on.
#[tokio::test]
async fn shim_agent_set_mode_emits_current_mode_changed() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let shim = shim_path();

    let cwd = std::env::temp_dir();
    let config = SpawnConfig {
        agent_key: "claude".into(),
        spec: AgentSpec {
            command: "node".into(),
            args: vec![shim.to_string_lossy().to_string()],
            description: "test shim".into(),
            env_allowlist: None,
        },
        cwd,
        additional_dirs: vec![],
        provider_env: vec![],
        default_effort: None,
        socket_path: None,
        stored_acp_session_id: None,
        seed_history_replay: false,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
    };

    let mut client = AcpClient::spawn(config, AcpSessionId("set-mode".into()))
        .await
        .expect("spawn shim agent");

    client
        .set_mode("bypassPermissions")
        .await
        .expect("set_mode dispatch through cmd_tx");

    // CurrentModeChanged is emitted by the structured view connection loop after
    // session/set_mode succeeds, not by the shim's response payload (the
    // shim's setSessionMode returns {}). We drain events with a short
    // deadline since no prompt is in flight; the only event we expect
    // is the synthesized one.
    let mut saw_change = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), client.next_event()).await {
            Ok(Some(Event::CurrentModeChanged { current_mode_id })) => {
                assert_eq!(current_mode_id, "bypassPermissions");
                saw_change = true;
                break;
            }
            Ok(Some(_)) => continue,
            Ok(None) | Err(_) => continue,
        }
    }

    let _ = client.shutdown().await;
    assert!(
        saw_change,
        "expected CurrentModeChanged(bypassPermissions) after set_mode"
    );
}

/// Rate-limit classifier round-trip: shim returns a JSON-RPC error
/// with `data.errorKind = "rate_limit"` on prompt; acp_client must
/// emit RateLimit + Stopped{rate_limited} instead of letting the
/// connection task die with an AgentStartupError. See #1281.
#[tokio::test]
async fn shim_agent_emits_rate_limit_event() {
    use agent_of_empires::acp::state::Event;
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let shim = shim_path();

    let cwd = std::env::temp_dir();
    let config = SpawnConfig {
        agent_key: "claude".into(),
        spec: AgentSpec {
            command: "node".into(),
            args: vec![shim.to_string_lossy().to_string()],
            description: "test shim".into(),
            env_allowlist: None,
        },
        cwd,
        additional_dirs: vec![],
        provider_env: vec![],
        default_effort: None,
        socket_path: None,
        stored_acp_session_id: None,
        seed_history_replay: false,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
    };

    let mut client = AcpClient::spawn(config, AcpSessionId("rl".into()))
        .await
        .expect("spawn shim agent");

    client
        .send_prompt("trigger RATE_LIMIT now", &[])
        .await
        .expect("send_prompt");

    let mut events: Vec<Event> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), client.next_event()).await {
            Ok(Some(event)) => {
                let stopped_rate_limited = matches!(
                    &event,
                    Event::Stopped { reason } if reason == "rate_limited"
                );
                events.push(event);
                if stopped_rate_limited {
                    break;
                }
            }
            Ok(None) | Err(_) => continue,
        }
    }

    let _ = client.shutdown().await;

    let rate_limit_count = events
        .iter()
        .filter(|e| matches!(e, Event::RateLimit { .. }))
        .count();
    let stopped_rate_limited_count = events
        .iter()
        .filter(|e| matches!(e, Event::Stopped { reason } if reason == "rate_limited"))
        .count();
    let startup_error_count = events
        .iter()
        .filter(|e| matches!(e, Event::AgentStartupError { .. }))
        .count();

    assert_eq!(
        rate_limit_count, 1,
        "expected exactly one RateLimit event, got {rate_limit_count} in {events:?}"
    );
    assert_eq!(
        stopped_rate_limited_count, 1,
        "expected exactly one Stopped{{rate_limited}}, got {stopped_rate_limited_count}"
    );
    assert_eq!(
        startup_error_count, 0,
        "rate-limit must NOT surface as AgentStartupError; got {startup_error_count}"
    );
}
