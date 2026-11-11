//! The connection task driven against a fake agent over in-memory pipes.

use super::super::pending::{ApprovalResolutionMessage, PendingResolver, PendingResponder};
use super::prompt::{SelectProbe, SELECT_PROBE};
use super::*;
use crate::acp::acp_client::reset::ResetSessionOutcome;
use crate::acp::approvals::ApprovalDecision;
use crate::acp::fs_handler::FsPolicy;
use crate::acp::state::SubagentState;
use crate::acp::terminal_handler::TerminalManager;
use agent_client_protocol::v1::schema::{ContentBlock, TextContent};
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
        default_model_config: Default::default(),
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
        assert_eq!(auth_events(mode, reports).await, want, "{name}");
    }
}

/// `AuthStatusUpdated` events up to session commit, as "carries a report".
async fn auth_events(mode: ConnectMode, reports: bool) -> Vec<bool> {
    let (daemon_write, agent_read) = tokio::io::duplex(64 * 1024);
    let (mut agent_write, daemon_read) = tokio::io::duplex(64 * 1024);
    let (event_tx, mut event_rx) = mpsc::channel(64);
    let (_cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(1);
    let (ready_tx, _ready_rx) = oneshot::channel();
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().to_path_buf();
    let params = ConnectionParams {
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
        default_model_config: Default::default(),
        default_mode: None,
        default_model: None,
        mcp_servers: Vec::new(),
        runner: None,
    };
    let transport = ByteStreams::new(daemon_write.compat_write(), daemon_read.compat());
    let connection = tokio::spawn(run_connection_task(transport, params));
    // Reports right after answering `initialize`, the earliest it can.
    let agent = tokio::spawn(async move {
        let mut lines = BufReader::new(agent_read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id = &msg["id"];
            let mut out = match msg["method"].as_str() {
                Some("initialize") => vec![format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1,"agentCapabilities":{{"_meta":{{"authStatus":{{}}}}}}}}}}"#
                )],
                Some("session/new") => vec![format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"sessionId":"s-auth"}}}}"#
                )],
                _ => vec![],
            };
            if reports && msg["method"] == "initialize" {
                out.push(r#"{"jsonrpc":"2.0","method":"_auth/status_update","params":{"authStatus":{"kind":"account","label":"Claude Max"}}}"#.into());
            }
            for line in out {
                agent_write
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .unwrap();
            }
        }
    });

    let (mut seen, mut committed) = (Vec::new(), false);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !committed || (reports && seen.last() != Some(&true)) {
            match event_rx.recv().await.expect("connection remains open") {
                Event::AuthStatusUpdated { status } => seen.push(status.is_some()),
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
    pending_responders: PendingResponders,
    /// Replies the daemon sent to the agent's own requests.
    responses: mpsc::UnboundedReceiver<serde_json::Value>,
    connection: tokio::task::JoinHandle<()>,
    agent: tokio::task::JoinHandle<()>,
    _cwd: tempfile::TempDir,
}

async fn spawn_fake_connection(
    session_id: &'static str,
    on_prompt: impl Fn(&serde_json::Value) -> Vec<String> + Send + 'static,
) -> FakeConnection {
    let (daemon_write, agent_read) = tokio::io::duplex(64 * 1024);
    let (agent_write, daemon_read) = tokio::io::duplex(64 * 1024);
    let agent_write: SharedWrite = Arc::new(Mutex::new(agent_write));
    let (event_tx, event_rx) = mpsc::channel(64);
    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(1);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), AcpError>>();
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().to_path_buf();
    let pending_responders: PendingResponders = Arc::new(Mutex::new(HashMap::new()));
    let (response_tx, responses) = mpsc::unbounded_channel();
    let params = ConnectionParams {
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
        default_model_config: Default::default(),
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
        while let Ok(Some(line)) = lines.next_line().await {
            let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id = &msg["id"];
            let replies = match msg["method"].as_str() {
                Some("initialize") => vec![format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":1,"agentCapabilities":{{}}}}}}"#
                )],
                Some("session/new") => vec![format!(
                    r#"{{"jsonrpc":"2.0","id":{id},"result":{{"sessionId":"{session_id}"}}}}"#
                )],
                Some("session/prompt") => on_prompt(id),
                None => {
                    let _ = response_tx.send(msg);
                    continue;
                }
                _ => continue,
            };
            for reply in replies {
                write_line(&reply_write, &reply).await;
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
        responses,
        connection,
        agent,
        _cwd: temp,
    }
}

/// A limit that ends an agent-initiated turn must end the connection, or the
/// drain loop never drops the worker and the reaper skips it as still running
/// (#4373).
#[tokio::test]
async fn rate_limit_on_an_agent_initiated_turn_ends_the_connection() {
    let mut fake = spawn_fake_connection("s-limit", |_| Vec::new()).await;

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

/// A message typed in another Remote Control client arrives as a live
/// `user_message_chunk` with no AoE prompt in flight (#5086). It must show as
/// a user message, and the reply that follows must close through the
/// between-prompt watchdog.
#[tokio::test]
async fn idle_remote_control_message_records_and_returns_to_idle() {
    let mut fake = spawn_fake_connection("s-remote", |_| Vec::new()).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            fake.event_rx.recv().await.expect("connection remains open"),
            Event::AcpSessionAssigned { .. }
        ) {}
    })
    .await
    .expect("session commits");

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

    // The idle grace is wall-clock, so the deadline covers it with margin.
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
        let mut fake = spawn_fake_connection("s-prompted", move |id| {
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

/// A child cancel reaches the agent addressed to the child and only when the
/// child announced `cancel`; the root turn's cancel stays a separate command.
#[tokio::test]
async fn child_cancel_targets_the_child_and_requires_the_capability() {
    let (daemon_write, agent_read) = tokio::io::duplex(1024 * 1024);
    let (agent_write, daemon_read) = tokio::io::duplex(1024 * 1024);
    let agent_write: SharedWrite = Arc::new(Mutex::new(agent_write));
    let cancels = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    let agent = tokio::spawn({
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
                    Some("session/cancel") => cancels
                        .lock()
                        .unwrap()
                        .push(msg["params"]["sessionId"].as_str().unwrap().to_string()),
                    _ => {}
                }
            }
        }
    });

    let (event_tx, mut event_rx) = mpsc::channel(64);
    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(16);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), AcpError>>();
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().to_path_buf();
    let params = ConnectionParams {
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
            label: "root".to_string(),
            sandbox: None,
        },
        mode: ConnectMode::Fresh {
            stored_acp_session_id: None,
            seed_history_replay: false,
            prior_conversation: false,
            fork_from: None,
        },
        ready_tx,
        profile: &crate::acp::agent_profiles::CLAUDE,
        expected_agent: ExpectedAgent::Gemini,
        source_profile: None,
        default_effort: None,
        default_mode: None,
        default_model_config: Default::default(),
        default_model: None,
        mcp_servers: Vec::new(),
        runner: None,
    };
    let transport = ByteStreams::new(daemon_write.compat_write(), daemon_read.compat());
    let connection = tokio::spawn(run_connection_task(transport, params));
    ready_rx.await.unwrap().expect("handshake ok");

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

    agent.abort();
    connection.abort();
    let _ = tokio::join!(agent, connection);
}

/// A reset is refused while a child runs, since it would orphan the child's
/// routes, and goes through once the child reports idle.
#[tokio::test]
async fn reset_waits_for_a_running_subagent() {
    let (daemon_write, agent_read) = tokio::io::duplex(1024 * 1024);
    let (agent_write, daemon_read) = tokio::io::duplex(1024 * 1024);
    let agent_write: SharedWrite = Arc::new(Mutex::new(agent_write));
    let new_sessions = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let agent = tokio::spawn({
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

    let (event_tx, mut event_rx) = mpsc::channel(64);
    let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(16);
    let (ready_tx, ready_rx) = oneshot::channel::<Result<(), AcpError>>();
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().to_path_buf();
    let params = ConnectionParams {
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
            label: "root".to_string(),
            sandbox: None,
        },
        mode: ConnectMode::Fresh {
            stored_acp_session_id: None,
            seed_history_replay: false,
            prior_conversation: false,
            fork_from: None,
        },
        ready_tx,
        profile: &crate::acp::agent_profiles::CLAUDE,
        expected_agent: ExpectedAgent::Gemini,
        source_profile: None,
        default_effort: None,
        default_mode: None,
        default_model_config: Default::default(),
        default_model: None,
        mcp_servers: Vec::new(),
        runner: None,
    };
    let transport = ByteStreams::new(daemon_write.compat_write(), daemon_read.compat());
    let connection = tokio::spawn(run_connection_task(transport, params));
    ready_rx.await.unwrap().expect("handshake ok");
    next_child_event(&mut event_rx, "child").await;

    let reset = |cmd_tx: mpsc::Sender<ClientCmd>| async move {
        let (respond_to, answer) = oneshot::channel();
        cmd_tx
            .send(ClientCmd::ResetSession {
                text: "/clear".into(),
                deadline: tokio::time::Instant::now() + Duration::from_secs(30),
                respond_to,
            })
            .await
            .unwrap();
        answer.await.unwrap()
    };

    match reset(cmd_tx.clone()).await {
        ResetSessionOutcome::Failed { message } => {
            assert!(message.contains("subagent is still running"), "{message}");
        }
        other => panic!("reset must be refused: {other:?}"),
    }
    assert_eq!(new_sessions.load(std::sync::atomic::Ordering::SeqCst), 1);

    // The agent reports the child idle before it ends the turn.
    cmd_tx
        .send(ClientCmd::Prompt(vec![ContentBlock::Text(
            TextContent::new("go"),
        )]))
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

    assert!(matches!(
        reset(cmd_tx.clone()).await,
        ResetSessionOutcome::Reset { .. }
    ));
    assert_eq!(new_sessions.load(std::sync::atomic::Ordering::SeqCst), 2);

    agent.abort();
    connection.abort();
    let _ = tokio::join!(agent, connection);
}

async fn next_child_event(event_rx: &mut mpsc::Receiver<Event>, child: &str) {
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

/// A child's approval waits on the human without holding the dispatch fence:
/// a sibling's announcement is handled first, and the child's answer then
/// reaches the adapter on the child's own request id.
#[tokio::test]
async fn child_approval_wait_does_not_stall_other_sessions() {
    let FakeConnection {
        mut event_rx,
        agent_write,
        cmd_tx: _cmd_tx,
        pending_responders,
        mut responses,
        connection,
        agent,
        ..
    } = spawn_fake_connection("root", |_| Vec::new()).await;
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

    let PendingResponder { resolver } = pending_responders
        .lock()
        .await
        .remove(&nonce)
        .expect("approval parked");
    let PendingResolver::Approval(tx) = resolver else {
        panic!("approval resolver expected");
    };
    tx.send(ApprovalResolutionMessage::Decision {
        decision: ApprovalDecision::Allow,
        option_id: None,
    })
    .map_err(|_| "resolver gone")
    .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(30), responses.recv())
        .await
        .expect("response reaches the adapter")
        .expect("agent still reading");
    assert_eq!(response["id"], 900);
    assert_eq!(response["result"]["outcome"]["outcome"], "selected");
    assert_eq!(response["result"]["outcome"]["optionId"], "allow");

    agent.abort();
    connection.abort();
    let _ = tokio::join!(agent, connection);
}
