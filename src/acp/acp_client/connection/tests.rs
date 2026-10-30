//! The connection task driven against a fake agent over in-memory pipes.

use super::prompt::{SelectProbe, SELECT_PROBE};
use super::*;
use crate::acp::approvals::Nonce;
use crate::acp::fs_handler::FsPolicy;
use crate::acp::terminal_handler::TerminalManager;
use agent_client_protocol::schema::v1::{ContentBlock, TextContent};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

type SharedWrite = Arc<Mutex<tokio::io::DuplexStream>>;

async fn write_line(w: &SharedWrite, line: &str) {
    let mut guard = w.lock().await;
    guard.write_all(line.as_bytes()).await.unwrap();
    guard.write_all(b"\n").await.unwrap();
    guard.flush().await.unwrap();
}

/// Fairness of the in-flight-prompt select: Cancel must not sit behind the
/// lifecycle arm, which a sustained update stream keeps permanently ready.
/// A fake agent floods updates until `session/cancel` arrives.
#[tokio::test]
async fn cancel_reaches_the_agent_while_notifications_remain_queued() {
    // Repeated contested polls make unbiased selection observable; this is
    // probabilistic mutation coverage, not a guarantee about Tokio's RNG.
    for _ in 0..32 {
        tokio::join!(cancel_under_flood(), cancel_under_flood());
    }
}

/// Fake agent: handshake, then flood updates while the turn runs; end the
/// turn only on `session/cancel`.
async fn fake_agent(
    agent_read: tokio::io::DuplexStream,
    agent_write: SharedWrite,
    flooded: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
) {
    let mut reader = BufReader::new(agent_read);
    let mut line = String::new();
    let mut prompt_id: Option<serde_json::Value> = None;
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
                flooded.store(true, Ordering::SeqCst);
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

async fn cancel_under_flood() {
    let (daemon_write, agent_read) = tokio::io::duplex(1024 * 1024);
    let (agent_write, daemon_read) = tokio::io::duplex(1024 * 1024);
    let agent_write: SharedWrite = Arc::new(Mutex::new(agent_write));
    let cancelled = Arc::new(AtomicBool::new(false));
    let flooded = Arc::new(AtomicBool::new(false));

    let (event_tx, mut event_rx) = mpsc::channel(64);
    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(16);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), AcpError>>();
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().to_path_buf();
    let transport = ByteStreams::new(daemon_write.compat_write(), daemon_read.compat());
    let resources = SessionResources {
        fs_policy: Arc::new(FsPolicy::new(vec![cwd.clone()])),
        terminals: TerminalManager::new(),
        cwd,
        label: "s-fair".to_string(),
        sandbox: None,
    };
    let (paused_tx, mut paused_rx) = oneshot::channel();
    let (resume_tx, resume_rx) = oneshot::channel();
    let (winner_tx, winner_rx) = oneshot::channel();
    let probe = std::cell::RefCell::new(SelectProbe {
        gate: Some((paused_tx, resume_rx)),
        armed: false,
        winner: Some(winner_tx),
    });
    let params = ConnectionParams {
        connection_state: Arc::default(),
        claude_home: None,
        shutdown: tokio_util::sync::CancellationToken::new(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        event_tx,
        cmd_rx,
        child: None,
        pending_responders: Arc::new(Mutex::new(HashMap::new())),
        resources,
        mode: ConnectMode::Fresh {
            stored_acp_session_id: None,
            seed_history_replay: false,
            prior_conversation: false,
            fork_from: None,
        },
        ready_tx,
        profile: &crate::acp::agent_profiles::GEMINI,
        expected_agent: ExpectedAgent::Gemini,
        source_profile: None,
        default_effort: None,
        default_mode: None,
        default_model: None,
        mcp_servers: Vec::new(),
        runner: None,
    };
    let connection =
        tokio::spawn(SELECT_PROBE.scope(probe, run_connection_task(transport, params)));
    let agent = tokio::spawn(fake_agent(
        agent_read,
        agent_write.clone(),
        flooded.clone(),
        cancelled.clone(),
    ));
    // The flood runs beside the reader so the agent keeps reading cancel.
    let flood_stop = cancelled.clone();
    let flood = tokio::spawn(async move {
        while !flooded.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let update = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s-fair","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"flood"}}}}"#;
        while !flood_stop.load(Ordering::SeqCst) {
            write_line(&agent_write, update).await;
        }
    });

    ready_rx
        .await
        .expect("handshake completes")
        .expect("handshake ok");
    cmd_tx
        .send(ClientCmd::Prompt(vec![ContentBlock::Text(
            TextContent::new("hi"),
        )]))
        .await
        .unwrap();

    // Pause with a notification queued, then enqueue Cancel before the next
    // select can poll either receiver.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                ready = &mut paused_rx => { ready.unwrap(); break; }
                event = event_rx.recv() => { event.expect("connection remains open"); }
            }
        }
    })
    .await
    .expect("connection reaches the selection barrier");
    cmd_tx.send(ClientCmd::Cancel).await.unwrap();
    resume_tx.send(()).unwrap();

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
    assert!(
        winner_rx.await.unwrap(),
        "Cancel must beat the queued lifecycle notification"
    );

    flood.abort();
    agent.abort();
    connection.abort();
    let _ = tokio::join!(flood, agent, connection);
}

/// A new adapter process clears the previous process's identity before it can
/// report its own; a reattach to the surviving process keeps it.
#[tokio::test]
async fn auth_status_is_cleared_per_adapter_process() {
    let fresh = || ConnectMode::Fresh {
        stored_acp_session_id: None,
        seed_history_replay: false,
        prior_conversation: false,
        fork_from: None,
    };
    let reattach = ConnectMode::Resume {
        acp_session_id: "s-auth".into(),
        in_flight_turn: false,
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
            auth_events(mode, usize::from(reports)).await,
            want,
            "{name}"
        );
    }
}

#[tokio::test]
async fn auth_bootstrap_exceeding_public_and_shim_capacity_reaches_ready_before_drain() {
    for mode in [
        ConnectMode::Fresh {
            stored_acp_session_id: None,
            seed_history_replay: false,
            prior_conversation: false,
            fork_from: None,
        },
        ConnectMode::Resume {
            acp_session_id: "s-auth".into(),
            in_flight_turn: false,
        },
    ] {
        let fresh = matches!(&mode, ConnectMode::Fresh { .. });
        let reports = auth_events(mode, 96).await;
        assert_eq!(reports.iter().filter(|present| **present).count(), 96);
        assert_eq!(
            reports.iter().filter(|present| !**present).count(),
            usize::from(fresh)
        );
    }
}

/// `AuthStatusUpdated` events up to session commit, as "carries a report".
async fn auth_events(mode: ConnectMode, report_count: usize) -> Vec<bool> {
    let (daemon_write, agent_read) = tokio::io::duplex(64 * 1024);
    let (mut agent_write, daemon_read) = tokio::io::duplex(64 * 1024);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(1);
    let (ready_tx, ready_rx) = oneshot::channel();
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().to_path_buf();
    let params = ConnectionParams {
        connection_state: Arc::default(),
        claude_home: None,
        shutdown: tokio_util::sync::CancellationToken::new(),
        cancellation: tokio_util::sync::CancellationToken::new(),
        event_tx,
        cmd_rx,
        child: None,
        pending_responders: Arc::new(Mutex::new(HashMap::new())),
        resources: SessionResources {
            fs_policy: Arc::new(FsPolicy::new(vec![cwd.clone()])),
            terminals: TerminalManager::new(),
            cwd,
            label: "s-auth".to_string(),
            sandbox: None,
        },
        mode,
        ready_tx,
        profile: &crate::acp::agent_profiles::GEMINI,
        expected_agent: ExpectedAgent::Gemini,
        source_profile: None,
        default_effort: None,
        default_mode: None,
        default_model: None,
        mcp_servers: Vec::new(),
        runner: None,
    };
    let transport = ByteStreams::new(daemon_write.compat_write(), daemon_read.compat());
    let connection = tokio::spawn(run_connection_task(transport, params));
    // More than a shim's bytes and public queue's frames can precede initialize.
    let agent = tokio::spawn(async move {
        let mut lines = BufReader::new(agent_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id = &msg["id"];
            if msg["method"] == "initialize" && report_count != 0 {
                let report = serde_json::json!({
                    "jsonrpc": "2.0", "method": "_auth/status_update",
                    "params": {"authStatus": {
                        "kind": "account", "label": "x".repeat(1024),
                    }},
                })
                .to_string();
                for _ in 0..report_count {
                    agent_write.write_all(report.as_bytes()).await.unwrap();
                    agent_write.write_all(b"\n").await.unwrap();
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
                agent_write
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .unwrap();
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(10), ready_rx)
        .await
        .expect("initialize must not require a public event consumer")
        .expect("ready sender remains owned")
        .expect("initialize succeeds");

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
    agent.abort();
    connection.abort();
    let _ = tokio::join!(agent, connection);
    seen
}

/// A connection driven against a fake agent that answers the handshake and, on
/// each `session/prompt`, writes the lines `on_prompt` returns for its request id.
struct FakeConnection {
    event_rx: mpsc::Receiver<Event>,
    cmd_tx: mpsc::Sender<ClientCmd>,
    agent_write: SharedWrite,
    pending_responders: super::super::pending::PendingResponders,
    replies: mpsc::UnboundedReceiver<serde_json::Value>,
    event_queue: mpsc::WeakSender<Event>,
    connection_state: Arc<ConnectionState>,
    connection: tokio::task::JoinHandle<()>,
    agent: tokio::task::JoinHandle<()>,
    _cwd: tempfile::TempDir,
}

async fn spawn_fake_connection(
    session_id: &'static str,
    mode: Option<ConnectMode>,
    close_after_prompt: bool,
    on_prompt: impl Fn(&serde_json::Value) -> Vec<String> + Send + 'static,
) -> FakeConnection {
    let (daemon_write, agent_read) = tokio::io::duplex(64 * 1024);
    let (agent_write, daemon_read) = tokio::io::duplex(64 * 1024);
    let agent_write: SharedWrite = Arc::new(Mutex::new(agent_write));
    let (event_tx, event_rx) = mpsc::channel(64);
    let event_queue = event_tx.downgrade();
    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(1);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), AcpError>>();
    let pending_responders = Arc::new(Mutex::new(HashMap::new()));
    let (reply_tx, replies_rx) = mpsc::unbounded_channel();
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().to_path_buf();
    let connection_state = Arc::new(ConnectionState::default());
    let params = ConnectionParams {
        connection_state: Arc::clone(&connection_state),
        claude_home: None,
        event_tx,
        cmd_rx,
        child: None,
        pending_responders: pending_responders.clone(),
        resources: SessionResources {
            fs_policy: Arc::new(FsPolicy::new(vec![cwd.clone()])),
            terminals: TerminalManager::new(),
            cwd,
            label: session_id.to_string(),
            sandbox: None,
        },
        mode: mode.unwrap_or(ConnectMode::Fresh {
            stored_acp_session_id: None,
            seed_history_replay: false,
            prior_conversation: false,
            fork_from: None,
        }),
        ready_tx,
        profile: &crate::acp::agent_profiles::GEMINI,
        expected_agent: ExpectedAgent::Gemini,
        source_profile: None,
        default_effort: None,
        default_mode: None,
        default_model: None,
        mcp_servers: Vec::new(),
        runner: None,
        cancellation: tokio_util::sync::CancellationToken::new(),
        shutdown: tokio_util::sync::CancellationToken::new(),
    };
    let transport = ByteStreams::new(daemon_write.compat_write(), daemon_read.compat());
    let connection = tokio::spawn(run_connection_task(transport, params));
    let reply_write = agent_write.clone();
    let agent = tokio::spawn(async move {
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
                Some("session/prompt") => on_prompt(id),
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
    ready_rx
        .await
        .expect("handshake completes")
        .expect("handshake ok");
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
        _cwd: temp,
    }
}

#[tokio::test]
async fn correlated_config_result_survives_eof_while_public_queue_is_full() {
    let mut fake = spawn_fake_connection("s-config-eof", None, false, |id| {
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
    let sender = fake.event_queue.upgrade().unwrap();
    while sender
        .try_send(Event::AuthStatusUpdated { status: None })
        .is_ok()
    {}
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
    tokio::time::timeout(Duration::from_secs(10), fake.connection)
        .await
        .unwrap()
        .unwrap();
    fake.agent.abort();
}

/// A limit that ends an agent-initiated turn must end the connection, or the
/// drain loop never drops the worker and the reaper skips it as still running
/// (#4373).
#[tokio::test]
async fn rate_limit_on_an_agent_initiated_turn_ends_the_connection() {
    let mut fake = spawn_fake_connection("s-limit", None, false, |_| Vec::new()).await;

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

    // The idle grace is wall-clock, so the deadline covers it with margin.
    let mut seen = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = fake.event_rx.recv().await {
            seen.push(event);
        }
    })
    .await
    .expect("the connection must end after parking, closing the event channel");
    tokio::time::timeout(Duration::from_secs(10), fake.connection)
        .await
        .expect("connection task ends")
        .expect("connection task does not panic");

    let limit = seen.iter().position(|e| {
        matches!(e, Event::RateLimit { info } if info.resets_at.map(|t| t.timestamp()) == Some(resets_at))
    });
    let stopped = seen
        .iter()
        .position(|e| matches!(e, Event::Stopped { reason } if reason == "rate_limited"));
    assert!(
        limit.is_some() && limit < stopped,
        "RateLimit then Stopped(rate_limited) expected: {seen:?}"
    );
    fake.agent.abort();
    let _ = fake.agent.await;
}

/// A prompted turn the adapter ends normally after a rejected window parks like
/// the error path; a cancelled turn, an already-past reset, or a window extra
/// usage covers does not.
#[tokio::test]
async fn prompted_turn_ending_on_a_rejected_window_parks() {
    let now = chrono::Utc::now().timestamp();
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
        let mut fake = spawn_fake_connection("s-prompted", None, false, move |id| {
            vec![
                update.clone(),
                format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"stopReason":"{stop_reason}"}}}}"#
                ),
            ]
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
            // The connection must end so the reaper can resume it.
            tokio::time::timeout(Duration::from_secs(10), fake.connection)
                .await
                .expect("connection task ends after parking")
                .expect("connection task does not panic");
        } else {
            assert!(
                limit.is_none(),
                "{stop_reason}{overage}: must not park: {seen:?}"
            );
            assert_ne!(stopped_reason, "rate_limited", "{seen:?}");
            fake.connection.abort();
            let _ = fake.connection.await;
        }
        fake.agent.abort();
        let _ = fake.agent.await;
    }
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

#[tokio::test]
async fn request_errors_fail_the_turn_without_replacing_the_connection() {
    use crate::session::{ErrorBlockingScope, ErrorEvidenceSource, RuntimeEvidence};
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
        let mut fake = spawn_fake_connection("s-operation-error", None, false, move |id| {
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
            tokio::time::timeout(Duration::from_secs(10), fake.connection)
                .await
                .unwrap()
                .unwrap();
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
            assert!(!fake.connection.is_finished());
            fake.cmd_tx.send(ClientCmd::Cancel).await.unwrap();
            fake.cmd_tx
                .send(ClientCmd::Prompt(Vec::new()))
                .await
                .unwrap();
            let next = collect_terminal(&mut fake).await;
            assert!(
                matches!(next.last(), Some(Event::Stopped { reason }) if reason == "prompt_complete")
            );
            events.extend(next);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Event::Stopped { .. }))
                    .count(),
                2,
                "late cancel must not add a second terminal to the failed turn"
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
            fake.connection.abort();
            let _ = fake.connection.await;
        }
        let public = serde_json::to_string(&events).unwrap();
        assert!(!public.contains("privateTokenMarker") && !public.contains("must-not-leak"));
        fake.agent.abort();
        let _ = fake.agent.await;
    }
}

#[tokio::test]
async fn transport_and_protocol_failure_remain_fatal_across_turn_ownership() {
    for mode in ["idle", "active", "adopted", "cancelling", "after_error"] {
        let (entered, mut prompts) = mpsc::unbounded_channel();
        let resume = (mode == "adopted").then(|| ConnectMode::Resume {
            acp_session_id: "s-eof".into(),
            in_flight_turn: true,
        });
        let mut fake = spawn_fake_connection("s-eof", resume, mode == "after_error", move |id| {
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
        fake.agent.abort();
        let _ = fake.agent.await;
    }
}

#[tokio::test]
async fn unsupported_session_prompt_prose_preserves_identity_and_the_next_explicit_input() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let native_calls = calls.clone();
    let mut fake = spawn_fake_connection(
        "s-rejected",
        Some(ConnectMode::Resume {
            acp_session_id: "s-rejected".into(),
            in_flight_turn: false,
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
        },
    )
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
    fake.connection.abort();
    let _ = fake.connection.await;
    fake.agent.abort();
    let _ = fake.agent.await;
}

#[tokio::test]
async fn request_failure_keeps_the_pending_permission_responder_usable() {
    use super::super::pending::{
        ApprovalResolutionMessage, ElicitationResolutionMessage, PendingResolver,
    };
    use crate::acp::approvals::ApprovalDecision;
    use crate::acp::elicitations::ElicitationOutcome;
    use crate::acp::state::{AcpSessionId, AcpState, AgentName};
    use agent_client_protocol::schema::v1::{CreateElicitationResponse, ElicitationAction};
    for kind in ["permission", "elicitation"] {
        let (prompt_ids, mut requested) = mpsc::unbounded_channel();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let native_calls = calls.clone();
        let mut fake = spawn_fake_connection("s-permission-error", None, false, move |id| {
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
        let pending = fake
            .pending_responders
            .lock()
            .await
            .remove(&nonce)
            .expect("failure must not clear responders");
        match pending.resolver {
            PendingResolver::Approval(resolver) => resolver
                .send(ApprovalResolutionMessage::Decision {
                    decision: ApprovalDecision::Allow,
                    option_id: Some("allow".into()),
                })
                .unwrap_or_else(|_| panic!("approval callback must remain alive")),
            PendingResolver::Elicitation { resolver, .. } => resolver
                .send(ElicitationResolutionMessage {
                    response: CreateElicitationResponse::new(ElicitationAction::Decline),
                    outcome: ElicitationOutcome::Declined,
                    answers: Vec::new(),
                })
                .unwrap_or_else(|_| panic!("elicitation callback must remain alive")),
        }
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
                assert!(!matches!(event, Event::ThinkingStarted | Event::Stopped { .. }), "post-failure interaction output does not own another main terminal");
                let echoed = matches!(&event, Event::AgentMessageChunk { text, .. } if text == "interaction reply echoed");
                events.push(event);
                if echoed { break; }
            }
        }).await.unwrap();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, Event::Stopped { .. }))
                .count(),
            1
        );
        let mut state = AcpState::new(
            AcpSessionId("s-permission-error".into()),
            AgentName("gemini".into()),
            None,
        );
        state
            .apply_event(
                1,
                crate::acp::state::test_support::prompt("first explicit request"),
            )
            .unwrap();
        for (index, event) in events.into_iter().enumerate() {
            state.apply_event(index as u64 + 2, event).unwrap();
        }
        assert!(!state.turn_active);
        assert_eq!(
            state.sidebar_phase,
            crate::acp::state::SidebarPhase::PromptFailed
        );
        assert_eq!(state.sidebar_status(), crate::session::Status::Error);
        assert!(state.pending_approvals.is_empty() && state.pending_elicitations.is_empty());
        assert!(!fake.connection.is_finished());
        fake.cmd_tx
            .send(ClientCmd::Prompt(Vec::new()))
            .await
            .unwrap();
        let next = collect_terminal(&mut fake).await;
        assert!(
            matches!(next.last(), Some(Event::Stopped { reason }) if reason == "prompt_complete")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        fake.connection.abort();
        let _ = fake.connection.await;
        fake.agent.abort();
        let _ = fake.agent.await;
    }
}

#[tokio::test]
async fn request_error_then_eof_survives_public_event_backpressure() {
    let mut fake =
        spawn_fake_connection("s-backpressure", None, true, |id| {
            let mut lines: Vec<_> = (0..80)
                .map(|index| {
                    serde_json::json!({
                        "jsonrpc": "2.0", "method": "session/update",
                        "params": {"sessionId": "s-backpressure", "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "content": {"type": "text", "text": format!("partial output {index}\n")}
                        }}
                    })
                    .to_string()
                })
                .collect();
            lines.push(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": "request failed"}
        }).to_string());
            lines
        })
        .await;
    fake.cmd_tx
        .send(ClientCmd::Prompt(Vec::new()))
        .await
        .unwrap();
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
    let mut events = collect_terminal(&mut fake).await;
    tokio::time::timeout(Duration::from_secs(10), fake.connection)
        .await
        .unwrap()
        .unwrap();
    while let Some(event) = fake.event_rx.recv().await {
        events.push(event);
    }
    let failure = events
        .iter()
        .position(|event| matches!(event, Event::PromptRuntimeError { .. }))
        .unwrap();
    let terminal = events
        .iter()
        .position(|event| matches!(event, Event::Stopped { reason } if reason == "prompt_failed"))
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
    fake.agent.abort();
    let _ = fake.agent.await;
}

#[tokio::test]
async fn malformed_json_retains_the_sdk_parse_error_policy_and_live_connection() {
    let mut fake = spawn_fake_connection("s-parse-error", None, false, |id| {
        vec![
            serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}})
                .to_string(),
        ]
    })
    .await;
    write_line(&fake.agent_write, "{not valid JSON").await;
    let response = tokio::time::timeout(Duration::from_secs(10), fake.replies.recv())
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
    fake.connection.abort();
    let _ = fake.connection.await;
    fake.agent.abort();
    let _ = fake.agent.await;
}

#[tokio::test]
async fn successful_explicit_reset_releases_failed_main_terminal_ownership() {
    use super::super::reset::ResetSessionOutcome;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let native_calls = calls.clone();
    let mut fake = spawn_fake_connection("s-reset-failure", None, false, move |id| {
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
    fake.cmd_tx
        .send(ClientCmd::ResetSession {
            text: "/clear".into(),
            deadline: tokio::time::Instant::now() + Duration::from_secs(10),
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
    fake.connection.abort();
    let _ = fake.connection.await;
    fake.agent.abort();
    let _ = fake.agent.await;
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
    use super::super::pending::{
        ApprovalResolutionMessage, ElicitationResolutionMessage, PendingResolver,
    };
    use crate::acp::{approvals::ApprovalDecision, elicitations::ElicitationOutcome};
    use agent_client_protocol::schema::v1::{CreateElicitationResponse, ElicitationAction};
    let pending = fake
        .pending_responders
        .lock()
        .await
        .remove(nonce)
        .expect("responder remains registered");
    match pending.resolver {
        PendingResolver::Approval(resolver) => resolver
            .send(ApprovalResolutionMessage::Decision {
                decision: ApprovalDecision::Allow,
                option_id: Some("allow".into()),
            })
            .unwrap_or_else(|_| panic!("approval callback must remain alive")),
        PendingResolver::Elicitation { resolver, .. } => resolver
            .send(ElicitationResolutionMessage {
                response: CreateElicitationResponse::new(ElicitationAction::Decline),
                outcome: ElicitationOutcome::Declined,
                answers: Vec::new(),
            })
            .unwrap_or_else(|_| panic!("elicitation callback must remain alive")),
    }
}

#[tokio::test]
async fn pending_callback_reset_write_waits_for_resolution_or_deadline() {
    use super::super::reset::ResetSessionOutcome;
    for kind in ["permission", "elicitation"] {
        for resolve_before_deadline in [true, false] {
            let mut fake =
                spawn_fake_connection("s-pending-reset", None, false, |_| Vec::new()).await;
            write_line(
                &fake.agent_write,
                &pending_interaction_line(kind, "s-pending-reset"),
            )
            .await;
            let nonce = registered_interaction(&mut fake).await;
            let (respond_to, mut response) = oneshot::channel();
            let bound = if resolve_before_deadline {
                Duration::from_secs(10)
            } else {
                Duration::from_millis(100)
            };
            fake.cmd_tx
                .send(ClientCmd::ResetSession {
                    text: "/clear".into(),
                    deadline: tokio::time::Instant::now() + bound,
                    respond_to,
                })
                .await
                .unwrap();
            // The capacity-one channel cannot accept another command until
            // the foreground dequeues reset and polls its blocked write fence.
            drop(
                tokio::time::timeout(Duration::from_secs(10), fake.cmd_tx.reserve())
                    .await
                    .unwrap()
                    .unwrap(),
            );
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
                assert!(!events.iter().any(|event| matches!(event, Event::AgentMessageChunk { text, .. } if text == "old-session progress while reset waits")),
                    "old-session notification cannot cross the reset identity boundary");
            } else {
                assert!(matches!(
                    tokio::time::timeout(Duration::from_secs(10), response).await.unwrap().unwrap(),
                    ResetSessionOutcome::Failed { message } if message.contains("agent callbacks")
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
            fake.connection.abort();
            let _ = fake.connection.await;
            fake.agent.abort();
            let _ = fake.agent.await;
        }
    }
}

#[tokio::test]
async fn transport_eof_cancels_unanswered_callbacks_after_intervening_progress() {
    use super::super::pending::PendingResolver;
    for kind in ["permission", "elicitation"] {
        let mut fake = spawn_fake_connection("s-pending-eof", None, false, move |_| {
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
        fake.agent.abort();
        let _ = fake.agent.await;
    }
}
