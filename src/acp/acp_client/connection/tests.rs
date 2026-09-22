//! Fairness of the in-flight-prompt select: Cancel must not sit behind the
//! lifecycle arm, which a sustained update stream keeps permanently ready.
//! A fake agent floods updates until `session/cancel` arrives.

use super::prompt::{SelectProbe, SELECT_PROBE};
use super::*;
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
        event_tx,
        cmd_rx,
        child: None,
        pending_responders: Arc::new(Mutex::new(HashMap::new())),
        resources,
        mode: ConnectMode::Fresh {
            stored_acp_session_id: None,
            seed_history_replay: false,
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
