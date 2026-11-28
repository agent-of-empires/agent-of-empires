//! Actual owned runner and SDK task, with original unit-test peers over the native bridge.

use super::*;
use crate::acp::acp_client::commands::ClientCmd;
use crate::acp::acp_client::commands::PromptInput;
use crate::acp::acp_client::reset::ResetSessionOutcome;
use crate::acp::acp_client::{ProxyFailurePolicy, ProxySpec, SdkTaskObservation, SdkTestOptions};
use crate::acp::agent_compat::ProxyCompat;
use crate::acp::approvals::ApprovalDecision;
use crate::acp::approvals::Nonce;
use crate::acp::event_sender::WeakEventSender;
use crate::acp::state::{IntegrationLostReason, StartupErrorDetail, SubagentState};
use crate::acp::supervisor::test_support::managed::{ManagedClient, ManagedScope, ManagedTask};
use agent_client_protocol::v1::schema::{ContentBlock, TextContent};
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
            .send(ClientCmd::Prompt(
                vec![ContentBlock::Text(TextContent::new("hi"))].into(),
            ))
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
                live_recovery: None,
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
                    live_recovery: None,
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
    responses: mpsc::UnboundedReceiver<serde_json::Value>,
    received: Arc<Mutex<Vec<serde_json::Value>>>,
    event_queue: WeakEventSender,
    connection_state: Arc<ConnectionState>,
    connection: SdkTaskObservation,
    agent: ManagedTask,
    native_prompt_id: Arc<std::sync::atomic::AtomicI64>,
    set_options: Arc<Mutex<Vec<String>>>,
    _cwd: Arc<tempfile::TempDir>,
}

async fn spawn_fake_connection(
    scope: &mut ManagedScope,
    session_id: &'static str,
    mode: Option<ConnectMode>,
    close_after_prompt: bool,
    on_prompt: impl Fn(&serde_json::Value) -> Vec<String> + Send + 'static,
) -> FakeConnection {
    spawn_fake_connection_with(
        scope,
        session_id,
        mode,
        close_after_prompt,
        None,
        None,
        on_prompt,
    )
    .await
}

/// `config_options` is the raw `configOptions` array `session/new` returns.
async fn spawn_fake_connection_with(
    scope: &mut ManagedScope,
    session_id: &'static str,
    mode: Option<ConnectMode>,
    close_after_prompt: bool,
    default_mode: Option<&'static str>,
    config_options: Option<&'static str>,
    on_prompt: impl Fn(&serde_json::Value) -> Vec<String> + Send + 'static,
) -> FakeConnection {
    let (ready, fake) = spawn_fake_connection_raw(
        scope,
        session_id,
        mode,
        close_after_prompt,
        "",
        (default_mode, config_options),
        on_prompt,
    )
    .await;
    ready.expect("handshake ok");
    fake
}

/// `init_extra` is spliced into the `initialize` result after
/// `agentCapabilities`, e.g. a `_meta` hello.
async fn spawn_fake_connection_raw(
    scope: &mut ManagedScope,
    session_id: &'static str,
    mode: Option<ConnectMode>,
    close_after_prompt: bool,
    init_extra: &'static str,
    (default_mode, config_options): (Option<&'static str>, Option<&'static str>),
    on_prompt: impl Fn(&serde_json::Value) -> Vec<String> + Send + 'static,
) -> (Result<(), AcpError>, FakeConnection) {
    let (daemon_write, agent_read) = tokio::io::duplex(64 * 1024);
    let (agent_write, daemon_read) = tokio::io::duplex(64 * 1024);
    let agent_write: SharedWrite = Arc::new(Mutex::new(agent_write));
    scope.configure_sdk(SdkTestOptions {
        command_capacity: Some(1),
        ..Default::default()
    });
    let (response_tx, responses) = mpsc::unbounded_channel();
    let temp = scope.tempdir();
    let mut config = scope.bridge_config(temp.path().to_path_buf(), daemon_write, daemon_read);
    config.default_mode = default_mode.map(str::to_string);
    let reply_write = agent_write.clone();
    let native_prompt_id = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let observed_prompt_id = Arc::clone(&native_prompt_id);
    let set_options = Arc::new(Mutex::new(Vec::new()));
    let recorded = set_options.clone();
    let options_json = config_options.unwrap_or("[]");
    let received = Arc::new(Mutex::new(Vec::new()));
    let received_frames = received.clone();
    let agent = scope.spawn_helper(async move {
        let mut lines = BufReader::new(agent_read).lines();
        let mut new_sessions = 0;
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
            received_frames.lock().await.push(msg.clone());
            let id = &msg["id"];
            let replies = match msg["method"].as_str() {
                Some("initialize") => vec![format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1,"agentCapabilities":{{}}{init_extra}}}}}"#
                )],
                Some("session/new") => {
                    let native = if new_sessions == 0 {
                        session_id.to_string()
                    } else {
                        format!("{session_id}-reset-{new_sessions}")
                    };
                    new_sessions += 1;
                    vec![format!(
                        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"sessionId":"{native}","configOptions":{options_json}}}}}"#
                    )]
                }
                Some("session/set_config_option") => {
                    let value = msg["params"]["value"].as_str().unwrap_or_default();
                    recorded.lock().await.push(value.to_string());
                    vec![format!(
                        r#"{{"jsonrpc":"2.0","id":{id},"result":{{"configOptions":{}}}}}"#,
                        options_json.replace(
                            r#""currentValue":"auto""#,
                            &format!(r#""currentValue":"{value}""#)
                        )
                    )]
                }
                Some("session/prompt") => {
                    observed_prompt_id.store(id.as_i64().expect("actual Native prompt ID"), Ordering::Release);
                    on_prompt(id)
                }
                None => {
                    let _ = response_tx.send(msg);
                    continue;
                }
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
    let event_rx = match mode {
        Some(ConnectMode::Resume { in_flight_turn, .. }) => {
            original.ready().await;
            scope.adopt(original, in_flight_turn).await
        }
        _ => original,
    };
    let actual = event_rx.actual();
    let ready = actual.wait_until_usable().await;
    if ready.is_ok() {
        event_rx.ready().await;
        assert_eq!(
            actual.selected_native_session(),
            session_id,
            "only the actual Original result selects identity"
        );
    }
    let cmd_tx = actual.test_command_sender();
    let pending_responders = actual.test_pending_responders();
    let connection_state = actual.test_connection_state();
    let connection = actual.observe_sdk_task();
    let event_queue = actual.test_event_queue();
    let fake = FakeConnection {
        event_rx,
        cmd_tx,
        agent_write,
        pending_responders,
        event_queue,
        connection_state,
        responses,
        received,
        connection,
        agent,
        native_prompt_id,
        set_options,
        _cwd: temp,
    };
    (ready, fake)
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
                    remember: false,
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
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {
let prompts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
let observed = Arc::clone(&prompts);
let mut fake = spawn_fake_connection(scope, "s-limit", None, false, move |id| {
    if observed.fetch_add(1, Ordering::SeqCst) == 0 {
        Vec::new()
    } else {
        vec![serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}).to_string()]
    }
}).await;

tokio::time::timeout(Duration::from_secs(10), async {
    while !matches!(
        fake.event_rx.recv().await.expect("connection remains open"),
        Event::AcpSessionAssigned { .. }
    ) {}
})
.await
.expect("session commits");

    // Accounting for a turn nobody prompted, carrying a rejected window.
    let resets_at = crate::time::Timestamp::now().as_unix_secs() + 3600;
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
// Ambient accounting supplies no cancellable main. Issue a genuine Native
// prompt before exercising the unchanged positive cancellation assertion.
fake.cmd_tx.send(ClientCmd::Prompt(PromptInput::default())).await.unwrap();
let native_prompt = tokio::time::timeout(Duration::from_secs(10), async {
    loop {
        let id = fake.native_prompt_id.load(Ordering::Acquire);
        if id > 0 { break id; }
        tokio::task::yield_now().await;
    }
}).await.expect("the actual Native peer receives the cancellable prompt");
fake.cmd_tx.send(ClientCmd::Cancel).await.unwrap();
tokio::time::timeout(Duration::from_secs(10), async {
    loop {
        if fake.received.lock().await.iter().any(|frame|
            frame["method"] == "session/cancel" && frame["params"]["sessionId"] == "s-limit"
        ) { break; }
        tokio::task::yield_now().await;
    }
}).await.expect("cancellation reaches the same actual Native session");
write_line(&fake.agent_write, &serde_json::json!({
    "jsonrpc":"2.0","id":native_prompt,"result":{"stopReason":"cancelled"}
}).to_string()).await;
let cancelled = collect_terminal(&mut fake).await;
assert!(matches!(cancelled.last(), Some(Event::Stopped { reason }) if reason == "cancelled"));
fake.cmd_tx
    .send(ClientCmd::Prompt(PromptInput::default()))
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
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { let now = crate::time::Timestamp::now().as_unix_secs();
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
        )].into()))
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
        matches!(e, Event::RateLimit { info } if info.resets_at.map(|t| t.as_unix_secs()) == Some(resets_at))
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
        .send(ClientCmd::Prompt(Vec::new().into()))
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
        .send(ClientCmd::Prompt(Vec::new().into()))
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
            matches!(events.last(), Some(Event::Stopped { reason }) if reason == "prompt_rejected")
        );
    }
    assert!(!fake.connection.is_finished());
    fake.cmd_tx.send(ClientCmd::Cancel).await.unwrap();
    fake.cmd_tx
        .send(ClientCmd::Prompt(Vec::new().into()))
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
        live_recovery: None,
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
            .send(ClientCmd::Prompt(PromptInput::default()))
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
        assert_eq!(events.iter().filter(|event| matches!(event, Event::Stopped { reason } if reason == "prompt_rejected")).count(), 1);
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
    live_recovery: None,
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
    .send(ClientCmd::Prompt(PromptInput::default()))
    .await
    .unwrap();
let mut events = collect_terminal(&mut fake).await;
assert!(matches!(events.last(), Some(Event::Stopped { reason }) if reason == "prompt_rejected"));
assert!(events
    .iter()
    .any(|event| matches!(event, Event::PromptRuntimeError { .. })));
assert!(!fake.connection.is_finished());
fake.cmd_tx
    .send(ClientCmd::Prompt(PromptInput::default()))
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
#[serial_test::serial]
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
        .send(ClientCmd::Prompt(Vec::new().into()))
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
        matches!(events.last(), Some(Event::Stopped { reason }) if reason == "prompt_rejected")
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        Event::ApprovalResolved { .. } | Event::ElicitationResolved { .. }
    )));
    resolve_registered_interaction(&fake, &nonce).await;
    let response = tokio::time::timeout(Duration::from_secs(10), fake.responses.recv())
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
            assert!(!matches!(event, Event::ThinkingStarted | Event::Stopped { .. }), "post-failure interaction output cannot reopen the failed main turn or own another terminal");
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
        .send(ClientCmd::Prompt(PromptInput::default()))
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
#[serial_test::serial]
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
                .send(ClientCmd::Prompt(PromptInput::default()))
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
                    |event| matches!(event, Event::Stopped { reason } if reason == "prompt_rejected"),
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
    .send(ClientCmd::Prompt(PromptInput::default()))
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
    .send(ClientCmd::Prompt(PromptInput::default()))
    .await
    .unwrap();
let failed = collect_terminal(&mut fake).await;
assert!(matches!(failed.last(), Some(Event::Stopped { reason }) if reason == "prompt_rejected"));
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
fake.cmd_tx
    .send(ClientCmd::Prompt(PromptInput::default()))
    .await
    .unwrap();
tokio::time::timeout(Duration::from_secs(10), async {
    loop {
        if matches!(fake.event_rx.recv().await.unwrap(), Event::ThinkingStarted) {
            break;
        }
    }
})
.await
.expect("intentional successful reset releases the prior failed outcome fence");
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

async fn collect_connection_events_until(
    fake: &mut FakeConnection,
    observed: &mut Vec<Event>,
    done: impl Fn(&Event) -> bool,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = fake.event_rx.recv().await.expect("connection remains live");
            let complete = done(&event);
            observed.push(event);
            if complete {
                return;
            }
        }
    })
    .await
    .expect("expected connection event arrives within the existing bound");
}

async fn registered_interaction(fake: &mut FakeConnection, observed: &mut Vec<Event>) -> Nonce {
    collect_connection_events_until(fake, observed, |event| {
        matches!(
            event,
            Event::ApprovalRequested { .. } | Event::ElicitationRequested { .. }
        )
    })
    .await;
    let nonce = match observed.last().unwrap() {
        Event::ApprovalRequested { approval } => approval.nonce.clone(),
        Event::ElicitationRequested { elicitation } => elicitation.nonce.clone(),
        _ => unreachable!(),
    };
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
#[serial_test::serial]
async fn parked_callback_refuses_clear_until_a_new_explicit_reset() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {

    use crate::acp::elicitations::ElicitationOutcome;
    let update = |session_id: &str, text: &str| {
        serde_json::json!({
            "jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":session_id,"update":{
                    "sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":text}
                }
            }
        })
        .to_string()
    };
    let boundary = |event: &Event| match event {
        Event::SessionCleared
        | Event::SessionContextReset { .. }
        | Event::AcpSessionAssigned { .. } => true,
        Event::Stopped { reason } => reason == "session_reset",
        _ => false,
    };
    for kind in ["permission", "elicitation"] {
        let mut fake = spawn_fake_connection(scope, "s-pending-reset", None, false, |id| {
            vec![serde_json::json!({
                "jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}
            })
            .to_string()]
        })
        .await;
        let mut observed = Vec::new();
        collect_connection_events_until(&mut fake, &mut observed, |event| {
            matches!(event, Event::AcpSessionAssigned { acp_session_id, connection_ready: true, .. } if acp_session_id == "s-pending-reset")
        }).await;
        let initial_end = observed.len();
        assert_eq!(
            fake.received
                .lock()
                .await
                .iter()
                .filter(|frame| frame["method"] == "session/new")
                .count(),
            1
        );
        write_line(
            &fake.agent_write,
            &pending_interaction_line(kind, "s-pending-reset"),
        )
        .await;
        let nonce = registered_interaction(&mut fake, &mut observed).await;
        write_line(
            &fake.agent_write,
            &update("s-pending-reset", "unanswered before refusal"),
        )
        .await;
        collect_connection_events_until(&mut fake, &mut observed, |event| {
            matches!(event, Event::AgentMessageChunk { text, .. } if text == "unanswered before refusal")
        }).await;
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
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(10), response).await.unwrap().unwrap(),
            ResetSessionOutcome::Failed { message } if message.contains("resolve it before clearing the conversation")
        ));
        assert!(fake.pending_responders.lock().await.contains_key(&nonce));
        write_line(
            &fake.agent_write,
            &update("s-pending-reset", "unanswered after refusal"),
        )
        .await;
        collect_connection_events_until(&mut fake, &mut observed, |event| {
            matches!(event, Event::AgentMessageChunk { text, .. } if text == "unanswered after refusal")
        }).await;
        assert!(
            !observed[initial_end..].iter().any(&boundary),
            "{kind}: {observed:?}"
        );
        assert!(fake.pending_responders.lock().await.contains_key(&nonce));
        assert!(!fake.received.lock().await.iter().any(|frame| {
            frame.get("method").is_none() && frame["id"] == "pending-interaction"
        }));
        resolve_registered_interaction(&fake, &nonce).await;
        let reply = tokio::time::timeout(Duration::from_secs(10), fake.responses.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply["id"], "pending-interaction");
        assert!(reply.get("error").is_none(), "{reply:?}");
        if kind == "permission" {
            assert_eq!(reply["result"]["outcome"]["outcome"], "selected");
            assert_eq!(reply["result"]["outcome"]["optionId"], "allow");
        } else {
            assert_eq!(reply["result"]["action"], "decline");
        }
        collect_connection_events_until(&mut fake, &mut observed, |event| match event {
            Event::ApprovalResolved {
                nonce: actual,
                decision: ApprovalDecision::Allow,
            } => kind == "permission" && actual == &nonce,
            Event::ElicitationResolved {
                nonce: actual,
                outcome: ElicitationOutcome::Declined,
                ..
            } => kind == "elicitation" && actual == &nonce,
            _ => false,
        })
        .await;
        let (done, flushed) = oneshot::channel();
        fake.cmd_tx
            .send(ClientCmd::FlushForTest(done))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), flushed)
            .await
            .unwrap()
            .unwrap();
        fake.cmd_tx
            .send(ClientCmd::Prompt(Vec::new().into()))
            .await
            .unwrap();
        collect_connection_events_until(
            &mut fake,
            &mut observed,
            |event| matches!(event, Event::Stopped { reason } if reason == "prompt_complete"),
        )
        .await;
        assert!(
            !observed[initial_end..].iter().any(&boundary),
            "{kind}: {observed:?}"
        );
        {
            let received = fake.received.lock().await;
            assert_eq!(
                received
                    .iter()
                    .filter(|frame| frame["method"] == "session/new")
                    .count(),
                1
            );
            assert!(received
                .iter()
                .filter(|frame| frame["method"] == "session/prompt")
                .map(|frame| frame["params"]["sessionId"].as_str().unwrap())
                .eq(["s-pending-reset"]));
        }
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
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(10), response).await.unwrap().unwrap(),
            ResetSessionOutcome::Reset { new_acp_session_id } if new_acp_session_id == "s-pending-reset-reset-1"
        ));
        collect_connection_events_until(
            &mut fake,
            &mut observed,
            |event| matches!(event, Event::Stopped { reason } if reason == "session_reset"),
        )
        .await;
        write_line(
            &fake.agent_write,
            &update("s-pending-reset", "stale after explicit reset"),
        )
        .await;
        write_line(
            &fake.agent_write,
            &update("s-pending-reset-reset-1", "fresh after explicit reset"),
        )
        .await;
        collect_connection_events_until(&mut fake, &mut observed, |event| {
            matches!(event, Event::AgentMessageChunk { text, .. } if text == "fresh after explicit reset")
        }).await;
        fake.cmd_tx
            .send(ClientCmd::Prompt(Vec::new().into()))
            .await
            .unwrap();
        collect_connection_events_until(
            &mut fake,
            &mut observed,
            |event| matches!(event, Event::Stopped { reason } if reason == "prompt_complete"),
        )
        .await;
        let stale = scope.state.acp_event_store.replay_from(&fake.event_rx.id, 0).unwrap()
            .into_iter().find(|stored| matches!(stored.event.as_ref(),
                Event::AgentMessageChunk { text, .. } if text == "stale after explicit reset"))
            .expect("the original's delayed text remains retained history");
        assert_eq!(stale.metadata.scope, crate::acp::event_sender::EventScope::HistoryOnly);
        assert!(stale.metadata.operation.is_none());
        assert!(stale.metadata.native_autonomous.is_none());
        assert!(!scope.state.session_service.fold_control_state(&fake.event_rx.id).await.unwrap().turn_active);
        assert!(
            observed[initial_end..]
                .iter()
                .filter_map(|event| match event {
                    Event::SessionCleared => Some(0),
                    Event::SessionContextReset { .. } => Some(1),
                    Event::AcpSessionAssigned { .. } => Some(2),
                    Event::Stopped { reason } if reason == "session_reset" => Some(3),
                    _ => None,
                })
                .eq([0, 1, 2, 3]),
            "{kind}: {observed:?}"
        );
        assert!(observed[initial_end..].iter().any(|event| {
            matches!(event, Event::AcpSessionAssigned { acp_session_id, connection_ready: true, .. } if acp_session_id == "s-pending-reset-reset-1")
        }));
        assert_eq!(
            observed[initial_end..]
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        Event::ApprovalRequested { .. } | Event::ElicitationRequested { .. }
                    )
                })
                .count(),
            1
        );
        {
            let received = fake.received.lock().await;
            assert_eq!(
                received
                    .iter()
                    .filter(|frame| frame["method"] == "session/new")
                    .count(),
                2
            );
            assert!(received
                .iter()
                .filter(|frame| frame["method"] == "session/prompt")
                .map(|frame| frame["params"]["sessionId"].as_str().unwrap())
                .eq(["s-pending-reset", "s-pending-reset-reset-1"]));
        }
        fake.event_rx.stop_and_forget().await;
        let _ = fake.connection.await;
        fake.agent.abort();
        let _ = fake.agent.await;
    }

    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn transport_eof_cancels_unanswered_callbacks_after_intervening_progress() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {

    use super::super::pending::PendingResolver;
    for kind in ["permission", "elicitation"] {
        let mut fake = spawn_fake_connection(scope, "s-pending-eof", None, false, move |_| {
            vec![pending_interaction_line(kind, "s-pending-eof")]
        })
        .await;
        fake.cmd_tx
            .send(ClientCmd::Prompt(Vec::new().into()))
            .await
            .unwrap();
        let nonce = registered_interaction(&mut fake, &mut Vec::new()).await;
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
    }

    })).await;
}

const MODE_OPTIONS: &str = r#"[{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"auto","options":[{"value":"auto","name":"Auto"},{"value":"default","name":"Manual"}]}]"#;

const GOOD_HELLO: &str = r#","_meta":{"aoe":{"proxy":{"plugin_id":"p","protocol_version":1,"primitive_schema_version":1}}}"#;

const BAD_HELLO: &str = r#","_meta":{"aoe":{"proxy":{"plugin_id":"p","protocol_version":9,"primitive_schema_version":1}}}"#;

async fn next_child_event(event_rx: &mut ManagedClient, child: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Event::SubagentUpdated { subagent, .. } =
                event_rx.recv().await.expect("connection remains open")
            {
                if subagent.session_id == child {
                    break;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{child} announced"));
}

fn tool_call_frame(id: &str, aoe: &str) -> String {
    let meta = if aoe.is_empty() {
        String::new()
    } else {
        format!(r#","_meta":{{"aoe":{aoe}}}"#)
    };
    format!(
        r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-h","update":{{"sessionUpdate":"tool_call","toolCallId":"{id}","title":"T"{meta}}}}}}}"#
    )
}

#[tokio::test]
#[serial_test::serial]
async fn idle_remote_control_message_records_and_returns_to_idle() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {

    let (ready, mut fake) = spawn_fake_connection_raw(
        scope, "s-remote", None, false,
        r#","_meta":{"ompAutonomousTurns":{"version":1}}"#,
        (None, None), |_| Vec::new(),
    ).await;
    ready.expect("the actual Native agent negotiates its own root lifecycle");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            fake.event_rx.recv().await.expect("connection remains open"),
            Event::AcpSessionAssigned { .. }
        ) {}
    })
    .await
    .expect("session commits");
    let run = uuid::Uuid::new_v4();
    write_line(&fake.agent_write, &serde_json::json!({
        "jsonrpc":"2.0","method":"_omp/autonomous_turn/start",
        "params":{"version":1,"sessionId":"s-remote","runId":run,"source":"irc"}
    }).to_string()).await;

    let update = |body: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-remote","update":{body}}}}}"#
        )
    };
    for body in [
        r#"{"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"from the app"}}"#,
        r#"{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"on it"}}"#,
        r#"{"sessionUpdate":"usage_update","used":10,"size":100,"cost":{"amount":0.1,"currency":"USD"}}"#,
    ] {
        write_line(&fake.agent_write, &update(body)).await;
    }
    write_line(&fake.agent_write, &serde_json::json!({
        "jsonrpc":"2.0","method":"_omp/autonomous_turn/end",
        "params":{"version":1,"sessionId":"s-remote","runId":run,"source":"irc","stopReason":"end_turn"}
    }).to_string()).await;

    // The same negotiated Native run, not an elapsed idle timer, owns its terminal.
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = fake.event_rx.recv().await {
            let stopped = matches!(event, Event::Stopped { .. });
            seen.push(event);
            if stopped {
                break;
            }
        }
    })
    .await
    .expect("the agent-initiated turn must reach a terminal Stopped");

    let user = seen.iter().position(|e| {
        matches!(e, Event::UserPromptSent { text, prompt_id: None, .. } if text == "from the app")
    });
    let reply = seen
        .iter()
        .position(|e| matches!(e, Event::AgentMessageChunk { .. }));
    let stopped = seen.iter().position(|e| matches!(e, Event::Stopped { .. }));
    assert!(
        user.is_some() && user < reply && reply < stopped,
        "user message, reply, then Stopped expected: {seen:?}"
    );
    fake.agent.abort();
    let _ = fake.agent.await;

    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn default_mode_reaches_the_live_mode_option_only_when_configured() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| {
        Box::pin(async move {
            // (default_mode, config values the agent received, mode the daemon last saw)
            let cases: [(Option<&'static str>, &[&str], Option<&str>); 2] = [
                (None, &[], None),
                (Some("default"), &["default"], Some("default")),
            ];
            for (default_mode, want_sets, want_mode) in cases {
                // The connection only dispatches a prompt once session setup, default
                // mode included, is done; `ready_rx` fires before `session/new`.
                let (prompted_tx, mut prompted_rx) = mpsc::unbounded_channel();
                let mut fake = spawn_fake_connection_with(
                    scope,
                    "s-mode",
                    None,
                    false,
                    default_mode,
                    Some(MODE_OPTIONS),
                    move |_| {
                        let _ = prompted_tx.send(());
                        Vec::new()
                    },
                )
                .await;
                fake.cmd_tx
                    .send(ClientCmd::Prompt(
                        vec![ContentBlock::Text(TextContent::new("hi"))].into(),
                    ))
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(10), prompted_rx.recv())
                    .await
                    .expect("session setup must finish and dispatch the prompt")
                    .expect("fake agent must receive the prompt");
                let mut last_mode = None;
                while let Ok(event) = fake.event_rx.try_recv() {
                    if let Event::ConfigOptionsUpdated { options } = event {
                        last_mode = options
                            .iter()
                            .find(|o| o.id == "mode")
                            .map(|o| o.current_value.clone());
                    }
                }
                assert_eq!(
                    *fake.set_options.lock().await,
                    want_sets,
                    "{default_mode:?}"
                );
                if let Some(want) = want_mode {
                    assert_eq!(last_mode.as_deref(), Some(want), "{default_mode:?}");
                }
                fake.agent.abort();
                let _ = fake.agent.await;
            }
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn adapter_started_idle_compaction_ends_without_a_prompt() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {

    let prompted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let seen_prompt = prompted.clone();
    let (ready, mut fake) = spawn_fake_connection_raw(
        scope, "s-idle", None, false,
        r#","_meta":{"ompAutonomousTurns":{"version":1}}"#,
        (None, None), move |id| {
            seen_prompt.store(true, std::sync::atomic::Ordering::SeqCst);
            vec![serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}).to_string()]
        },
    ).await;
    ready.expect("the actual Native agent negotiates its own root lifecycle");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(fake.event_rx.recv().await.expect("connection remains open"),
            Event::AcpSessionAssigned { connection_ready: true, .. }) {}
    }).await.expect("the observed original commits before its own compaction");
    let run = uuid::Uuid::new_v4();
    write_line(&fake.agent_write, &serde_json::json!({
        "jsonrpc":"2.0","method":"_omp/autonomous_turn/start",
        "params":{"version":1,"sessionId":"s-idle","runId":run,"source":"scheduled_continuation"}
    }).to_string()).await;

    for update in [
        r#"{"sessionUpdate":"compaction_update","compactionId":"c","status":"in_progress"}"#,
        r#"{"sessionUpdate":"compaction_update","compactionId":"c","status":"completed"}"#,
        r#"{"sessionUpdate":"usage_update","used":10,"size":100,"cost":{"amount":0.1,"currency":"USD"}}"#,
    ] {
        write_line(
            &fake.agent_write,
            &format!(
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s-idle","update":{update}}}}}"#
            ),
        )
        .await;
    }
    write_line(&fake.agent_write, &serde_json::json!({
        "jsonrpc":"2.0","method":"_omp/autonomous_turn/end",
        "params":{"version":1,"sessionId":"s-idle","runId":run,"source":"scheduled_continuation","stopReason":"end_turn"}
    }).to_string()).await;

    // Compaction's real own run supplies the terminal; no aoe prompt is required.
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = fake.event_rx.recv().await {
            let stopped = matches!(event, Event::Stopped { .. });
            seen.push(event);
            if stopped {
                break;
            }
        }
    })
    .await
    .expect("the compaction turn must reach a terminal event");

    let started = seen
        .iter()
        .position(|e| matches!(e, Event::ConversationCompactionStarted));
    let completed = seen
        .iter()
        .position(|e| matches!(e, Event::ConversationCompacted));
    let stopped = seen
        .iter()
        .position(|e| matches!(e, Event::Stopped { reason } if reason == "agent_idle"));
    assert!(
        matches!((started, completed, stopped), (Some(a), Some(b), Some(c)) if a < b && b < c),
        "compaction start, completion, then Stopped(agent_idle) expected: {seen:?}"
    );
    assert!(
        !prompted.load(std::sync::atomic::Ordering::SeqCst),
        "no aoe prompt may be in flight"
    );
    fake.cmd_tx.send(ClientCmd::Cancel).await.unwrap();
    fake.cmd_tx
        .send(ClientCmd::Prompt(PromptInput::default()))
        .await
        .unwrap();
    let mut next = collect_terminal(&mut fake).await;
    assert!(matches!(next.last(), Some(Event::Stopped { reason }) if reason == "prompt_complete"));
    fake.cmd_tx.send(ClientCmd::Shutdown).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), fake.connection)
        .await
        .unwrap()
        .unwrap();
    while let Ok(event) = fake.event_rx.try_recv() {
        next.push(event);
    }
    assert!(prompted.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(
        next.iter()
            .filter(|event| matches!(event, Event::Stopped { .. }))
            .count(),
        1
    );
    assert!(!next
        .iter()
        .any(|event| matches!(event, Event::AcpSessionAssigned { .. })));
    assert!(!next.iter().any(|event| matches!(
        event,
        Event::SessionContextReset { .. } | Event::CancelRequested { .. }
    )));
    fake.agent.abort();
    let _ = fake.agent.await;

    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn child_cancel_targets_the_child_and_requires_the_capability() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {

    let (daemon_write, agent_read) = tokio::io::duplex(1024 * 1024);
    let (agent_write, daemon_read) = tokio::io::duplex(1024 * 1024);
    let agent_write: SharedWrite = Arc::new(Mutex::new(agent_write));
    let cancels = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let (prompted, mut native_prompts) = mpsc::unbounded_channel();

    let agent = scope.spawn_helper({
        let (agent_write, cancels) = (agent_write.clone(), cancels.clone());
        async move {
            let mut reader = BufReader::new(agent_read);
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                let id = &msg["id"];
                match msg.get("method").and_then(|m| m.as_str()) {
                    Some("initialize") => {
                        write_line(&agent_write, &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1,"agentCapabilities":{{}}}}}}"#
                        )).await;
                    }
                    Some("session/new") => {
                        write_line(
                            &agent_write,
                            &format!(
                                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"sessionId":"root"}}}}"#
                            ),
                        )
                        .await;
                        for (child, caps) in
                            [("c-yes", r#","capabilities":{"cancel":{}}"#), ("c-no", "")]
                        {
                            write_line(&agent_write, &format!(
                                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"root","update":{{"sessionUpdate":"subagent_update","sessionId":"{child}","state":{{"state":"running"}}{caps}}}}}}}"#
                            )).await;
                        }
                    }
                    Some("session/prompt") => { let _ = prompted.send(msg["id"].clone()); }
                    Some("session/cancel") => cancels
                        .lock()
                        .unwrap()
                        .push(msg["params"]["sessionId"].as_str().unwrap().to_string()),
                    _ => {}
                }
            }
        }
    });

    let temp = scope.tempdir();
    let config = scope.bridge_config(temp.path().into(), daemon_write, daemon_read);
    let mut event_rx = scope.spawn(config, "child-cancel-native-wire").await.unwrap();
    event_rx.ready().await;
    let actual = event_rx.actual();
    let cmd_tx = actual.test_command_sender();
    let connection = actual.observe_sdk_task();

    // Both announcements are observed before any cancel is sent.
    let mut announced = 0;
    tokio::time::timeout(Duration::from_secs(30), async {
        while announced < 2 {
            if let Some(Event::SubagentUpdated { .. }) = event_rx.recv().await {
                announced += 1;
            }
        }
    })
    .await
    .expect("both subagents announced");
    cmd_tx.send(ClientCmd::Prompt(vec![ContentBlock::Text(TextContent::new("hold the actual root turn"))].into()))
        .await.unwrap();
    let native_prompt = tokio::time::timeout(Duration::from_secs(30), native_prompts.recv())
        .await.unwrap().expect("actual native root prompt");
    assert!(native_prompt.as_i64().is_some(), "the held root has its own real Native request ID");

    let mut sent = Vec::new();
    for child in ["c-yes", "c-no", "unknown"] {
        let (respond_to, answer) = oneshot::channel();
        cmd_tx
            .send(ClientCmd::CancelChild {
                session_id: child.to_string(),
                respond_to,
            })
            .await
            .unwrap();
        sent.push(answer.await.unwrap());
    }
    assert_eq!(sent, [true, false, false]);

    // The root cancel is a sentinel: the agent reads in order, so everything
    // sent before it has been recorded once it shows up.
    cmd_tx.send(ClientCmd::Cancel).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        while !cancels.lock().unwrap().contains(&"root".to_string()) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("root cancel reaches the agent");
    assert_eq!(*cancels.lock().unwrap(), ["c-yes", "root"]);

    event_rx.stop_and_forget().await;
    agent.abort();
    let _ = tokio::join!(agent, connection);

    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn reset_waits_for_a_running_subagent() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {

    let (daemon_write, agent_read) = tokio::io::duplex(1024 * 1024);
    let (agent_write, daemon_read) = tokio::io::duplex(1024 * 1024);
    let agent_write: SharedWrite = Arc::new(Mutex::new(agent_write));
    let new_sessions = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let agent = scope.spawn_helper({
        let (agent_write, new_sessions) = (agent_write.clone(), new_sessions.clone());
        async move {
            let mut reader = BufReader::new(agent_read);
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                    continue;
                };
                let id = &msg["id"];
                match msg.get("method").and_then(|m| m.as_str()) {
                    Some("initialize") => {
                        write_line(&agent_write, &format!(
                            r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1,"agentCapabilities":{{}}}}}}"#
                        )).await;
                    }
                    Some("session/new") => {
                        let n = new_sessions.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        write_line(
                            &agent_write,
                            &format!(
                                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"sessionId":"root-{n}"}}}}"#
                            ),
                        )
                        .await;
                        if n == 1 {
                            write_line(&agent_write, r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"root-1","update":{"sessionUpdate":"subagent_update","sessionId":"child","state":{"state":"running"}}}}"#).await;
                        }
                    }
                    Some("session/prompt") => {
                        write_line(&agent_write, r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"root-1","update":{"sessionUpdate":"subagent_update","sessionId":"child","state":{"state":"idle","stopReason":"end_turn"}}}}"#).await;
                        write_line(
                            &agent_write,
                            &format!(
                                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"stopReason":"end_turn"}}}}"#
                            ),
                        )
                        .await;
                    }
                    _ => {}
                }
            }
        }
    });

    let temp = scope.tempdir();
    let config = scope.bridge_config(temp.path().into(), daemon_write, daemon_read);
    let mut event_rx = scope.spawn(config, "child-reset-native-wire").await.unwrap();
    event_rx.ready().await;
    let actual = event_rx.actual();
    let cmd_tx = actual.test_command_sender();
    let connection = actual.observe_sdk_task();
    next_child_event(&mut event_rx, "child").await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let proposal = event_rx.prepare_reset(deadline).await;
    let (respond_to, answer) = oneshot::channel();
    cmd_tx.send(ClientCmd::ResetSession {
        text: "/clear".into(), deadline, proposal, respond_to,
    }).await.unwrap();

    match answer.await.unwrap() {
        ResetSessionOutcome::Failed { message } => {
            assert!(message.contains("subagent is still running"), "{message}");
        }
        other => panic!("reset must be refused: {other:?}"),
    }
    assert_eq!(new_sessions.load(std::sync::atomic::Ordering::SeqCst), 1);

    // The agent reports the child idle before it ends the turn.
    cmd_tx
        .send(ClientCmd::Prompt(PromptInput::from(vec![
            ContentBlock::Text(TextContent::new("go")),
        ])))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        let (mut idle, mut stopped) = (false, false);
        while !(idle && stopped) {
            match event_rx.recv().await.expect("connection remains open") {
                Event::SubagentUpdated { subagent, .. } => {
                    idle |= subagent.state == SubagentState::Idle
                }
                Event::Stopped { .. } => stopped = true,
                _ => {}
            }
        }
    })
    .await
    .expect("child idle and turn stopped");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let proposal = event_rx.prepare_reset(deadline).await;
    let (respond_to, answer) = oneshot::channel();
    cmd_tx.send(ClientCmd::ResetSession {
        text: "/clear".into(), deadline, proposal, respond_to,
    }).await.unwrap();

    assert!(matches!(
        answer.await.unwrap(),
        ResetSessionOutcome::Reset { .. }
    ));
    assert_eq!(new_sessions.load(std::sync::atomic::Ordering::SeqCst), 2);

    event_rx.stop_and_forget().await;
    agent.abort();
    let _ = tokio::join!(agent, connection);

    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn child_approval_wait_does_not_stall_other_sessions() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {

    let FakeConnection {
        mut event_rx,
        agent_write,
        cmd_tx: _cmd_tx,
        mut responses,
        connection,
        agent,
        ..
    } = spawn_fake_connection(scope, "root", None, false, |_| Vec::new()).await;
    let announce = |child: &str| {
        format!(
            r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"root","update":{{"sessionUpdate":"subagent_update","sessionId":"{child}","state":{{"state":"running"}}}}}}}}"#
        )
    };
    write_line(&agent_write, &announce("c1")).await;
    next_child_event(&mut event_rx, "c1").await;
    write_line(
        &agent_write,
        r#"{"jsonrpc":"2.0","id":900,"method":"session/request_permission","params":{"sessionId":"c1","toolCall":{"toolCallId":"t1"},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]}}"#,
    )
    .await;
    let nonce = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Event::ApprovalRequested { approval } =
                event_rx.recv().await.expect("connection remains open")
            {
                break approval.nonce;
            }
        }
    })
    .await
    .expect("approval requested");

    // The approval is still unanswered, yet a sibling announcement is handled.
    write_line(&agent_write, &announce("c2")).await;
    next_child_event(&mut event_rx, "c2").await;

    use axum::response::IntoResponse;
    let resolution = crate::server::api::resolve_approval(
        axum::extract::State(event_rx.app_state()),
        axum::extract::Path((event_rx.id.clone(), nonce.0.clone())),
        Ok(axum::Json(crate::acp::protocol::ResolveApprovalRequest {
            decision: crate::acp::protocol::ApprovalDecisionWire::Allow,
            option_id: None,
        })),
    ).await.into_response();
    assert_eq!(resolution.status(), axum::http::StatusCode::NO_CONTENT);
    let response = tokio::time::timeout(Duration::from_secs(30), responses.recv())
        .await
        .expect("response reaches the adapter")
        .expect("agent still reading");
    assert_eq!(response["id"], 900);
    assert_eq!(response["result"]["outcome"]["outcome"], "selected");
    assert_eq!(response["result"]["outcome"]["optionId"], "allow");

    event_rx.stop_and_forget().await;
    agent.abort();
    let _ = tokio::join!(agent, connection);

    })).await;
}

struct DirectConnection {
    events: mpsc::Receiver<crate::acp::event_sender::SdkEvent>,
    connection: tokio::task::JoinHandle<()>,
    agent: tokio::task::JoinHandle<()>,
    _commands: mpsc::Sender<super::super::commands::QueuedClientCmd>,
    _cwd: tempfile::TempDir,
}

impl DirectConnection {
    async fn next_event(&mut self) -> Option<Event> {
        use crate::acp::event_sender::SdkEvent;
        loop {
            match self.events.recv().await? {
                SdkEvent::Data(data) => return Some(Arc::unwrap_or_clone(data.event)),
                SdkEvent::ControlPlaneLoss(loss) => {
                    return Some(Event::AcpConnectionUnavailable {
                        source: loss.observation.source,
                        cause: loss.observation.cause,
                        phase: loss.observation.phase,
                        native_subject: None,
                    });
                }
                SdkEvent::EstablishmentClosed { .. } => {}
            }
        }
    }
}

async fn spawn_direct_connection(
    session_id: &'static str,
    init_extra: &'static str,
    proxy: Option<ProxySpec>,
    close_after_init: bool,
) -> (Result<(), AcpError>, DirectConnection) {
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    let (daemon_write, agent_read) = tokio::io::duplex(64 * 1024);
    let (agent_write, daemon_read) = tokio::io::duplex(64 * 1024);
    let agent_write = Arc::new(Mutex::new(agent_write));
    let replies = Arc::clone(&agent_write);
    let agent = tokio::spawn(async move {
        let mut lines = BufReader::new(agent_read).lines();
        while let Some(line) = lines.next_line().await.unwrap() {
            let message: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id = &message["id"];
            let response = match message["method"].as_str().unwrap() {
                "initialize" => format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1,"agentCapabilities":{{}}{init_extra}}}}}"#
                ),
                "session/new" => format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"sessionId":"{session_id}"}}}}"#
                ),
                method => panic!("unexpected direct fixture method: {method}"),
            };
            write_line(&replies, &response).await;
        }
    });
    let cwd = tempfile::tempdir().unwrap();
    let resources = SessionResources::new(
        cwd.path().into(),
        vec![],
        session_id.into(),
        None,
        &SessionState::default(),
    );
    let connection_state = Arc::new(ConnectionState::default());
    let close_gate = close_after_init.then(|| {
        let (entered, initialized) = oneshot::channel();
        let (allow, release) = oneshot::channel();
        *connection_state
            .test_initialize_response_application
            .lock()
            .unwrap() = Some((entered, release));
        (initialized, allow)
    });
    let (event_tx, events) = mpsc::channel(64);
    let (commands, cmd_rx) = mpsc::channel(1);
    let (ready_tx, ready_rx) = oneshot::channel();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let params = ConnectionParams {
        event_tx: event_tx.into(),
        connection_state: Arc::clone(&connection_state),
        cmd_rx,
        child: None,
        pending_responders: Arc::default(),
        resources,
        mode: ConnectMode::Fresh,
        ready_tx,
        profile: &crate::acp::agent_profiles::GEMINI,
        expected_agent: ExpectedAgent::Gemini,
        proxy,
        source_profile: None,
        default_effort: None,
        default_model_config: Default::default(),
        default_mode: None,
        default_model: None,
        mcp_servers: Vec::new(),
        claude_home: None,
        runner: None,
        cancellation: tokio_util::sync::CancellationToken::new(),
        shutdown: shutdown.clone(),
        tracking_observer: None,
    };
    let transport = ByteStreams::new(daemon_write.compat_write(), daemon_read.compat());
    let connection = tokio::spawn(run_connection_task(transport, params));
    if let Some((initialized, allow)) = close_gate {
        tokio::time::timeout(Duration::from_secs(5), async {
            initialized.await.expect("proxy hello validated before EOF");
            agent_write.lock().await.shutdown().await.unwrap();
            connection_state.wait_unavailable().await;
            let _ = allow.send(());
        })
        .await
        .expect("validated proxy reaches the closed transport");
    }
    let ready = ready_rx.await.expect("initialize handoff resolves");
    (
        ready,
        DirectConnection {
            events,
            connection,
            agent,
            _commands: commands,
            _cwd: cwd,
        },
    )
}

const HELLO_WITH_INFO: &str = r#","agentInfo":{"name":"claude-x","version":"1.2.0"},"_meta":{"aoe":{"proxy":{"plugin_id":"p","protocol_version":1,"primitive_schema_version":1}}}"#;

fn proxy_spec(compat: ProxyCompat) -> ProxySpec {
    ProxySpec {
        plugin_id: "p".into(),
        command: "proxy".into(),
        args: vec![],
        failure_policy: ProxyFailurePolicy::Refuse,
        display_name: "P".into(),
        compat,
    }
}

/// A launched proxy must send a hello naming itself, in front of an adapter
/// its compat accepts; each breach is an `InvalidProxyHello`. Without a launched
/// proxy a hello is accepted as announced.
#[tokio::test]
async fn launched_proxy_hello_is_held_to_its_spec() {
    let other_plugin = r#","_meta":{"aoe":{"proxy":{"plugin_id":"q","protocol_version":1,"primitive_schema_version":1}}}"#;
    let compat = |names: &[&str], req: Option<&str>| ProxyCompat {
        agent_names: names.iter().map(|n| n.to_string()).collect(),
        version_req: req.map(|r| r.parse().unwrap()),
    };
    let cases: [(&str, &str, Option<ProxySpec>, Option<&str>); 8] = [
        (
            "no hello",
            "",
            Some(proxy_spec(ProxyCompat::default())),
            Some("sent no hello"),
        ),
        (
            "other plugin",
            other_plugin,
            Some(proxy_spec(ProxyCompat::default())),
            Some("names plugin `q`"),
        ),
        (
            "any adapter",
            GOOD_HELLO,
            Some(proxy_spec(ProxyCompat::default())),
            None,
        ),
        (
            "compat met",
            HELLO_WITH_INFO,
            Some(proxy_spec(compat(&["claude-x"], Some(">=1.2")))),
            None,
        ),
        (
            "wrong name",
            HELLO_WITH_INFO,
            Some(proxy_spec(compat(&["other"], None))),
            Some("`claude-x` is not supported"),
        ),
        (
            "version too old",
            HELLO_WITH_INFO,
            Some(proxy_spec(compat(&[], Some(">=2")))),
            Some("does not satisfy"),
        ),
        (
            "no agent_info",
            GOOD_HELLO,
            Some(proxy_spec(compat(&["claude-x"], None))),
            Some("no agent_info"),
        ),
        ("hello without a launched proxy", GOOD_HELLO, None, None),
    ];
    for (name, init_extra, proxy, refusal) in cases {
        let (ready, fake) = spawn_direct_connection("s-v", init_extra, proxy, false).await;
        match refusal {
            None => assert!(ready.is_ok(), "{name}: {ready:?}"),
            Some(needle) => assert!(
                matches!(&ready, Err(AcpError::IncompatibleAgent(e))
                    if matches!(&e.detail, StartupErrorDetail::InvalidProxyHello { reason } if reason.contains(needle))),
                "{name}: {ready:?}"
            ),
        }
        fake.agent.abort();
        fake.connection.abort();
    }
}

/// A hello core cannot accept refuses the session; a valid one makes the
/// agent's `_meta.aoe` frames dedup, drop and reorder before the reducer, and a
/// broken frame ends the integration.
#[tokio::test]
#[serial_test::serial]
async fn proxy_hello_and_primitives_shape_the_event_stream() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| {
        Box::pin(async move {
            let directory = scope.tempdir();
            let initialize: serde_json::Value = serde_json::from_str(&format!(
                r#"{{"protocolVersion":1,"agentCapabilities":{{}}{BAD_HELLO}}}"#
            ))
            .unwrap();
            let (config, native) = scope.native_peer(
                directory.path().into(),
                "gemini",
                "gemini",
                move |request| {
                    assert_eq!(
                        request["method"], "initialize",
                        "a rejected hello cannot issue Native New"
                    );
                    Some(
                        serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":initialize}),
                    )
                },
            );
            let (id, mut publications) = scope.create_observed(config, "incompatible-proxy").await;
            let detail = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let frame = publications
                        .recv()
                        .await
                        .expect("actual failed constructor publication");
                    if frame.session_id != id {
                        continue;
                    }
                    match frame.event.as_ref() {
                        Event::IncompatibleAgent { detail } => break detail.clone(),
                        Event::AcpSessionAssigned { .. } => {
                            panic!("an incompatible proxy cannot establish a Native session")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("the actual failed constructor reports its typed incompatibility");
            assert!(matches!(
                detail,
                StartupErrorDetail::InvalidProxyHello { .. }
            ));
            assert_eq!(native.requests(), ["initialize"]);
            // Keep the row until this scope's captured retirement proves its cleanup.

            let (ready, mut fake) = spawn_fake_connection_raw(
                scope,
                "s-h",
                None,
                false,
                GOOD_HELLO,
                (None, None),
                |_| vec![],
            )
            .await;
            ready.expect("valid hello accepted");
            let frames = [
                tool_call_frame("t1", ""),
                // Tailer-emitted: waits for adapter frame 3.
                tool_call_frame("t2", r#"{"v":1,"key":"k2","after_adapter_sequence":3}"#),
                tool_call_frame("t3", r#"{"v":1,"key":"k3","ephemeral":true}"#),
                tool_call_frame("t4", r#"{"v":1,"key":"k4"}"#),
                tool_call_frame("t4-again", r#"{"v":1,"key":"k4"}"#),
                tool_call_frame("t5", ""),
                tool_call_frame("t6", r#"{"v":1}"#),
            ];
            for frame in &frames {
                write_line(&fake.agent_write, frame).await;
            }
            let (mut started, mut lost) = (Vec::new(), None);
            tokio::time::timeout(Duration::from_secs(10), async {
                while lost.is_none() {
                    match fake.event_rx.recv().await.expect("connection remains open") {
                        Event::ToolCallStarted { tool_call } => started.push(tool_call.id),
                        Event::IntegrationLost { reason } => lost = Some(reason),
                        _ => {}
                    }
                }
            })
            .await
            .expect("integration loss is reported");
            assert_eq!(started, ["t1", "t4", "t2", "t5"]);
            assert!(matches!(
                lost,
                Some(IntegrationLostReason::ContractViolation { .. })
            ));
            fake.event_rx
                .shutdown()
                .await
                .expect("retire the exact primitive fixture");
            fake.agent.abort();
        })
    })
    .await;
}

/// Native lifecycle owns its terminal; ambient quota hints cannot replace it.
/// A matching failed Native prompt still keeps the exact rejected window.
#[tokio::test]
#[serial_test::serial]
async fn rate_limit_hints_do_not_override_an_owned_autonomous_terminal() {
    use crate::acp::event_store::ControlKnowledge::Known;

    crate::acp::supervisor::test_support::managed::with_managed(|scope| {
        Box::pin(async move {
            let resets_at = crate::time::Timestamp::now().as_unix_secs() + 3600;
            let report = |window: &str, reset: i64| serde_json::json!({
                "sessionUpdate":"usage_update","used":10,"size":100,
                "cost":{"amount":0.1,"currency":"USD"},
                "_meta":{"_claude/rateLimit":{
                    "status":"rejected","rateLimitType":window,"resetsAt":reset
                }}
            });
            let frame = |update: serde_json::Value| serde_json::json!({
                "jsonrpc":"2.0","method":"session/update",
                "params":{"sessionId":"s-autonomous-quota","update":update}
            });
            let owned_report = frame(report("five_hour", resets_at)).to_string();
            let first_prompt = std::cell::Cell::new(true);
            let (ready, mut fake) = spawn_fake_connection_raw(
                scope,
                "s-autonomous-quota",
                None,
                false,
                r#","_meta":{"ompAutonomousTurns":{"version":1}}"#,
                (None, None),
                move |id| {
                    if first_prompt.replace(false) {
                        return vec![serde_json::json!({
                            "jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}
                        }).to_string()];
                    }
                    vec![
                        owned_report.clone(),
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-32603,"message":"limit reached","data":{"errorKind":"rate_limit"}
                        }}).to_string(),
                    ]
                },
            )
            .await;
            ready.expect("the Native peer negotiates its own lifecycle");

            // Establish known absence of a park with an actual main result.
            // An autonomous terminal cannot initialize missing main knowledge.
            fake.cmd_tx.send(ClientCmd::Prompt(PromptInput::default())).await.unwrap();
            let baseline = collect_terminal(&mut fake).await;
            assert!(matches!(baseline.last(), Some(Event::Stopped { reason }) if reason == "prompt_complete"));
            assert_native_prompt_succeeded(scope, &fake);
            let baseline_prompt_id = fake.native_prompt_id.load(Ordering::Acquire);
            let store = &scope.state.acp_event_store;
            assert!(matches!(store.rate_limit_park(&fake.event_rx.id).unwrap(), Known(None)));

            let run = uuid::Uuid::new_v4();
            let lifecycle = |method: &str, stop: Option<&str>| {
                let mut params = serde_json::json!({
                    "version":1,"sessionId":"s-autonomous-quota","runId":run,"source":"irc"
                });
                if let Some(reason) = stop {
                    params["stopReason"] = reason.into();
                }
                serde_json::json!({"jsonrpc":"2.0","method":method,"params":params})
            };
            for message in [
                lifecycle("_omp/autonomous_turn/start", None),
                frame(serde_json::json!({"sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":"You've hit your 5-hour limit · resets 10pm (America/New_York)"}})),
                frame(report("seven_day", resets_at + 7200)),
                lifecycle("_omp/autonomous_turn/end", Some("end_turn")),
            ] {
                write_line(&fake.agent_write, &message.to_string()).await;
            }
            let seen = collect_terminal(&mut fake).await;
            assert!(matches!(seen.last(), Some(Event::Stopped { reason }) if reason == "agent_idle"));
            assert!(seen.iter().any(|event| matches!(event, Event::UsageUpdated { .. })));
            assert!(!seen.iter().any(|event| matches!(event, Event::RateLimit { .. })));
            assert!(matches!(store.rate_limit_park(&fake.event_rx.id).unwrap(), Known(None)));
            assert_eq!(fake.native_prompt_id.load(Ordering::Acquire), baseline_prompt_id);

            fake.cmd_tx.send(ClientCmd::Prompt(PromptInput::default())).await.unwrap();
            let seen = collect_terminal(&mut fake).await;
            assert!(matches!(seen.last(), Some(Event::Stopped { reason }) if reason == "rate_limited"));
            assert_eq!(
                seen.iter().filter_map(|event| match event {
                    Event::RateLimit { info } => Some((info.kind.as_str(), info.resets_at.map(|t| t.as_unix_secs()))),
                    _ => None,
                }).collect::<Vec<_>>(),
                [("five_hour", Some(resets_at))],
                "the rejected window belongs to the failed prompt, not the earlier hint"
            );
            assert!(matches!(store.rate_limit_park(&fake.event_rx.id).unwrap(), Known(Some(_))));
            fake.event_rx.shutdown().await.expect("retire the exact owner");
            fake.agent.abort();
            let _ = fake.agent.await;
        })
    })
    .await;
}

/// A proxy that exits after a valid hello but before the connection is ready
/// is a lost integration; reporting it as a spawn failure would make the
/// supervisor relaunch the session without the proxy.
#[tokio::test]
async fn proxy_lost_between_hello_and_ready_is_integration_lost() {
    let (ready, mut fake) = spawn_direct_connection("s-w", GOOD_HELLO, None, true).await;
    ready.expect("a proxy that passed its hello is not a launch failure");
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = fake.next_event().await {
            events.push(event);
        }
    })
    .await
    .expect("connection ends once the agent closes");
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::IntegrationLost {
                reason: IntegrationLostReason::ProxyExited
            }
        )),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            Event::AcpConnectionUnavailable { .. } | Event::AgentStartupError { .. }
        )),
        "{events:?}"
    );
    fake.connection.await.unwrap();
    fake.agent.abort();
}

/// Native EOF always publishes its denial; only a verified proxy also loses
/// its integration. A close the daemon requested emits neither.
#[tokio::test]
#[serial_test::serial]
async fn closing_the_transport_after_a_hello_loses_the_integration() {
    crate::acp::supervisor::test_support::managed::with_managed(|scope| {
        Box::pin(async move {
            for (sid, hello, deliberate) in [
                ("s-proxied", GOOD_HELLO, false),
                ("s-unproxied", "", false),
                ("s-deliberate", GOOD_HELLO, true),
            ] {
                let (ready, mut fake) =
                    spawn_fake_connection_raw(scope, sid, None, false, hello, (None, None), |_| {
                        Vec::new()
                    })
                    .await;
                ready.expect("the Native original is ready");
                assert_eq!(fake.event_rx.verified_proxy(), !hello.is_empty());
                let store = &scope.state.acp_event_store;
                let id = &fake.event_rx.id;
                let before = store.highest_seq(id).unwrap();
                if deliberate {
                    fake.event_rx.actual().shutdown().await.unwrap();
                }
                fake.agent_write.lock().await.shutdown().await.unwrap();
                let proxy_exited = !hello.is_empty() && !deliberate;
                if !deliberate {
                    tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            let event =
                                fake.event_rx.recv().await.expect("owned loss is published");
                            let received = if proxy_exited {
                                matches!(
                                    event,
                                    Event::IntegrationLost {
                                        reason: IntegrationLostReason::ProxyExited
                                    }
                                )
                            } else {
                                matches!(
                                    event,
                                    Event::AcpConnectionUnavailable {
                                        source: crate::acp::state::AcpConnectionSource::Native,
                                        ..
                                    }
                                )
                            };
                            if received {
                                break;
                            }
                        }
                    })
                    .await
                    .expect("Native EOF reaches the managed drain");
                }
                fake.event_rx
                    .shutdown()
                    .await
                    .expect("retire the exact owner");
                let events = store.replay_from(&fake.event_rx.id, before).unwrap();
                let lost = events.iter().position(|row| {
                    matches!(
                        row.event.as_ref(),
                        Event::IntegrationLost {
                            reason: IntegrationLostReason::ProxyExited
                        }
                    )
                });
                let unavailable = events.iter().position(|row| {
                    matches!(
                        row.event.as_ref(),
                        Event::AcpConnectionUnavailable {
                            source: crate::acp::state::AcpConnectionSource::Native,
                            ..
                        }
                    )
                });
                assert_eq!(unavailable.is_some(), !deliberate, "{sid}");
                assert_eq!(lost.is_some(), proxy_exited, "{sid}");
                if proxy_exited {
                    assert!(unavailable.unwrap() < lost.unwrap());
                }
                assert!(
                    !events
                        .iter()
                        .any(|row| matches!(row.event.as_ref(), Event::AgentStartupError { .. })),
                    "{sid}"
                );
                fake.agent.abort();
                let _ = fake.agent.await;
            }
        })
    })
    .await;
}
