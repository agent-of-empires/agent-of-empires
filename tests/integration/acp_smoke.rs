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

use crate::acp::acp_client::SpawnConfig;
use crate::acp::agent_registry::AgentSpec;
use crate::acp::approvals::{ApprovalDecision, ApprovalOption};
use crate::acp::state::Event;

use super::managed::{shim_path, shim_ready};
/// Permission round-trip: shim asks for permission, structured view resolves
/// allow, agent observes the selected option_id and reports back.
#[tokio::test]
#[serial_test::serial]
async fn shim_agent_round_trips_approval_allow() {
    super::managed::with_managed(|scope| {
        Box::pin(async move {
            if let Err(reason) = shim_ready() {
                eprintln!("skipping: {reason}");
                return;
            }
            let shim = shim_path();

            let cwd = std::env::temp_dir();
            let config = SpawnConfig {
                launch_lease: None,
                provider_routing: Vec::new(),
                wrapper_substitution: None,
                agent_key: "claude".into(),
                tool: "claude".into(),
                spec: AgentSpec {
                    command: super::managed::shim_node()
                        .expect("shim prerequisite")
                        .to_string_lossy()
                        .into_owned(),
                    args: vec![shim.to_string_lossy().to_string()],
                    description: "test shim".into(),
                    env_allowlist: None,
                },
                cwd,
                additional_dirs: vec![],
                provider_env: vec![],
                host_environment: vec![],
                default_effort: None,
                default_effort_explicit: false,
                default_mode: None,
                default_model: None,
                socket_path: None,
                stored_acp_session_id: None,
                fork_from: None,
                seed_history_replay: false,
                prior_conversation: false,
                generation: 0,
                artifact_dir: None,
                sandbox_info: None,
                source_profile: None,
                mcp_servers: Vec::new(),
                claude_store_pin: None,
                base_host_environment: vec![],
            };

            let mut client = scope
                .spawn(config, "approve")
                .await
                .expect("spawn shim agent");
            client.ready().await;

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
                                .resolve_permission(nonce, ApprovalDecision::Allow, None)
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
                Event::AgentMessageChunk { text, .. } => text.contains("permission_outcome=yes"),
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
        })
    })
    .await;
}

/// #3741: an agent can put a question in the option list (pi's
/// `ask_user_question` sends N `allow_once` options). The approval must
/// carry those labels, be flagged as a choice, and resolve with the
/// option the user actually picked instead of the first allow-once.
#[tokio::test]
#[serial_test::serial]
async fn shim_agent_round_trips_a_question_option_list() {
    super::managed::with_managed(|scope| {
        Box::pin(async move {
            if let Err(reason) = shim_ready() {
                eprintln!("skipping: {reason}");
                return;
            }
            let shim = shim_path();

            let config = SpawnConfig {
                launch_lease: None,
                provider_routing: Vec::new(),
                generation: 0,
                wrapper_substitution: None,
                agent_key: "claude".into(),
                tool: "claude".into(),
                spec: AgentSpec {
                    command: super::managed::shim_node()
                        .expect("shim prerequisite")
                        .to_string_lossy()
                        .into_owned(),
                    args: vec![shim.to_string_lossy().to_string()],
                    description: "test shim".into(),
                    env_allowlist: None,
                },
                cwd: std::env::temp_dir(),
                additional_dirs: vec![],
                provider_env: vec![],
                host_environment: vec![],
                default_effort: None,
                default_effort_explicit: false,
                default_mode: None,
                default_model: None,
                socket_path: None,
                stored_acp_session_id: None,
                fork_from: None,
                seed_history_replay: false,
                prior_conversation: false,
                artifact_dir: None,
                sandbox_info: None,
                source_profile: None,
                mcp_servers: Vec::new(),
                claude_store_pin: None,
                base_host_environment: vec![],
            };

            let mut client = scope
                .spawn(config, "choice")
                .await
                .expect("spawn shim agent");
            client.ready().await;

            client
                .send_prompt("REQUEST_CHOICE please", &[])
                .await
                .expect("send_prompt");

            let mut events: Vec<Event> = Vec::new();
            let mut offered: Vec<ApprovalOption> = Vec::new();
            let mut was_choice = false;
            let drain_deadline = std::time::Instant::now() + Duration::from_secs(15);
            while std::time::Instant::now() < drain_deadline {
                match tokio::time::timeout(Duration::from_millis(500), client.next_event()).await {
                    Ok(Some(event)) => {
                        if let Event::ApprovalRequested { approval } = &event {
                            offered = approval.options.clone();
                            was_choice = approval.choice;
                            client
                                .resolve_permission(
                                    approval.nonce.clone(),
                                    ApprovalDecision::Allow,
                                    Some("choice-2".into()),
                                )
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

            assert!(was_choice, "option list should classify as a question");
            assert_eq!(
                offered
                    .iter()
                    .map(|o| (o.option_id.as_str(), o.name.as_str()))
                    .collect::<Vec<_>>(),
                vec![
                    ("choice-0", "Option Alpha"),
                    ("choice-1", "Option Bravo"),
                    ("choice-2", "Option Charlie"),
                    ("choice-3", "Option Delta"),
                ],
                "the agent's own labels must reach the approval card"
            );
            assert!(
                events.iter().any(|e| match e {
                    Event::AgentMessageChunk { text, .. } =>
                        text.contains("choice_outcome=choice-2"),
                    _ => false,
                }),
                "agent should have received the picked option, not the first; got {events:?}"
            );
        })
    })
    .await;
}

/// #3741: dismissing an answer list must reach the agent as a
/// cancellation. Resolving it as a deny would be mapped back onto an
/// option by kind, so a list whose answers are all reject-kind would
/// send the first one as the user's answer.
#[tokio::test]
#[serial_test::serial]
async fn shim_agent_sees_a_dismissed_question_as_cancelled() {
    super::managed::with_managed(|scope| {
        Box::pin(async move {
            if let Err(reason) = shim_ready() {
                eprintln!("skipping: {reason}");
                return;
            }
            let shim = shim_path();

            let config = SpawnConfig {
                launch_lease: None,
                provider_routing: Vec::new(),
                generation: 0,
                wrapper_substitution: None,
                agent_key: "claude".into(),
                tool: "claude".into(),
                spec: AgentSpec {
                    command: super::managed::shim_node()
                        .expect("shim prerequisite")
                        .to_string_lossy()
                        .into_owned(),
                    args: vec![shim.to_string_lossy().to_string()],
                    description: "test shim".into(),
                    env_allowlist: None,
                },
                cwd: std::env::temp_dir(),
                additional_dirs: vec![],
                provider_env: vec![],
                host_environment: vec![],
                default_effort: None,
                default_effort_explicit: false,
                default_mode: None,
                default_model: None,
                socket_path: None,
                stored_acp_session_id: None,
                fork_from: None,
                seed_history_replay: false,
                prior_conversation: false,
                artifact_dir: None,
                sandbox_info: None,
                source_profile: None,
                mcp_servers: Vec::new(),
                claude_store_pin: None,
                base_host_environment: vec![],
            };

            let mut client = scope
                .spawn(config, "dismiss")
                .await
                .expect("spawn shim agent");
            client.ready().await;

            client
                .send_prompt("REQUEST_CHOICE please", &[])
                .await
                .expect("send_prompt");

            let mut events: Vec<Event> = Vec::new();
            let drain_deadline = std::time::Instant::now() + Duration::from_secs(15);
            while std::time::Instant::now() < drain_deadline {
                match tokio::time::timeout(Duration::from_millis(500), client.next_event()).await {
                    Ok(Some(event)) => {
                        if let Event::ApprovalRequested { approval } = &event {
                            client
                                .cancel_permission(approval.nonce.clone())
                                .await
                                .expect("cancel_permission");
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

            assert!(
                events.iter().any(|e| match e {
                    Event::AgentMessageChunk { text, .. } =>
                        text.contains("choice_outcome=cancelled"),
                    _ => false,
                }),
                "a dismissal must not answer with an option; got {events:?}"
            );
        })
    })
    .await;
}

/// fs round-trip: shim issues writeTextFile + readTextFile against a
/// temp dir; aoe handles them via fs_handler with sandbox enforcement;
/// shim echoes the read content back so we can assert the wire works.
#[tokio::test]
#[serial_test::serial]
async fn shim_agent_round_trips_fs() {
    super::managed::with_managed(|scope| {
        Box::pin(async move {
            if let Err(reason) = shim_ready() {
                eprintln!("skipping: {reason}");
                return;
            }
            let shim = shim_path();
            let temp = scope.tempdir();
            let cwd = temp.path().to_path_buf();
            let config = SpawnConfig {
                launch_lease: None,
                provider_routing: Vec::new(),
                wrapper_substitution: None,
                agent_key: "claude".into(),
                tool: "claude".into(),
                spec: AgentSpec {
                    command: super::managed::shim_node()
                        .expect("shim prerequisite")
                        .to_string_lossy()
                        .into_owned(),
                    args: vec![shim.to_string_lossy().to_string()],
                    description: "test shim".into(),
                    env_allowlist: None,
                },
                cwd: cwd.clone(),
                additional_dirs: vec![],
                provider_env: vec![],
                host_environment: vec![],
                default_effort: None,
                default_effort_explicit: false,
                default_mode: None,
                default_model: None,
                socket_path: None,
                stored_acp_session_id: None,
                fork_from: None,
                seed_history_replay: false,
                prior_conversation: false,
                generation: 0,
                artifact_dir: None,
                sandbox_info: None,
                source_profile: None,
                mcp_servers: Vec::new(),
                claude_store_pin: None,
                base_host_environment: vec![],
            };

            let mut client = scope.spawn(config, "fs").await.expect("spawn shim");
            client.ready().await;

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
                Event::AgentMessageChunk { text, .. } => {
                    text.starts_with("fs_read=hello from shim")
                }
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
        })
    })
    .await;
}

/// terminal round-trip: shim creates a terminal that runs `echo`, waits
/// for exit, fetches output, and reports back. Validates the fs_policy
/// + TerminalManager wiring end-to-end.
#[tokio::test]
#[serial_test::serial]
async fn shim_agent_round_trips_terminal() {
    super::managed::with_managed(|scope| {
        Box::pin(async move {
            if let Err(reason) = shim_ready() {
                eprintln!("skipping: {reason}");
                return;
            }
            let shim = shim_path();
            let temp = scope.tempdir();
            let cwd = temp.path().to_path_buf();
            let config = SpawnConfig {
                launch_lease: None,
                provider_routing: Vec::new(),
                wrapper_substitution: None,
                agent_key: "claude".into(),
                tool: "claude".into(),
                spec: AgentSpec {
                    command: super::managed::shim_node()
                        .expect("shim prerequisite")
                        .to_string_lossy()
                        .into_owned(),
                    args: vec![shim.to_string_lossy().to_string()],
                    description: "test shim".into(),
                    env_allowlist: None,
                },
                cwd: cwd.clone(),
                additional_dirs: vec![],
                provider_env: vec![],
                host_environment: vec![],
                default_effort: None,
                default_effort_explicit: false,
                default_mode: None,
                default_model: None,
                socket_path: None,
                stored_acp_session_id: None,
                fork_from: None,
                seed_history_replay: false,
                prior_conversation: false,
                generation: 0,
                artifact_dir: None,
                sandbox_info: None,
                source_profile: None,
                mcp_servers: Vec::new(),
                claude_store_pin: None,
                base_host_environment: vec![],
            };

            let mut client = scope.spawn(config, "term").await.expect("spawn shim");
            client.ready().await;

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
                Event::AgentMessageChunk { text, .. } => {
                    text.contains("terminal_output=terminal-roundtrip-ok")
                        && text.contains("exit=0")
                }
                _ => false,
            });
            assert!(
                saw_terminal,
                "shim should echo terminal_output=...;exit=0; got {events:?}"
            );
        })
    })
    .await;
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
#[serial_test::serial]
async fn shim_agent_set_mode_emits_current_mode_changed() {
    super::managed::with_managed(|scope| {
        Box::pin(async move {
            if let Err(reason) = shim_ready() {
                eprintln!("skipping: {reason}");
                return;
            }
            let shim = shim_path();

            let cwd = std::env::temp_dir();
            let config = SpawnConfig {
                launch_lease: None,
                provider_routing: Vec::new(),
                wrapper_substitution: None,
                agent_key: "claude".into(),
                tool: "claude".into(),
                spec: AgentSpec {
                    command: super::managed::shim_node()
                        .expect("shim prerequisite")
                        .to_string_lossy()
                        .into_owned(),
                    args: vec![shim.to_string_lossy().to_string()],
                    description: "test shim".into(),
                    env_allowlist: None,
                },
                cwd,
                additional_dirs: vec![],
                provider_env: vec![],
                host_environment: vec![],
                default_effort: None,
                default_effort_explicit: false,
                default_mode: None,
                default_model: None,
                socket_path: None,
                stored_acp_session_id: None,
                fork_from: None,
                seed_history_replay: false,
                prior_conversation: false,
                generation: 0,
                artifact_dir: None,
                sandbox_info: None,
                source_profile: None,
                mcp_servers: Vec::new(),
                claude_store_pin: None,
                base_host_environment: vec![],
            };

            let mut client = scope
                .spawn(config, "set-mode")
                .await
                .expect("spawn shim agent");
            client.ready().await;

            client
                .set_mode("bypassPermissions", None)
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
        })
    })
    .await;
}

/// Rate-limit classifier round-trip: shim returns a JSON-RPC error
/// with `data.errorKind = "rate_limit"` on prompt; acp_client must
/// emit RateLimit + Stopped{rate_limited} instead of letting the
/// connection task die with an AgentStartupError. See #1281.
#[tokio::test]
#[serial_test::serial]
async fn shim_agent_emits_rate_limit_event() {
    super::managed::with_managed(|scope| {
        Box::pin(async move {
            use crate::acp::state::Event;
            if let Err(reason) = shim_ready() {
                eprintln!("skipping: {reason}");
                return;
            }
            let shim = shim_path();

            let cwd = std::env::temp_dir();
            let config = SpawnConfig {
                launch_lease: None,
                provider_routing: Vec::new(),
                wrapper_substitution: None,
                agent_key: "claude".into(),
                tool: "claude".into(),
                spec: AgentSpec {
                    command: super::managed::shim_node()
                        .expect("shim prerequisite")
                        .to_string_lossy()
                        .into_owned(),
                    args: vec![shim.to_string_lossy().to_string()],
                    description: "test shim".into(),
                    env_allowlist: None,
                },
                cwd,
                additional_dirs: vec![],
                provider_env: vec![],
                host_environment: vec![],
                default_effort: None,
                default_effort_explicit: false,
                default_mode: None,
                default_model: None,
                socket_path: None,
                stored_acp_session_id: None,
                fork_from: None,
                seed_history_replay: false,
                prior_conversation: false,
                generation: 0,
                artifact_dir: None,
                sandbox_info: None,
                source_profile: None,
                mcp_servers: Vec::new(),
                claude_store_pin: None,
                base_host_environment: vec![],
            };

            let mut client = scope.spawn(config, "rl").await.expect("spawn shim agent");
            client.ready().await;

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
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn shim_usage_updates_preserve_cost_before_real_prompt_completion() {
    super::managed::with_managed(|scope| {
        Box::pin(async move {
            use crate::acp::control_protocol::{ControlBody, PromptOutcome};

            let directory = scope.tempdir();
            let test_name = std::thread::current().name().expect("managed libtest name").to_owned();
            let inspection = scope.inspect_next_runner(&test_name);
            let config = SpawnConfig {
                launch_lease: None,
                provider_routing: Vec::new(),
                wrapper_substitution: None,
                agent_key: "claude".into(),
                tool: "claude".into(),
                spec: AgentSpec {
                    command: super::managed::shim_node().expect("shim prerequisite")
                        .to_string_lossy().into_owned(),
                    args: vec![shim_path().to_string_lossy().into_owned()],
                    description: "test shim".into(),
                    env_allowlist: None,
                },
                cwd: directory.path().to_path_buf(),
                additional_dirs: vec![],
                provider_env: vec![],
                host_environment: vec![],
                default_effort: None,
                default_effort_explicit: false,
                default_mode: None,
                default_model: None,
                socket_path: None,
                stored_acp_session_id: None,
                fork_from: None,
                seed_history_replay: false,
                prior_conversation: false,
                generation: 0,
                artifact_dir: None,
                sandbox_info: None,
                source_profile: None,
                mcp_servers: Vec::new(),
                claude_store_pin: None,
                base_host_environment: vec![],
            };
            let mut client = scope.spawn(config, "usage-schema").await
                .expect("managed original Node session");
            client.ready().await;
            inspection.query("observe-producer").await;

            for (prompt, expected_cost) in [
                ("USAGE_BEFORE_COST", Some((0.01, "USD"))),
                ("USAGE_BEFORE_NO_COST", None),
            ] {
                client.send_prompt(prompt, &[]).await.expect("actual prompt");
                let events = tokio::time::timeout(Duration::from_secs(15), async {
                    let mut events = Vec::new();
                    loop {
                        let event = client.next_event().await
                            .expect("managed stream remains open until original completion");
                        let stopped = matches!(event, Event::Stopped { .. });
                        events.push(event);
                        if stopped { return events; }
                    }
                }).await.expect("actual Node prompt response reaches daemon");

                assert!(events.iter().any(|event| matches!(event,
                    Event::AgentMessageChunk { text, .. } if text == &format!("received: {prompt}")
                )), "Node must receive the actual prompt: {events:?}");
                let updates: Vec<_> = events.iter().filter_map(|event| match event {
                    Event::UsageUpdated { usage } => Some(usage),
                    _ => None,
                }).collect();
                assert_eq!(updates.len(), 1, "one actual accounting update: {events:?}");
                assert_eq!(updates[0].used, 120);
                assert_eq!(updates[0].size, 200_000);
                assert_eq!(updates[0].cost.as_ref().map(|cost| (cost.amount, cost.currency.as_str())),
                    expected_cost, "cost must follow the current wire update, including removal");
                assert!(matches!(events.last(),
                    Some(Event::Stopped { reason }) if reason == "prompt_complete"
                ), "actual end_turn must settle its prompt: {events:?}");
            }

            let snapshot = inspection.query("producer-snapshot").await;
            let producer_events = snapshot["events"].as_array().unwrap();
            let written: Vec<_> = producer_events.iter().filter(|event| {
                event["stage"] == "written" && event["method"] == "session/prompt"
            }).map(|event| event["id"].as_i64().expect("original native prompt ID")).collect();
            assert_eq!(written.len(), 2, "both prompts reach actual native stdin");
            let completed: Vec<_> = producer_events.iter()
                .filter(|event| event["stage"] == "queued")
                .filter_map(|event| {
                    match serde_json::from_value::<ControlBody>(event["body"].clone())
                        .expect("actual producer control frame") {
                        ControlBody::PromptCompleted { prompt_req_id, outcome, .. } => {
                            assert!(matches!(outcome,
                                PromptOutcome::Completed { stop_reason: Some(reason) } if reason == "end_turn"
                            ), "only actual Node end_turn may settle these prompts");
                            Some(prompt_req_id)
                        }
                        _ => None,
                    }
                }).collect();
            assert_eq!(completed, written, "each original write receives its real result");
            client.shutdown().await.expect("retire managed original owner");
        })
    }).await;
}
