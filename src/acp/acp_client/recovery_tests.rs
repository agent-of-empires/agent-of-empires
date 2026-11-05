use super::*;
use crate::acp::control_protocol::{self, ControlBody, PromptOutcome};
use crate::acp::runner_lifecycle::{LifecycleTable, ResumeKind};
use std::time::Duration;
use tokio::net::UnixListener;
use tokio::time::{timeout, Instant};

struct Runner {
    temp: tempfile::TempDir,
    frames: mpsc::UnboundedReceiver<ControlBody>,
    send: mpsc::Sender<Option<ControlBody>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Runner {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Runner {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let listener = UnixListener::bind(crate::process::worker::control_socket_sibling(
            &temp.path().join("runner.sock"),
        ))
        .unwrap();
        let (send, mut receive) = mpsc::channel::<Option<ControlBody>>(256);
        let (frames_tx, frames) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                control_protocol::write_frame(
                    &mut stream,
                    &ControlBody::Hello {
                        control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                        session_id: "recovery".into(),
                    },
                )
                .await
                .unwrap();
                loop {
                    let frame = tokio::select! {
                        frame = control_protocol::read_frame(&mut stream) => match frame.unwrap() {
                            Some(frame) => frame,
                            None => break,
                        },
                        body = receive.recv() => {
                            match body {
                                Some(Some(body)) => control_protocol::write_frame(&mut stream, &body).await.unwrap(),
                                Some(None) => break,
                                None => return,
                            }
                            continue;
                        }
                    };
                    let response = match &frame {
                        ControlBody::Attach { .. } => None,
                        ControlBody::Initialize { .. } => Some(ControlBody::Initialized {
                            result: serde_json::json!({"protocolVersion": 1, "agentCapabilities": {}}),
                        }),
                        ControlBody::ResumeSession => Some(ControlBody::SessionReady {
                            acp_session_id: "native".into(),
                            result: serde_json::json!({}),
                        }),
                        ControlBody::Prompt { .. } => {
                            Some(ControlBody::PromptStarted { prompt_req_id: 7 })
                        }
                        _ => None,
                    };
                    if let Some(response) = response {
                        control_protocol::write_frame(&mut stream, &response)
                            .await
                            .unwrap();
                    }
                    frames_tx.send(frame).unwrap();
                }
            }
        });
        Self {
            temp,
            frames,
            send,
            task,
        }
    }

    async fn attach(&self, in_flight: bool) -> AcpClient {
        let mut client = AcpClient::attach(
            self.temp.path().join("runner.sock"),
            self.temp.path().into(),
            vec![],
            "native".into(),
            in_flight,
            AcpSessionId("recovery".into()),
            None,
            "test-agent".into(),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        assigned(&mut client).await;
        client.test_flush_commands().await;
        client
    }

    async fn frame(&mut self) -> ControlBody {
        loop {
            let frame = timeout(Duration::from_secs(5), self.frames.recv())
                .await
                .unwrap()
                .unwrap();
            if !matches!(
                frame,
                ControlBody::Attach { .. }
                    | ControlBody::Initialize { .. }
                    | ControlBody::ResumeSession
            ) {
                return frame;
            }
        }
    }

    async fn send(&self, frame: ControlBody) {
        self.send.send(Some(frame)).await.unwrap();
    }

    async fn call(&mut self, method: &str, fields: serde_json::Value) -> serde_json::Value {
        let mut params = fields;
        params["sessionId"] = "native".into();
        self.send(ControlBody::ServerCall {
            call_id: 1,
            method: method.into(),
            params,
        })
        .await;
        match self.frame().await {
            ControlBody::ServerResult { call_id: 1, result } => result,
            other => panic!("callback failed: {other:?}"),
        }
    }

    async fn disconnect(&self, client: &mut AcpClient) {
        self.send.send(None).await.unwrap();
        timeout(Duration::from_secs(5), async {
            while let Some(event) = client.next_event().await {
                assert!(
                    !matches!(
                        event,
                        Event::Stopped { .. } | Event::AgentStartupError { .. }
                    ),
                    "{event:?}"
                );
            }
        })
        .await
        .expect("connection loss must release watchdog senders promptly");
        assert!(client.control_loss().is_some());
    }

    async fn recover(&self, client: &AcpClient, in_flight: bool) -> AcpClient {
        let lease = LifecycleTable::new(1)
            .admit("recovery", ResumeKind::Attach)
            .unwrap();
        let mut next = client
            .reattach(
                "native".into(),
                in_flight,
                Instant::now() + Duration::from_secs(5),
                lease,
            )
            .await
            .unwrap();
        assigned(&mut next).await;
        next.test_flush_commands().await;
        next
    }

    async fn complete(&self, stop_reason: &str) {
        self.send(ControlBody::PromptCompleted {
            prompt_req_id: 7,
            outcome: PromptOutcome::Completed {
                stop_reason: Some(stop_reason.into()),
            },
        })
        .await;
    }
}

async fn assigned(client: &mut AcpClient) {
    timeout(Duration::from_secs(5), async {
        loop {
            match client.next_event().await.unwrap() {
                Event::AcpSessionAssigned { acp_session_id } => {
                    assert_eq!(acp_session_id, "native");
                    return;
                }
                Event::AgentStartupError { message } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap();
}

async fn stopped(client: &mut AcpClient) -> String {
    timeout(Duration::from_secs(5), async {
        loop {
            match client.next_event().await.unwrap() {
                Event::Stopped { reason } => return reason,
                Event::AgentStartupError { message } => panic!("{message}"),
                _ => {}
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn terminal_callbacks_keep_their_ids_after_control_recovery() {
    let _env = crate::session::test_support::EnvGuard::read_lock();
    let mut runner = Runner::new();
    let mut first = runner.attach(false).await;
    let terminal = runner
        .call(
            "terminal/create",
            serde_json::json!({"command": "echo", "args": ["retained"]}),
        )
        .await;
    let id = terminal["terminalId"].as_str().unwrap();
    runner.disconnect(&mut first).await;
    let next = runner.recover(&first, false).await;
    let fields = serde_json::json!({"terminalId": id});
    let output = runner.call("terminal/output", fields.clone()).await;
    assert_eq!(output["output"], "retained\n");
    assert_eq!(
        runner.call("terminal/wait_for_exit", fields.clone()).await["exitCode"],
        0
    );
    runner.call("terminal/release", fields.clone()).await;
    runner
        .send(ControlBody::ServerCall {
            call_id: 2,
            method: "terminal/output".into(),
            params: serde_json::json!({"sessionId": "native", "terminalId": id}),
        })
        .await;
    assert!(matches!(
        runner.frame().await,
        ControlBody::ServerError { call_id: 2, .. }
    ));
    next.shutdown().await.unwrap();
}

#[tokio::test]
async fn completed_turn_keeps_its_terminal_claim_when_eof_snapshots_it_active() {
    let _env = crate::session::test_support::EnvGuard::read_lock();
    let mut runner = Runner::new();
    let mut first = runner.attach(false).await;
    first.send_prompt("run", &[]).await.unwrap();
    assert!(matches!(runner.frame().await, ControlBody::Prompt { .. }));
    runner.complete("end_turn").await;
    assert_eq!(stopped(&mut first).await, "prompt_complete");
    runner.disconnect(&mut first).await;
    // EOF can observe the active flag before the prior terminal send clears it.
    let mut next = runner.recover(&first, true).await;
    runner.complete("end_turn").await;
    // A later callback proves the cached completion passed the ordered dispatcher.
    runner
        .call(
            "terminal/release",
            serde_json::json!({"terminalId": "unused"}),
        )
        .await;
    next.test_flush_commands().await;
    assert!(matches!(
        next.inbound.as_mut().unwrap().try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    next.shutdown().await.unwrap();
}

#[tokio::test]
async fn terminal_publication_survives_eof_while_the_event_channel_is_full() {
    let _env = crate::session::test_support::EnvGuard::read_lock();
    for (local, limited) in [(true, false), (false, false), (true, true), (false, true)] {
        let mut runner = Runner::new();
        let mut first = runner.attach(!local).await;
        if local {
            first.send_prompt("run", &[]).await.unwrap();
            assert!(matches!(runner.frame().await, ControlBody::Prompt { .. }));
        }
        let capacity = first.inbound.as_ref().unwrap().max_capacity();
        for index in 0..capacity {
            runner
                .send(ControlBody::Notify {
                    method: "session/update".into(),
                    params: serde_json::json!({"sessionId": "native", "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": format!("queued-{index}")}
                    }}),
                })
                .await;
        }
        if limited {
            runner
                .send(ControlBody::PromptCompleted {
                    prompt_req_id: 7,
                    outcome: PromptOutcome::Error {
                        code: -32000,
                        message: "rate limit exceeded".into(),
                        data: Some(serde_json::json!({"errorKind": "rate_limit"})),
                    },
                })
                .await;
        } else {
            runner.complete("end_turn").await;
        }
        let claim = first
            .reattach
            .as_ref()
            .unwrap()
            .state
            .terminal_claim
            .clone();
        timeout(Duration::from_secs(5), async {
            while !claim.claimed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(first.inbound.as_ref().unwrap().len(), capacity);
        runner.send.send(None).await.unwrap();
        timeout(Duration::from_secs(5), async {
            while !first
                .connection_task
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .is_finished()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the transport must close while terminal publication is backpressured");
        let mut terminals = 0;
        let mut rate_limits = 0;
        timeout(Duration::from_secs(5), async {
            while let Some(event) = first.next_event().await {
                terminals += usize::from(matches!(event, Event::Stopped { .. }));
                rate_limits += usize::from(matches!(event, Event::RateLimit { .. }));
            }
        })
        .await
        .unwrap();
        let mut next = runner.recover(&first, true).await;
        runner.complete("end_turn").await;
        runner
            .call(
                "terminal/release",
                serde_json::json!({"terminalId": "unused"}),
            )
            .await;
        next.test_flush_commands().await;
        while let Ok(event) = next.inbound.as_mut().unwrap().try_recv() {
            terminals += usize::from(matches!(event, Event::Stopped { .. }));
        }
        assert_eq!(
            terminals, 1,
            "a terminal claimed before EOF must reach the drain exactly once"
        );
        assert_eq!(rate_limits, usize::from(limited));
        next.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn cancelling_turn_recovers_with_only_the_original_grace_remaining() {
    let _env = crate::session::test_support::EnvGuard::read_lock();
    let mut runner = Runner::new();
    let mut first = runner.attach(false).await;
    first.send_prompt("run", &[]).await.unwrap();
    assert!(matches!(runner.frame().await, ControlBody::Prompt { .. }));
    first.cancel_prompt().await.unwrap();
    assert!(matches!(runner.frame().await, ControlBody::Cancel));
    loop {
        if matches!(
            first.next_event().await.unwrap(),
            Event::CancelRequested { .. }
        ) {
            break;
        }
    }
    // Only the Tokio cancellation timer affects this assertion.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::time::resume();
    let deadline = first
        .reattach
        .as_ref()
        .unwrap()
        .state
        .cancel_deadline
        .lock()
        .unwrap()
        .unwrap();
    runner.disconnect(&mut first).await;
    let mut next = runner.recover(&first, true).await;
    next.cancel_prompt().await.unwrap();
    assert!(matches!(runner.frame().await, ControlBody::Cancel));
    next.test_flush_commands().await;
    assert!(
        tokio::time::timeout_at(deadline - Duration::from_millis(1), next.next_event())
            .await
            .is_err(),
        "recovery must retain the remaining grace"
    );
    assert_eq!(stopped(&mut next).await, "agent_unresponsive");
    assert!(timeout(Duration::from_secs(2), next.next_event())
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn silent_initial_adoption_releases_its_watchdog_on_control_loss() {
    let _env = crate::session::test_support::EnvGuard::read_lock();
    let runner = Runner::new();
    let mut first = runner.attach(true).await;
    runner.disconnect(&mut first).await;
    let mut next = runner.recover(&first, true).await;
    runner.complete("end_turn").await;
    assert_eq!(stopped(&mut next).await, "prompt_complete");
    next.shutdown().await.unwrap();
}

#[tokio::test]
async fn recovered_completion_orders_updates_and_applies_rate_limit_parking() {
    let _env = crate::session::test_support::EnvGuard::read_lock();
    for (report, stop_reason, reset_after, parked) in [
        ("rejected", "end_turn", 3600, true),
        ("rejected", "cancelled", 3600, false),
        ("allowed", "end_turn", 3600, false),
        ("rejected", "end_turn", -60, false),
        ("rejected", "error", 3600, true),
    ] {
        let mut runner = Runner::new();
        let mut first = runner.attach(false).await;
        first.send_prompt("run", &[]).await.unwrap();
        assert!(matches!(runner.frame().await, ControlBody::Prompt { .. }));
        let resets_at = chrono::Utc::now().timestamp() + reset_after;
        runner.send(ControlBody::Notify { method: "session/update".into(), params: serde_json::json!({"sessionId": "native", "update": {
            "sessionUpdate": "usage_update", "used": 1, "size": 2, "_meta": {"_claude/rateLimit": {"status": "rejected", "resetsAt": resets_at}}
        }}) }).await;
        // The callback reply proves the pre-loss rejection was captured.
        runner
            .call(
                "terminal/release",
                serde_json::json!({"terminalId": "unused"}),
            )
            .await;
        runner.disconnect(&mut first).await;
        let mut next = runner.recover(&first, true).await;
        if report == "allowed" {
            runner.send(ControlBody::Notify { method: "session/update".into(), params: serde_json::json!({"sessionId": "native", "update": {
                "sessionUpdate": "usage_update", "used": 1, "size": 2, "_meta": {"_claude/rateLimit": {"status": report}}
            }}) }).await;
        }
        for index in 0..100 {
            runner.send(ControlBody::Notify { method: "session/update".into(), params: serde_json::json!({"sessionId": "native", "update": {
                "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": format!("final-{index}")}
            }}) }).await;
        }
        if stop_reason == "error" {
            runner
                .send(ControlBody::PromptCompleted {
                    prompt_req_id: 7,
                    outcome: PromptOutcome::Error {
                        code: -32000,
                        message: "rate limit exceeded".into(),
                        data: Some(serde_json::json!({"errorKind": "rate_limit"})),
                    },
                })
                .await;
        } else {
            runner.complete(stop_reason).await;
        }
        let mut chunks = Vec::new();
        let mut limited = false;
        loop {
            match timeout(Duration::from_secs(5), next.next_event())
                .await
                .unwrap()
                .unwrap()
            {
                Event::AgentMessageChunk { text, .. } => chunks.push(text),
                Event::RateLimit { info } => {
                    limited = true;
                    assert_eq!(info.resets_at.unwrap().timestamp(), resets_at);
                }
                Event::Stopped { reason } => {
                    assert_eq!(
                        reason,
                        if parked {
                            "rate_limited"
                        } else if stop_reason == "cancelled" {
                            "cancelled"
                        } else {
                            "prompt_complete"
                        }
                    );
                    break;
                }
                Event::AgentStartupError { message } => panic!("{message}"),
                _ => {}
            }
        }
        assert_eq!(
            chunks,
            (0..100)
                .map(|index| format!("final-{index}"))
                .collect::<Vec<_>>()
        );
        assert_eq!(limited, parked);
        if parked {
            assert!(
                timeout(Duration::from_secs(2), next.next_event())
                    .await
                    .unwrap()
                    .is_none(),
                "parking must end the event stream"
            );
        } else {
            next.test_flush_commands().await;
            next.shutdown().await.unwrap();
        }
    }
}
