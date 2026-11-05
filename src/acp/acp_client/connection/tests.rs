//! Actual owned runner and SDK task, with original unit-test peers over the native bridge.

use super::*;
use crate::acp::acp_client::commands::ClientCmd;
use crate::acp::acp_client::{SdkTaskObservation, SdkTestOptions};
use crate::acp::approvals::Nonce;
use crate::acp::event_sender::WeakEventSender;
use crate::acp::supervisor::test_support::managed::{ManagedClient, ManagedScope, ManagedTask};
use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
use futures_util::FutureExt;
use std::future::Future;
use std::pin::Pin;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

type SharedWrite = Arc<Mutex<tokio::io::DuplexStream>>;

async fn write_line(w: &SharedWrite, line: &str) {
    let mut guard = w.lock().await;
    guard.write_all(line.as_bytes()).await.unwrap();
    guard.write_all(b"\n").await.unwrap();
    guard.flush().await.unwrap();
}
#[test]
fn admission_keeps_known_loss_after_foreground_completion_without_issuing_work() {
    for (stopping, finished, lost) in [
        (true, true, true),
        (false, true, true),
        (false, true, false),
        (false, false, false),
    ] {
        let state = ConnectionState::default();
        state.record_usable();
        if lost {
            state.observe(
                AcpConnectionSource::Control,
                AcpConnectionCause::IncomingEnded,
            );
        }
        let original_loss = state.loss();
        state.foreground_finished.store(finished, Ordering::Release);
        if stopping {
            state.mark_stopping();
        }
        let mut issued = false;
        let result = state.admit(|| issued = true);
        if stopping || (finished && !lost) {
            assert!(matches!(result, Err(AcpError::NotRunning)));
            assert!(!issued);
        } else if lost {
            assert!(
                matches!(result, Err(AcpError::ConnectionUnavailable(observation)) if Some(observation) == original_loss)
            );
            assert!(!issued);
        } else {
            assert!(result.is_ok());
            assert!(issued);
        }
        assert_eq!(
            state.loss(),
            original_loss,
            "foreground settlement cannot erase the original cause"
        );
    }
}

/// Cancel reaches each actual Native while its public event queue is full.
/// Output backpressure cannot require the consumer to drain before cancelling.
#[tokio::test]
async fn cancel_reaches_the_agent_while_notifications_remain_queued() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| {
        Box::pin(async move {
            let first = cancel_under_flood(scope).await;
            let second = cancel_under_flood(scope).await;
            tokio::join!(first, second);
        })
    })
    .await;
}

/// Fake agent: handshake, then flood updates while the turn runs; end the
/// turn only on `session/cancel`.
async fn fake_agent(
    agent_read: tokio::io::DuplexStream,
    agent_write: SharedWrite,
    started: oneshot::Sender<()>,
    cancelled: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(agent_read);
    let mut line = String::new();
    let mut prompt_id: Option<serde_json::Value> = None;
    let mut started = Some(started);
    loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            break;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let id = &msg["id"];
        let reply = match msg.get("method").and_then(|m| m.as_str()) {
            Some("initialize") => Some(format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1,"agentCapabilities":{{}}}}}}"#
            )),
            Some("session/new") => Some(format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"sessionId":"s-fair"}}}}"#
            )),
            Some("session/prompt") => {
                prompt_id = Some(id.clone());
                started.take().expect("one issued prompt").send(()).unwrap();
                None
            }
            Some("session/cancel") => {
                cancelled.store(true, Ordering::SeqCst);
                prompt_id.take().map(|id| {
                    format!(
                        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"stopReason":"cancelled"}}}}"#
                    )
                })
            }
            _ => None,
        };
        if let Some(reply) = reply {
            write_line(&agent_write, &reply).await;
        }
    }
}

async fn cancel_under_flood(scope: &mut ManagedScope) -> Pin<Box<dyn Future<Output = ()>>> {
    let (daemon_write, agent_read) = tokio::io::duplex(1024 * 1024);
    let (agent_write, daemon_read) = tokio::io::duplex(1024 * 1024);
    let agent_write: SharedWrite = Arc::new(Mutex::new(agent_write));
    let cancelled = Arc::new(AtomicBool::new(false));
    let (started, prompt_seen) = oneshot::channel();
    let (start_flood, flood_started) = oneshot::channel();

    let temp = scope.tempdir();
    let config = scope.bridge_config(temp.path().to_path_buf(), daemon_write, daemon_read);
    let agent = scope.spawn_helper(fake_agent(
        agent_read,
        agent_write.clone(),
        started,
        cancelled.clone(),
    ));
    // The flood runs beside the reader so the agent keeps reading cancel.
    let flood_stop = cancelled.clone();
    let flood = scope.spawn_helper(async move {
        flood_started.await.expect("actual prompt admission precedes output pressure");
        let update = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-fair","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"flood"}}}}"#;
        for _ in 0..128 {
            if flood_stop.load(Ordering::SeqCst) {
                break;
            }
            write_line(&agent_write, update).await;
        }
    });

    let mut event_rx = scope
        .spawn(config, "s-fair")
        .await
        .expect("real managed fair connection");
    event_rx.ready().await;
    let actual = event_rx.actual();
    assert_eq!(actual.selected_native_session(), "s-fair");
    let queue = actual.test_event_queue();
    let cmd_tx = actual.test_command_sender();
    let connection = actual.observe_sdk_task();
    Box::pin(async move {
        cmd_tx
            .send(ClientCmd::Prompt(vec![ContentBlock::Text(
                TextContent::new("hi"),
            )]))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(30), prompt_seen)
            .await
            .expect("the actual Native receives its admitted prompt")
            .unwrap();
        let drain_pause = event_rx.pause_drain().await;
        start_flood.send(()).unwrap();

        tokio::time::timeout(Duration::from_secs(30), async {
            while queue.upgrade().expect("original SDK queue").capacity() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual Native output fills the paused public queue");
        cmd_tx.send(ClientCmd::Cancel).await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            while !cancelled.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Cancel reaches Native without releasing output backpressure");
        assert_eq!(
            queue.upgrade().expect("same original SDK queue").capacity(),
            0
        );
        drop(drain_pause);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let stopped = loop {
            let event = tokio::time::timeout_at(deadline, event_rx.recv())
                .await
                .expect("cancel must be processed under sustained notifications")
                .expect("event channel open");
            if let Event::Stopped { .. } = event {
                break event;
            }
        };
        assert!(
            cancelled.load(Ordering::SeqCst),
            "session/cancel must reach the agent while notifications are queued: {stopped:?}"
        );
        let _ = flood.await;
        event_rx
            .shutdown()
            .await
            .expect("settle actual fairness runner");
        agent.abort();
        let _ = tokio::join!(agent, connection);
    })
}

/// A new adapter process clears the previous process's identity before it can
/// report its own; a reattach to the surviving process keeps it.
#[tokio::test]
async fn auth_status_is_cleared_per_adapter_process() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| {
        Box::pin(async move {
            let fresh = || ConnectMode::Fresh;
            let reattach = ConnectMode::Resume {
                acp_session_id: "s-auth".into(),
                in_flight_turn: false,
                unsuccessful_main: false,
            };
            let cases = [
                ("silent replacement", fresh(), false, vec![false]),
                (
                    "replacement that reports early",
                    fresh(),
                    true,
                    vec![false, true],
                ),
                ("reattach to the surviving process", reattach, false, vec![]),
            ];
            for (name, mode, reports, want) in cases {
                assert_eq!(
                    auth_events(scope, mode, usize::from(reports)).await,
                    want,
                    "{name}"
                );
            }
        })
    })
    .await;
}

#[tokio::test]
async fn auth_bootstrap_exceeding_public_and_shim_capacity_reaches_ready_before_drain() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| {
        Box::pin(async move {
            for mode in [
                ConnectMode::Fresh,
                ConnectMode::Resume {
                    acp_session_id: "s-auth".into(),
                    in_flight_turn: false,
                    unsuccessful_main: false,
                },
            ] {
                let fresh = matches!(&mode, ConnectMode::Fresh);
                let reports = auth_events(scope, mode, 96).await;
                assert_eq!(reports.iter().filter(|present| **present).count(), 96);
                assert_eq!(
                    reports.iter().filter(|present| !**present).count(),
                    usize::from(fresh)
                );
            }
        })
    })
    .await;
}

/// `AuthStatusUpdated` events up to session commit, as "carries a report".
async fn auth_events(
    scope: &mut ManagedScope,
    mode: ConnectMode,
    report_count: usize,
) -> Vec<bool> {
    let (daemon_write, agent_read) = tokio::io::duplex(64 * 1024);
    let (agent_write, daemon_read) = tokio::io::duplex(64 * 1024);
    let agent_write: SharedWrite = Arc::new(Mutex::new(agent_write));
    let replay_write = agent_write.clone();
    let temp = scope.tempdir();
    let config = scope.bridge_config(temp.path().to_path_buf(), daemon_write, daemon_read);
    let resume = matches!(mode, ConnectMode::Resume { .. });
    // More than a shim's bytes and public queue's frames can precede initialize.
    let agent = scope.spawn_helper(async move {
        let mut lines = BufReader::new(agent_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id = &msg["id"];
            if msg["method"] == "initialize" && report_count != 0 && !resume {
                let report = serde_json::json!({
                    "jsonrpc": "2.0", "method": "_auth/status_update",
                    "params": {"authStatus": {
                        "kind": "account", "label": "x".repeat(1024),
                    }},
                })
                .to_string();
                for _ in 0..report_count {
                    write_line(&agent_write, &report).await;
                }
            }
            let out = match msg["method"].as_str() {
                Some("initialize") => vec![format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1,"agentCapabilities":{{"_meta":{{"authStatus":{{}}}}}}}}}}"#
                )],
                Some("session/new") => vec![format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"sessionId":"s-auth"}}}}"#
                )],
                _ => vec![],
            };
            for line in out {
                write_line(&agent_write, &line).await;
            }
        }
    });
    let original = scope
        .spawn(config, "s-auth")
        .await
        .expect("real managed auth original");
    original.ready().await;
    let mut event_rx = if resume {
        scope
            .adopt_with(original, false, || async move {
                let report = serde_json::json!({
                    "jsonrpc": "2.0", "method": "_auth/status_update",
                    "params": {"authStatus": {"kind": "account", "label": "x".repeat(1024)}},
                })
                .to_string();
                for _ in 0..report_count {
                    write_line(&replay_write, &report).await;
                }
            })
            .await
    } else {
        original
    };
    tokio::time::timeout(Duration::from_secs(10), event_rx.ready())
        .await
        .expect("initialize must not require a public event consumer");
    let connection = event_rx.actual().observe_sdk_task();

    let (mut seen, mut committed, mut reports_seen) = (Vec::new(), false, 0);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !committed || reports_seen < report_count {
            match event_rx.recv().await.expect("connection remains open") {
                Event::AuthStatusUpdated { status } => {
                    reports_seen += usize::from(status.is_some());
                    seen.push(status.is_some());
                }
                Event::AcpSessionAssigned { .. } => committed = true,
                _ => {}
            }
        }
    })
    .await
    .expect("session commits and any report arrives");
    event_rx
        .shutdown()
        .await
        .expect("settle actual auth runner");
    agent.abort();
    let _ = tokio::join!(agent, connection);
    seen
}

/// A connection driven against a fake agent that answers the handshake and, on
/// each `session/prompt`, writes the lines `on_prompt` returns for its request id.
struct FakeConnection {
    event_rx: ManagedClient,
    cmd_tx: crate::acp::acp_client::test_helpers::TestCommandSender,
    agent_write: SharedWrite,
    pending_responders: super::super::pending::PendingResponders,
    replies: mpsc::UnboundedReceiver<serde_json::Value>,
    event_queue: WeakEventSender,
    connection_state: Arc<ConnectionState>,
    connection: SdkTaskObservation,
    agent: ManagedTask,
    native_prompt_id: Arc<std::sync::atomic::AtomicI64>,
    _cwd: Arc<tempfile::TempDir>,
}

async fn spawn_fake_connection(
    scope: &mut ManagedScope,
    session_id: &'static str,
    mode: Option<ConnectMode>,
    close_after_prompt: bool,
    on_prompt: impl Fn(&serde_json::Value) -> Vec<String> + Send + 'static,
) -> FakeConnection {
    let (daemon_write, agent_read) = tokio::io::duplex(64 * 1024);
    let (agent_write, daemon_read) = tokio::io::duplex(64 * 1024);
    let agent_write: SharedWrite = Arc::new(Mutex::new(agent_write));
    scope.configure_sdk(SdkTestOptions {
        command_capacity: Some(1),
        ..Default::default()
    });
    let (reply_tx, replies_rx) = mpsc::unbounded_channel();
    let temp = scope.tempdir();
    let config = scope.bridge_config(temp.path().to_path_buf(), daemon_write, daemon_read);
    let reply_write = agent_write.clone();
    let native_prompt_id = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let observed_prompt_id = Arc::clone(&native_prompt_id);
    let agent = scope.spawn_helper(async move {
        let mut lines = BufReader::new(agent_read).lines();
        let mut new_sessions = 0;
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
            if msg.get("method").is_none() {
                let _ = reply_tx.send(msg);
                continue;
            }
            let id = &msg["id"];
            let replies = match msg["method"].as_str() {
                Some("initialize") => vec![format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1,"agentCapabilities":{{}}}}}}"#
                )],
                Some("session/new") => {
                    let native = if new_sessions == 0 {
                        session_id.to_string()
                    } else {
                        format!("{session_id}-reset-{new_sessions}")
                    };
                    new_sessions += 1;
                    vec![
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":native}})
                            .to_string(),
                    ]
                }
                Some("session/prompt") => {
                    observed_prompt_id.store(id.as_i64().expect("actual Native prompt ID"), Ordering::Release);
                    on_prompt(id)
                }
                Some("session/set_config_option") => on_prompt(id),
                _ => continue,
            };
            for reply in replies {
                write_line(&reply_write, &reply).await;
            }
            if close_after_prompt && msg["method"] == "session/prompt" {
                reply_write.lock().await.shutdown().await.unwrap();
                return;
            }
        }
    });
    let original = scope
        .spawn(config, session_id)
        .await
        .expect("genuine managed original");
    original.ready().await;
    let event_rx = match mode {
        Some(ConnectMode::Resume { in_flight_turn, .. }) => {
            scope.adopt(original, in_flight_turn).await
        }
        _ => original,
    };
    event_rx.ready().await;
    let actual = event_rx.actual();
    assert_eq!(
        actual.selected_native_session(),
        session_id,
        "only the actual Original result selects identity"
    );
    let cmd_tx = actual.test_command_sender();
    let pending_responders = actual.test_pending_responders();
    let connection_state = actual.test_connection_state();
    let connection = actual.observe_sdk_task();
    let event_queue = actual.test_event_queue();
    FakeConnection {
        event_rx,
        cmd_tx,
        agent_write,
        pending_responders,
        replies: replies_rx,
        event_queue,
        connection_state,
        connection,
        agent,
        native_prompt_id,
        _cwd: temp,
    }
}

#[tokio::test]
async fn correlated_config_result_survives_eof_while_public_queue_is_full() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| {
        Box::pin(async move {
            let mut fake = spawn_fake_connection(scope, "s-config-eof", None, false, |id| {
                vec![serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "result": { "configOptions": [] },
                })
                .to_string()]
            })
            .await;
            tokio::time::timeout(Duration::from_secs(10), async {
                while !matches!(
                    fake.event_rx.recv().await.expect("connection remains live"),
                    Event::AcpSessionAssigned { .. }
                ) {}
            })
            .await
            .expect("session commits");
            let drain_pause = fake.event_rx.pause_drain().await;
            let sender = fake.event_queue.upgrade().unwrap();
            while let Some(result) = sender
                .send(Event::AuthStatusUpdated { status: None })
                .now_or_never()
            {
                result.expect("actual SDK queue remains open");
            }
            assert_eq!(sender.capacity(), 0);
            fake.cmd_tx
                .send(ClientCmd::SetConfigOption {
                    config_id: "model".into(),
                    value: "selected".into(),
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while fake
                    .connection_state
                    .completed_outcomes
                    .load(Ordering::Acquire)
                    == 0
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("correlated config result reaches its blocked application scope");
            fake.agent_write.lock().await.shutdown().await.unwrap();
            drop(sender);
            drop(drain_pause);
            let events = tokio::time::timeout(Duration::from_secs(10), async {
                let mut events = Vec::new();
                while let Some(event) = fake.event_rx.recv().await {
                    let unavailable = matches!(event, Event::AcpConnectionUnavailable { .. });
                    events.push(event);
                    if unavailable {
                        break;
                    }
                }
                events
            })
            .await
            .expect("completed config result drains before EOF");
            let config = events
    .iter()
    .position(
        |event| matches!(event, Event::ConfigOptionsUpdated { options } if options.is_empty()),
    )
    .expect("completed native configuration outcome survives SDK closure");
            let unavailable = events
                .iter()
                .position(|event| matches!(event, Event::AcpConnectionUnavailable { .. }))
                .expect("real EOF remains visible");
            assert!(config < unavailable);
            assert!(!events
                .iter()
                .any(|event| matches!(event, Event::AgentStartupError { .. })));
            fake.event_rx
                .shutdown()
                .await
                .expect("retire exact EOF case owner");
            tokio::time::timeout(Duration::from_secs(10), fake.connection)
                .await
                .unwrap()
                .unwrap();
            fake.agent.abort();
        })
    })
    .await;
}

#[tokio::test]
async fn rate_limit_on_an_agent_initiated_turn_preserves_the_connection() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { let mut fake = spawn_fake_connection(scope, "s-limit", None, false, |id| {
    vec![
        serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}})
            .to_string(),
    ]
})
.await;

tokio::time::timeout(Duration::from_secs(10), async {
    while !matches!(
        fake.event_rx.recv().await.expect("connection remains open"),
        Event::AcpSessionAssigned { .. }
    ) {}
})
.await
.expect("session commits");

// Accounting for a turn nobody prompted, carrying a rejected window.
let resets_at = chrono::Utc::now().timestamp() + 3600;
write_line(
    &fake.agent_write,
    &format!(
        r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-limit","update":{{"sessionUpdate":"usage_update","used":10,"size":100,"cost":{{"amount":0.1,"currency":"USD"}},"_meta":{{"_claude/rateLimit":{{"status":"rejected","rateLimitType":"five_hour","resetsAt":{resets_at}}}}}}}}}}}"#
    ),
)
.await;

// A later physical-source chunk is the ingress barrier. Ambient accounting
// cannot own a main terminal or park a turn that was never issued.
write_line(
    &fake.agent_write,
    r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-limit","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"accounting barrier"}}}}"#,
).await;
let seen = tokio::time::timeout(Duration::from_secs(10), async {
    let mut seen = Vec::new();
    loop {
        let event = fake.event_rx.recv().await.expect("same Native remains open");
        let barrier = matches!(&event, Event::AgentMessageChunk { text, .. } if text == "accounting barrier");
        seen.push(event);
        if barrier {
            break seen;
        }
    }
}).await.expect("ambient accounting reaches its ordered barrier");
assert!(!seen.iter().any(|event| matches!(event,
    Event::RateLimit { .. } | Event::Stopped { .. } | Event::AcpSessionAssigned { .. }
)));
fake.cmd_tx.send(ClientCmd::Cancel).await.unwrap();
let cancelled = collect_terminal(&mut fake).await;
assert!(matches!(cancelled.last(), Some(Event::Stopped { reason }) if reason == "cancelled"));
fake.cmd_tx
    .send(ClientCmd::Prompt(Vec::new()))
    .await
    .unwrap();
let next = collect_terminal(&mut fake).await;
assert_native_prompt_succeeded(scope, &fake);
assert_eq!(
    next.iter()
        .filter(|event| matches!(event, Event::Stopped { .. }))
        .count(),
    1
);
assert!(!next
    .iter()
    .any(|event| matches!(event, Event::AcpSessionAssigned { .. })));
fake.event_rx.shutdown().await.expect("retire exact case owner");
tokio::time::timeout(Duration::from_secs(10), fake.connection)
    .await
    .unwrap()
    .unwrap();
fake.agent.abort();
let _ = fake.agent.await; })).await;
}

/// Only an owned Native RPC failure may park the main turn. Ambient rejected
/// windows cannot turn a successful or cancelled result into a quota failure.
#[tokio::test]
async fn native_rate_limit_error_parks_but_ambient_rejection_does_not() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { let now = chrono::Utc::now().timestamp();
for (stop_reason, resets_at, overage, parks) in [
    ("end_turn", now + 3600, "", true),
    ("cancelled", now + 3600, "", false),
    ("end_turn", now - 60, "", false),
    (
        "end_turn",
        now + 3600,
        r#","overageStatus":"rejected","isUsingOverage":false"#,
        true,
    ),
    (
        "end_turn",
        now + 3600,
        r#","overageStatus":"allowed","isUsingOverage":true"#,
        false,
    ),
] {
    let update = format!(
        r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-prompted","update":{{"sessionUpdate":"usage_update","used":10,"size":100,"_meta":{{"_claude/rateLimit":{{"status":"rejected","rateLimitType":"five_hour","resetsAt":{resets_at}{overage}}}}}}}}}}}"#
    );
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let native_calls = calls.clone();
    let mut fake = spawn_fake_connection(scope, "s-prompted", None, false, move |id| {
        if native_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![
                update.clone(),
                if parks {
                    serde_json::json!({
                        "jsonrpc":"2.0","id":id,"error":{
                            "code":-32603,"message":"limit reached",
                            "data":{"errorKind":"rate_limit"}
                        }
                    }).to_string()
                } else {
                    format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"stopReason":"{stop_reason}"}}}}"#)
                },
            ]
        } else {
            vec![
                serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}})
                    .to_string(),
            ]
        }
    })
    .await;
    fake.cmd_tx
        .send(ClientCmd::Prompt(vec![ContentBlock::Text(
            TextContent::new("hi"),
        )]))
        .await
        .unwrap();

    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = fake.event_rx.recv().await {
            let stopped = matches!(event, Event::Stopped { .. });
            seen.push(event);
            if stopped {
                break;
            }
        }
    })
    .await
    .expect("the turn must reach a terminal event");
    let limit = seen.iter().position(|e| {
        matches!(e, Event::RateLimit { info } if info.resets_at.map(|t| t.timestamp()) == Some(resets_at))
    });
    let stopped_reason = match seen.last() {
        Some(Event::Stopped { reason }) => reason.as_str(),
        other => panic!("expected a terminal Stopped: {other:?}"),
    };
    if parks {
        assert!(
            limit.is_some(),
            "{stop_reason}: RateLimit expected: {seen:?}"
        );
        assert_eq!(stopped_reason, "rate_limited", "{seen:?}");
    } else {
        assert!(
            limit.is_none(),
            "{stop_reason}{overage}: must not park: {seen:?}"
        );
        assert_ne!(stopped_reason, "rate_limited", "{seen:?}");
    }
    fake.cmd_tx.send(ClientCmd::Cancel).await.unwrap();
    fake.cmd_tx
        .send(ClientCmd::Prompt(Vec::new()))
        .await
        .unwrap();
    let next = collect_terminal(&mut fake).await;
    assert_native_prompt_succeeded(scope, &fake);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(!next.iter().any(|event| matches!(
        event,
        Event::RateLimit { .. } | Event::AcpSessionAssigned { .. }
    )));
    fake.event_rx.shutdown().await.expect("retire exact case owner");
    tokio::time::timeout(Duration::from_secs(10), fake.connection)
        .await
        .unwrap()
        .unwrap();
    fake.agent.abort();
    let _ = fake.agent.await;
} })).await;
}

async fn collect_terminal(fake: &mut FakeConnection) -> Vec<Event> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        while let Some(event) = fake.event_rx.recv().await {
            let terminal = matches!(event, Event::Stopped { .. });
            events.push(event);
            if terminal {
                return events;
            }
        }
        panic!("connection closed without a turn terminal: {events:?}");
    })
    .await
    .expect("turn reaches a terminal")
}

fn assert_native_prompt_succeeded(scope: &ManagedScope, fake: &FakeConnection) {
    let id = fake.native_prompt_id.load(Ordering::Acquire);
    assert!(id > 0, "the explicit prompt reached the actual Native peer");
    let source = fake
        .event_rx
        .lease
        .native_scope()
        .unwrap()
        .expect("owned Native scope");
    let origin = fake
        .event_rx
        .original_request_context()
        .and_then(|context| context.origin())
        .expect("genuine Original native provenance");
    assert_eq!(origin.context, source.context);
    assert_eq!(origin.owner, fake.event_rx.connected_runner());
    let stored = scope
        .state
        .acp_event_store
        .replay_from(&fake.event_rx.id, 0)
        .unwrap();
    let mut terminals = stored.iter().filter(|stored| {
        matches!(stored.event.as_ref(), Event::Stopped { .. })
            && stored.metadata.native_prompt_id == Some(id)
            && stored.metadata.native_origin == Some(origin)
            && !stored.metadata.host_action
    });
    let terminal = terminals
        .next()
        .expect("exact Native prompt has its own terminal");
    assert_eq!(
        terminal
            .metadata
            .operation
            .and_then(|operation| operation.outcome),
        Some(crate::acp::protocol::AcpOriginalRequestResult::Succeeded),
        "explicit follow-up completes successfully in the same Native"
    );
    assert!(
        terminals.next().is_none(),
        "same Native prompt completes once"
    );
}

#[tokio::test]
async fn request_errors_fail_the_turn_without_replacing_the_connection() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { use crate::session::{ErrorBlockingScope, ErrorEvidenceSource, RuntimeEvidence};
use std::sync::atomic::AtomicUsize;
for (message, kind, parks) in [
    ("provider connection failure", "provider_error", false),
    ("invalid credentials", "auth_error", false),
    ("tool failed", "tool_error", false),
    ("unclassified failure", "unknown", false),
    ("limit reached", "rate_limit", true),
] {
    let calls = Arc::new(AtomicUsize::new(0));
    let native_calls = calls.clone();
    let mut fake = spawn_fake_connection(scope, "s-operation-error", None, false, move |id| {
        if native_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            vec![
                r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-operation-error","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"partial work survives"}}}}"#.into(),
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32603, "message": message,
                        "data": {"errorKind": kind, "privateTokenMarker": "must-not-leak"}}
                }).to_string(),
            ]
        } else {
            vec![serde_json::json!({
                "jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}
            }).to_string()]
        }
    }).await;
    fake.cmd_tx
        .send(ClientCmd::Prompt(Vec::new()))
        .await
        .unwrap();
    let mut events = collect_terminal(&mut fake).await;
    assert!(events.iter().any(|event| matches!(event, Event::AgentMessageChunk { text, .. } if text == "partial work survives")));
    if parks {
        assert!(events
            .iter()
            .any(|event| matches!(event, Event::RateLimit { .. })));
        assert!(
            matches!(events.last(), Some(Event::Stopped { reason }) if reason == "rate_limited")
        );
    } else {
        let failures: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                Event::PromptRuntimeError {
                    message, evidence, ..
                } => Some((message, evidence)),
                _ => None,
            })
            .collect();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, message);
        let evidence = failures[0].1.expect("classified operation metadata");
        assert_eq!(evidence.source, ErrorEvidenceSource::LocalPromptResult);
        assert_eq!(evidence.runtime, RuntimeEvidence::Unknown);
        assert_eq!(evidence.origin, None);
        assert_eq!(evidence.retryable, None);
        assert_eq!(evidence.blocking_scope, Some(ErrorBlockingScope::Operation));
        assert_eq!(evidence.reported_code, Some(-32603));
        assert!(
            matches!(events.last(), Some(Event::Stopped { reason }) if reason == "prompt_failed")
        );
    }
    assert!(!fake.connection.is_finished());
    fake.cmd_tx.send(ClientCmd::Cancel).await.unwrap();
    fake.cmd_tx
        .send(ClientCmd::Prompt(Vec::new()))
        .await
        .unwrap();
    let next = collect_terminal(&mut fake).await;
    assert_native_prompt_succeeded(scope, &fake);
    events.extend(next);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::Stopped { .. }))
            .count(),
        2,
        "late cancel must not add a second terminal to the unsuccessful turn"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "only explicit prompts reach the same connection"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Event::AcpSessionAssigned { .. }))
            .count(),
        1
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::AgentStartupError { .. } | Event::SessionContextReset { .. }
    )));
    fake.event_rx.stop_and_forget().await;
    let _ = fake.connection.await;
    let public = serde_json::to_string(&events).unwrap();
    assert!(!public.contains("privateTokenMarker") && !public.contains("must-not-leak"));
    fake.agent.abort();
    let _ = fake.agent.await;
} })).await;
}

#[tokio::test]
async fn transport_and_protocol_failure_remain_fatal_across_turn_ownership() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { for mode in ["idle", "active", "adopted", "cancelling", "after_error"] {
    let (entered, mut prompts) = mpsc::unbounded_channel();
    let resume = (mode == "adopted").then(|| ConnectMode::Resume {
        acp_session_id: "s-eof".into(),
        in_flight_turn: true,
        unsuccessful_main: false,
    });
    let mut fake = spawn_fake_connection(scope, "s-eof", resume, mode == "after_error", move |id| {
        entered.send(()).unwrap();
        if mode == "after_error" {
            vec![serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32603, "message": "request failed",
                    "data": {"privateTokenMarker": "must-not-leak"}}
            })
            .to_string()]
        } else {
            Vec::new()
        }
    })
    .await;
    if !matches!(mode, "idle" | "adopted") {
        fake.cmd_tx
            .send(ClientCmd::Prompt(Vec::new()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), prompts.recv())
            .await
            .unwrap()
            .unwrap();
    }
    if mode == "cancelling" {
        fake.cmd_tx.send(ClientCmd::Cancel).await.unwrap();
    }
    if mode != "after_error" {
        fake.agent_write.lock().await.shutdown().await.unwrap();
    }
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), fake.connection)
        .await
        .unwrap_or_else(|error| {
            panic!("{mode}: transport failure did not end its turn ownership: {error}")
        })
        .unwrap();
    while let Some(event) = fake.event_rx.recv().await {
        events.push(event);
    }
    assert!(
        events.iter().any(|event| matches!(
            event,
            Event::AcpConnectionUnavailable {
                phase: crate::acp::state::AcpConnectionPhase::Established,
                ..
            }
        )),
        "{mode}: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::AgentStartupError { .. })),
        "post-ready closure is not startup failure: {mode}: {events:?}"
    );
    if mode == "after_error" {
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::PromptRuntimeError { .. }))
                .count(),
            1
        );
        assert_eq!(events.iter().filter(|event| matches!(event, Event::Stopped { reason } if reason == "prompt_failed")).count(), 1);
        assert!(!events.iter().any(
            |event| matches!(event, Event::Stopped { reason } if reason == "prompt_complete")
        ));
    } else {
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::PromptRuntimeError { .. })),
            "EOF is not an agent response"
        );
    }
    assert!(!serde_json::to_string(&events)
        .unwrap()
        .contains("privateTokenMarker"));
    fake.event_rx.shutdown().await.expect("retire exact EOF case owner");
    fake.agent.abort();
    let _ = fake.agent.await;
} })).await;
}

#[tokio::test]
async fn unsupported_session_prompt_prose_preserves_identity_and_the_next_explicit_input() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
let native_calls = calls.clone();
let mut fake = spawn_fake_connection(scope, "s-rejected",
Some(ConnectMode::Resume {
    acp_session_id: "s-rejected".into(),
    in_flight_turn: false,
    unsuccessful_main: false,
}),
false,
move |id| {
    let reply = if native_calls.fetch_add(1, Ordering::SeqCst) == 0 {
        serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32603, "message": "unknown session",
                "data": {"privateTokenMarker": "must-not-leak"}}
        })
    } else {
        serde_json::json!({
            "jsonrpc": "2.0", "id": id, "result": {"stopReason": "end_turn"}
        })
    };
    vec![reply.to_string()]
},)
.await;
fake.cmd_tx
    .send(ClientCmd::Prompt(Vec::new()))
    .await
    .unwrap();
let mut events = collect_terminal(&mut fake).await;
assert!(matches!(events.last(), Some(Event::Stopped { reason }) if reason == "prompt_failed"));
assert!(events
    .iter()
    .any(|event| matches!(event, Event::PromptRuntimeError { .. })));
assert!(!fake.connection.is_finished());
fake.cmd_tx
    .send(ClientCmd::Prompt(Vec::new()))
    .await
    .unwrap();
let next = collect_terminal(&mut fake).await;
assert!(matches!(next.last(), Some(Event::Stopped { reason }) if reason == "prompt_complete"));
events.extend(next);
assert_eq!(calls.load(Ordering::SeqCst), 2);
let assigned: Vec<_> = events
    .iter()
    .filter_map(|event| match event {
        Event::AcpSessionAssigned { acp_session_id, .. } => Some(acp_session_id.as_str()),
        _ => None,
    })
    .collect();
assert_eq!(assigned, vec!["s-rejected"]);
assert!(!events.iter().any(|event| matches!(
    event,
    Event::SessionContextReset { .. } | Event::AgentStartupError { .. }
)));
let public = serde_json::to_string(&events).unwrap();
assert!(!public.contains("privateTokenMarker") && !public.contains("must-not-leak"));
fake.event_rx.shutdown().await.expect("retire exact case owner");
let _ = fake.connection.await;
fake.agent.abort();
let _ = fake.agent.await; })).await;
}

#[tokio::test]
async fn request_failure_keeps_the_pending_permission_responder_usable() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {
use crate::acp::state::{AcpSessionId, AcpState, AgentName};
for kind in ["permission", "elicitation"] {
    let (prompt_ids, mut requested) = mpsc::unbounded_channel();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let native_calls = calls.clone();
    let mut fake = spawn_fake_connection(scope, "s-permission-error", None, false, move |id| {
        if native_calls.fetch_add(1, Ordering::SeqCst) != 0 {
            return vec![serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}).to_string()];
        }
        prompt_ids.send(id.clone()).unwrap();
        let (method, params) = if kind == "permission" {
            ("session/request_permission", serde_json::json!({
                "sessionId":"s-permission-error",
                "toolCall":{"toolCallId":"pending-tool","title":"Wait for user"},
                "options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]
            }))
        } else {
            ("elicitation/create", serde_json::json!({
                "mode":"form","sessionId":"s-permission-error","message":"Choose",
                "requestedSchema":{"type":"object","properties":{"answer":{"type":"string"}}}
            }))
        };
        vec![serde_json::json!({"jsonrpc":"2.0","id":"pending-interaction","method":method,"params":params}).to_string()]
    }).await;
    fake.cmd_tx
        .send(ClientCmd::Prompt(Vec::new()))
        .await
        .unwrap();
    let mut events = Vec::new();
    let nonce = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = fake.event_rx.recv().await.expect("connection remains live");
            let nonce = match &event {
                Event::ApprovalRequested { approval } => Some(approval.nonce.clone()),
                Event::ElicitationRequested { elicitation } => Some(elicitation.nonce.clone()),
                _ => None,
            };
            events.push(event);
            if let Some(nonce) = nonce {
                break nonce;
            }
        }
    })
    .await
    .unwrap_or_else(|error| panic!("{kind}: interaction was not registered: {error}"));
    assert!(fake.pending_responders.lock().await.contains_key(&nonce));
    let id = requested.recv().await.unwrap();
    write_line(
        &fake.agent_write,
        &serde_json::json!({
            "jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":"s-permission-error","update":{
                    "sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":"progress while user decision is pending"}
                }
            }
        })
        .to_string(),
    )
    .await;
    write_line(
        &fake.agent_write,
        &serde_json::json!({
            "jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"request failed"}
        })
        .to_string(),
    )
    .await;
    events.extend(collect_terminal(&mut fake).await);
    let progress = events.iter().position(|event| matches!(event,
        Event::AgentMessageChunk { text, .. } if text == "progress while user decision is pending"
    )).expect("pending callback must not block intervening progress");
    let diagnostic = events
        .iter()
        .position(|event| matches!(event, Event::PromptRuntimeError { .. }))
        .unwrap();
    assert!(progress < diagnostic);
    assert!(fake.pending_responders.lock().await.contains_key(&nonce));
    assert!(
        matches!(events.last(), Some(Event::Stopped { reason }) if reason == "prompt_failed")
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::ApprovalResolved { .. } | Event::ElicitationResolved { .. }
    )));
    resolve_registered_interaction(&fake, &nonce).await;
    let response = tokio::time::timeout(Duration::from_secs(10), fake.replies.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response["id"], "pending-interaction");
    if kind == "permission" {
        assert_eq!(response["result"]["outcome"]["outcome"], "selected");
        assert_eq!(response["result"]["outcome"]["optionId"], "allow");
    } else {
        assert_eq!(response["result"]["action"], "decline");
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = fake.event_rx.recv().await.unwrap();
            let resolved = matches!(&event, Event::ApprovalResolved { nonce: got, .. } | Event::ElicitationResolved { nonce: got, .. } if got == &nonce);
            events.push(event);
            if resolved { break; }
        }
    }).await.unwrap();
    for (session_update, text) in [
        ("agent_thought_chunk", "callback bookkeeping"),
        ("agent_message_chunk", "interaction reply echoed"),
    ] {
        write_line(
            &fake.agent_write,
            &serde_json::json!({
                "jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":"s-permission-error","update":{
                        "sessionUpdate":session_update,"content":{"type":"text","text":text}
                    }
                }
            })
            .to_string(),
        )
        .await;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = fake.event_rx.recv().await.unwrap();
            assert!(!matches!(event, Event::ThinkingStarted), "post-failure interaction output cannot reopen the failed main turn");
            let echoed = matches!(&event, Event::AgentMessageChunk { text, .. } if text == "interaction reply echoed");
            events.push(event);
            if echoed { break; }
        }
    }).await.unwrap();
    let stored = scope.state.acp_event_store.replay_from(&fake.event_rx.id, 0).unwrap();
    let terminal = stored.iter().find(|stored| {
        matches!(stored.event.as_ref(), Event::Stopped { .. })
            && stored.metadata.native_prompt_id == Some(id.as_i64().expect("actual Native prompt ID"))
    }).expect("the failed Native prompt has its own correlated terminal");
    assert!(terminal.metadata.native_origin.is_some());
    assert!(!terminal.metadata.host_action);
    assert_eq!(
        stored.iter().filter(|stored| {
            matches!(stored.event.as_ref(), Event::Stopped { .. })
                && stored.metadata.native_prompt_id == terminal.metadata.native_prompt_id
                && stored.metadata.native_origin == terminal.metadata.native_origin
        }).count(),
        1,
        "callback bookkeeping cannot duplicate the original Native prompt terminal"
    );
    let mut state = AcpState::new(
        AcpSessionId(fake.event_rx.id.clone()),
        AgentName("gemini".into()),
        None,
    );
    for stored in scope.state.acp_event_store.replay_from(&fake.event_rx.id, 0).unwrap() {
        state.apply_scoped_event(stored.seq, stored.event.as_ref(), &stored.metadata).unwrap();
    }
    assert!(!state.turn_active);
    assert_eq!(state.sidebar_status(), crate::session::Status::Error);
    assert!(state.pending_approvals.is_empty() && state.pending_elicitations.is_empty());
    assert!(!fake.connection.is_finished());
    fake.cmd_tx
        .send(ClientCmd::Prompt(Vec::new()))
        .await
        .unwrap();
    collect_terminal(&mut fake).await;
    assert_native_prompt_succeeded(scope, &fake);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    fake.event_rx.stop_and_forget().await;
    let _ = fake.connection.await;
    fake.agent.abort();
    let _ = fake.agent.await;
} })).await;
}

#[tokio::test]
async fn request_error_then_eof_survives_public_event_backpressure() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| {
        Box::pin(async move {
            let (prompt_seen, mut prompts) = mpsc::unbounded_channel();
            let mut fake = spawn_fake_connection(scope, "s-backpressure", None, false, move |id| {
                prompt_seen.send(id.clone()).unwrap();
                Vec::new()
            })
            .await;
            fake.cmd_tx
                .send(ClientCmd::Prompt(Vec::new()))
                .await
                .unwrap();
            let id = tokio::time::timeout(Duration::from_secs(10), prompts.recv())
                .await
                .expect("actual prompt admission precedes public backpressure")
                .unwrap();
            let drain_pause = fake.event_rx.pause_drain().await;
            let selected = fake.event_rx.selected_native_session();
            for index in 0..80 {
                write_line(
                    &fake.agent_write,
                    &serde_json::json!({
                        "jsonrpc":"2.0","method":"session/update","params":{
                            "sessionId":selected,"update":{
                                "sessionUpdate":"agent_message_chunk",
                                "content":{"type":"text","text":format!("partial output {index}\n")}
                            }
                        }
                    })
                    .to_string(),
                )
                .await;
            }
            write_line(
                &fake.agent_write,
                &serde_json::json!({
                    "jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"request failed"}
                })
                .to_string(),
            )
            .await;
            fake.agent_write.lock().await.shutdown().await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let capacity = fake
                        .event_queue
                        .upgrade()
                        .expect("connection still owns its event queue")
                        .capacity();
                    if capacity == 0 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("public event queue fills before draining resumes");
            drop(drain_pause);
            let mut events = collect_terminal(&mut fake).await;
            tokio::time::timeout(Duration::from_secs(10), fake.connection)
                .await
                .unwrap()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(event) = fake.event_rx.recv().await {
                    events.push(event);
                }
            })
            .await
            .expect("actual EOF retirement closes the publication observer");
            let failure = events
                .iter()
                .position(|event| matches!(event, Event::PromptRuntimeError { .. }))
                .unwrap();
            let terminal = events
                .iter()
                .position(
                    |event| matches!(event, Event::Stopped { reason } if reason == "prompt_failed"),
                )
                .unwrap();
            let closure = events
                .iter()
                .position(|event| matches!(event, Event::AcpConnectionUnavailable { .. }))
                .unwrap();
            assert!(failure < terminal && terminal < closure);
            assert!(!events
                .iter()
                .any(|event| matches!(event, Event::AgentStartupError { .. })));
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Event::Stopped { .. }))
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Event::AgentMessageChunk { .. }))
                    .count(),
                80
            );
            fake.event_rx
                .shutdown()
                .await
                .expect("retire exact EOF case owner");
            fake.agent.abort();
            let _ = fake.agent.await;
        })
    })
    .await;
}

#[tokio::test]
async fn malformed_json_retains_the_sdk_parse_error_policy_and_live_connection() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { let mut fake = spawn_fake_connection(scope, "s-parse-error", None, false, |id| {
    vec![
        serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}})
            .to_string(),
    ]
})
.await;
let (observed, mut sdk_output) = mpsc::unbounded_channel();
fake.event_rx.actual().observe_sdk_output(observed);
fake.event_rx.actual().inject_sdk_line("{not valid JSON").await;
let response = tokio::time::timeout(Duration::from_secs(10), sdk_output.recv())
    .await
    .unwrap()
    .unwrap();
assert_eq!(response["error"]["code"], -32700);
assert!(
    !fake.connection.is_finished(),
    "SDK replies to malformed input instead of closing transport"
);
fake.cmd_tx
    .send(ClientCmd::Prompt(Vec::new()))
    .await
    .unwrap();
let events = collect_terminal(&mut fake).await;
assert!(
    matches!(events.last(), Some(Event::Stopped { reason }) if reason == "prompt_complete")
);
assert!(!events.iter().any(|event| matches!(
    event,
    Event::AgentStartupError { .. } | Event::PromptRuntimeError { .. }
)));
fake.event_rx.shutdown().await.expect("retire exact case owner");
let _ = fake.connection.await;
fake.agent.abort();
let _ = fake.agent.await; })).await;
}

#[tokio::test]
async fn successful_explicit_reset_releases_failed_main_terminal_ownership() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { use super::super::reset::ResetSessionOutcome;
let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
let native_calls = calls.clone();
let mut fake = spawn_fake_connection(scope, "s-reset-failure", None, false, move |id| {
    if native_calls.fetch_add(1, Ordering::SeqCst) == 0 {
        vec![serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"request failed"}}).to_string()]
    } else {
        vec![serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}).to_string()]
    }
}).await;
fake.cmd_tx
    .send(ClientCmd::Prompt(Vec::new()))
    .await
    .unwrap();
let failed = collect_terminal(&mut fake).await;
assert!(matches!(failed.last(), Some(Event::Stopped { reason }) if reason == "prompt_failed"));
let (respond_to, response) = oneshot::channel();
let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
let proposal = fake.event_rx.prepare_reset(deadline).await;
fake.cmd_tx
    .send(ClientCmd::ResetSession {
        text: "/clear".into(),
        deadline,
        proposal,
        respond_to,
    })
    .await
    .unwrap();
let reset = collect_terminal(&mut fake).await;
assert!(
    matches!(response.await.unwrap(), ResetSessionOutcome::Reset { new_acp_session_id } if new_acp_session_id == "s-reset-failure-reset-1")
);
assert!(reset
    .iter()
    .any(|event| matches!(event, Event::SessionCleared)));
assert!(matches!(reset.last(), Some(Event::Stopped { reason }) if reason == "session_reset"));
write_line(&fake.agent_write, &serde_json::json!({
    "jsonrpc":"2.0","method":"session/update","params":{
        "sessionId":"s-reset-failure-reset-1","update":{
            "sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"new context activity"}
        }
    }
}).to_string()).await;
tokio::time::timeout(Duration::from_secs(10), async {
    loop {
        if matches!(fake.event_rx.recv().await.unwrap(), Event::ThinkingStarted) {
            break;
        }
    }
})
.await
.expect("intentional successful reset releases the prior failed outcome fence");
fake.cmd_tx
    .send(ClientCmd::Prompt(Vec::new()))
    .await
    .unwrap();
assert!(
    matches!(collect_terminal(&mut fake).await.last(), Some(Event::Stopped { reason }) if reason == "prompt_complete")
);
assert_eq!(calls.load(Ordering::SeqCst), 2);
fake.event_rx.shutdown().await.expect("retire exact case owner");
let _ = fake.connection.await;
fake.agent.abort();
let _ = fake.agent.await; })).await;
}

fn pending_interaction_line(kind: &str, session_id: &str) -> String {
    let (method, params) = if kind == "permission" {
        (
            "session/request_permission",
            serde_json::json!({
                "sessionId":session_id,
                "toolCall":{"toolCallId":"pending-tool","title":"Wait for user"},
                "options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]
            }),
        )
    } else {
        (
            "elicitation/create",
            serde_json::json!({
                "mode":"form","sessionId":session_id,"message":"Choose",
                "requestedSchema":{"type":"object","properties":{"answer":{"type":"string"}}}
            }),
        )
    };
    serde_json::json!({"jsonrpc":"2.0","id":"pending-interaction","method":method,"params":params})
        .to_string()
}

async fn registered_interaction(fake: &mut FakeConnection) -> Nonce {
    let nonce = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match fake.event_rx.recv().await.expect("connection remains live") {
                Event::ApprovalRequested { approval } => break approval.nonce,
                Event::ElicitationRequested { elicitation } => break elicitation.nonce,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    assert!(fake.pending_responders.lock().await.contains_key(&nonce));
    nonce
}

async fn resolve_registered_interaction(fake: &FakeConnection, nonce: &Nonce) {
    use super::super::pending::PendingResolver;
    use axum::response::IntoResponse;
    let approval = {
        let pending = fake.pending_responders.lock().await;
        matches!(
            &pending
                .get(nonce)
                .expect("responder remains registered")
                .resolver,
            PendingResolver::Approval(_)
        )
    };
    let state = axum::extract::State(fake.event_rx.app_state());
    let path = axum::extract::Path((fake.event_rx.id.clone(), nonce.0.clone()));
    let response = if approval {
        crate::server::api::resolve_approval(
            state,
            path,
            Ok(axum::Json(crate::acp::protocol::ResolveApprovalRequest {
                decision: crate::acp::protocol::ApprovalDecisionWire::Allow,
                option_id: Some("allow".into()),
            })),
        )
        .await
        .into_response()
    } else {
        crate::server::api::resolve_elicitation(
            state,
            path,
            Ok(axum::Json(
                crate::acp::elicitations::ElicitationResolution::Decline,
            )),
        )
        .await
        .into_response()
    };
    assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn pending_callback_reset_write_waits_for_resolution_or_deadline() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { use super::super::reset::ResetSessionOutcome;
for kind in ["permission", "elicitation"] {
    for resolve_before_deadline in [true, false] {
        let mut fake =
            spawn_fake_connection(scope, "s-pending-reset", None, false, |_| Vec::new()).await;
        write_line(
            &fake.agent_write,
            &pending_interaction_line(kind, "s-pending-reset"),
        )
        .await;
        let nonce = registered_interaction(&mut fake).await;
        let (write_pending, writing) = oneshot::channel();
        *fake.event_rx.actual().test_connection_state().test_fresh_write_pending.lock().unwrap() = Some(write_pending);
        let (respond_to, mut response) = oneshot::channel();
        let bound = Duration::from_secs(if resolve_before_deadline { 10 } else { 2 });
        let deadline = tokio::time::Instant::now() + bound;
        let proposal = fake.event_rx.prepare_reset(deadline).await;
        fake.cmd_tx
            .send(ClientCmd::ResetSession {
                text: "/clear".into(),
                deadline,
                proposal,
                respond_to,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), writing)
            .await
            .expect("real Fresh writer reached the pending callback fence")
            .unwrap();
        write_line(&fake.agent_write, &serde_json::json!({
            "jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":"s-pending-reset","update":{
                    "sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":"old-session progress while reset waits"}
                }
            }
        }).to_string()).await;
        if resolve_before_deadline {
            assert!(matches!(
                response.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ));
            resolve_registered_interaction(&fake, &nonce).await;
            let reply = tokio::time::timeout(Duration::from_secs(10), fake.replies.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reply["id"], "pending-interaction");
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(10), response).await.unwrap().unwrap(),
                ResetSessionOutcome::Reset { new_acp_session_id } if new_acp_session_id == "s-pending-reset-reset-1"
            ));
            let events = collect_terminal(&mut fake).await;
            assert!(events
                .iter()
                .any(|event| matches!(event, Event::SessionCleared)));
            assert!(
                matches!(events.last(), Some(Event::Stopped { reason }) if reason == "session_reset")
            );
            let history = scope.state.acp_event_store.replay_from(&fake.event_rx.id, 0).unwrap();
            let progress: Vec<_> = history.iter().filter(|stored| matches!(stored.event.as_ref(),
                Event::AgentMessageChunk { text, .. } if text == "old-session progress while reset waits"
            )).collect();
            assert_eq!(progress.len(), 1, "physical progress remains history across the reset");
            assert_eq!(progress[0].metadata.scope, crate::acp::event_sender::EventScope::HistoryOnly);
        } else {
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(10), response).await.unwrap().unwrap(),
                ResetSessionOutcome::Failed { .. }
            ));
            assert!(fake.pending_responders.lock().await.contains_key(&nonce));
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let event = fake.event_rx.recv().await.unwrap();
                    assert!(!matches!(event, Event::SessionCleared));
                    if matches!(event, Event::AgentMessageChunk { text, .. } if text == "old-session progress while reset waits") { break; }
                }
            }).await.unwrap();
            assert!(
                fake.pending_responders.lock().await.contains_key(&nonce),
                "expired reset writer releases progress without resolving the card"
            );
            resolve_registered_interaction(&fake, &nonce).await;
            let reply = tokio::time::timeout(Duration::from_secs(10), fake.replies.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reply["id"], "pending-interaction");
            while let Ok(event) = fake.event_rx.try_recv() {
                assert!(!matches!(event, Event::SessionCleared));
            }
        }
        fake.event_rx.stop_and_forget().await;
        let _ = fake.connection.await;
        fake.agent.abort();
        let _ = fake.agent.await;
    }
} })).await;
}

#[tokio::test]
async fn transport_eof_cancels_unanswered_callbacks_after_intervening_progress() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { use super::super::pending::PendingResolver;
for kind in ["permission", "elicitation"] {
    let mut fake = spawn_fake_connection(scope, "s-pending-eof", None, false, move |_| {
        vec![pending_interaction_line(kind, "s-pending-eof")]
    })
    .await;
    fake.cmd_tx
        .send(ClientCmd::Prompt(Vec::new()))
        .await
        .unwrap();
    let nonce = registered_interaction(&mut fake).await;
    write_line(&fake.agent_write, &serde_json::json!({
        "jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"s-pending-eof","update":{
                "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"progress before EOF"}
            }
        }
    }).to_string()).await;
    fake.agent_write.lock().await.shutdown().await.unwrap();
    let events = tokio::time::timeout(Duration::from_secs(10), async {
        let mut events = Vec::new();
        while let Some(event) = fake.event_rx.recv().await {
            events.push(event);
        }
        events
    })
    .await
    .unwrap_or_else(|error| panic!("{kind}: unanswered callback blocked EOF: {error}"));
    tokio::time::timeout(Duration::from_secs(10), &mut fake.connection)
        .await
        .unwrap()
        .unwrap();
    assert!(events.iter().any(|event| matches!(event, Event::AgentMessageChunk { text, .. } if text == "progress before EOF")));
    assert!(events
        .iter()
        .any(|event| matches!(event, Event::AcpConnectionUnavailable { .. })));
    assert!(!events
        .iter()
        .any(|event| matches!(event, Event::AgentStartupError { .. })));
    assert!(!events.iter().any(
        |event| matches!(event, Event::Stopped { reason } if reason == "prompt_complete")
    ));
    let pending = fake.pending_responders.lock().await;
    if let Some(pending) = pending.get(&nonce) {
        assert!(
            match &pending.resolver {
                PendingResolver::Approval(resolver) => resolver.is_closed(),
                PendingResolver::Elicitation { resolver, .. } => resolver.is_closed(),
            },
            "fatal closure cancels the SDK-owned callback"
        );
    }
    drop(pending);
    fake.event_rx.shutdown().await.expect("retire exact EOF case owner");
    fake.agent.abort();
    let _ = fake.agent.await;
} })).await;
}
