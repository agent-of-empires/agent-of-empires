//! Driven conversation reset: the deadline discipline that keeps a refused
//! reset from wedging the connection loop, and the outcomes callers see.

/// Hard cap on a driven conversation reset's `session/new` round-trip
/// (#2979). A fresh session on a live, already-initialized adapter
/// normally answers in well under a second; the timeout keeps a wedged
/// adapter from stalling the prompt path that requested the reset.
pub(super) const SESSION_RESET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Inner deadline for the connection task's complete reset RPC sequence.
/// Kept below `SESSION_RESET_TIMEOUT` so the task can report the specific
/// failure before the caller's outer guard expires, then resume draining
/// commands instead of remaining parked on a wedged adapter.
pub(super) const SESSION_RESET_IN_TASK_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(28);

#[derive(Debug)]
pub(super) enum ResetRequestError {
    Acp(agent_client_protocol::Error),
    TimedOut,
}

pub(super) async fn await_reset_request<T, F, Fut>(
    deadline: tokio::time::Instant,
    request: F,
) -> Result<T, ResetRequestError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, agent_client_protocol::Error>>,
{
    // `send_request` enqueues the stateful RPC synchronously. Keep it
    // lazy so a command whose caller-created deadline expired in the
    // queue cannot send a late session/new or config mutation at all.
    if tokio::time::Instant::now() >= deadline {
        return Err(ResetRequestError::TimedOut);
    }
    match tokio::time::timeout_at(deadline, request()).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => Err(ResetRequestError::Acp(error)),
        Err(_) => Err(ResetRequestError::TimedOut),
    }
}

/// Outcome of a driven conversation reset (`ClientCmd::ResetSession`).
#[derive(Debug)]
pub enum ResetSessionOutcome {
    /// `session/new` succeeded and the connection task swapped its ACP
    /// session id; carries the fresh id for logging.
    Reset { new_acp_session_id: String },
    /// The reset did not happen: `session/new` failed, timed out, a turn
    /// was in flight, or a stale runner replayed the old session from its
    /// handshake cache. The conversation keeps its context.
    Failed { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::commands::ClientCmd;
    use crate::acp::acp_client::test_helpers::reset_fake_spawn_config;
    use crate::acp::acp_client::AcpClient;
    use crate::acp::state::{AcpSessionId, Event};
    use tokio::sync::oneshot;

    #[test]
    fn reset_request_deadline_precedes_the_outer_guard() {
        assert!(
            SESSION_RESET_IN_TASK_TIMEOUT < SESSION_RESET_TIMEOUT,
            "the in-task deadline must expire before the caller's outer guard"
        );
    }

    /// Write a scripted stdio ACP agent for the conversation-reset tests
    /// (#2979): answers `initialize`, mints `sid-1`, `sid-2`, ... on each
    /// `session/new` (each carrying a `thought_level` config option so the
    /// default-effort application path has a target), acks
    /// `session/set_config_option`, and answers every `session/prompt`
    /// with an `agent_message_chunk` notification, a `prompt_delay_secs`
    /// pause (0 = immediate), then the turn-ending response.
    /// `reset_new_delay_secs` delays only the second `session/new`, while
    /// `reset_config_delay_secs` delays only the second config request;
    /// those hooks exercise the reset deadlines without slowing ordinary
    /// tests. Appends every inbound request line to the returned capture
    /// file so tests can assert exactly which requests were issued.
    #[cfg(unix)]
    fn write_reset_fake_agent(
        dir: &std::path::Path,
        prompt_delay_secs: u32,
        reset_new_delay_secs: u32,
        reset_config_delay_secs: u32,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        write_reset_fake_agent_with_initial_update(
            dir,
            prompt_delay_secs,
            reset_new_delay_secs,
            reset_config_delay_secs,
            None,
        )
    }

    /// Variant that emits one unsolicited update immediately after the
    /// initial `session/new`. This reproduces between-prompt work without
    /// reaching into the connection task's private tracking state.
    #[cfg(unix)]
    fn write_reset_fake_agent_with_initial_update(
        dir: &std::path::Path,
        prompt_delay_secs: u32,
        reset_new_delay_secs: u32,
        reset_config_delay_secs: u32,
        initial_update: Option<serde_json::Value>,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let capture = dir.join("capture.ndjson");
        let script_path = dir.join("fake-reset-agent.sh");
        let initial_notification = initial_update
            .map(|update| {
                let payload = serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "session/update",
                    "params": {
                        "sessionId": "sid-1",
                        "update": update,
                    },
                })
                .to_string();
                let shell_quoted = format!("'{}'", payload.replace('\'', "'\"'\"'"));
                format!("if [ \"$count\" -eq 1 ]; then printf '%s\\n' {shell_quoted}; fi")
            })
            .unwrap_or_else(|| ":".into());
        let script = r#"#!/bin/sh
CAPTURE=__CAPTURE__
DELAY=__DELAY__
RESET_NEW_DELAY=__RESET_NEW_DELAY__
RESET_CONFIG_DELAY=__RESET_CONFIG_DELAY__
count=0
config_count=0
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CAPTURE"
  id=$(printf '%s' "$line" | sed -En 's/.*"id":("[^"]*"|[0-9]+).*/\1/p')
  case $line in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":false}}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      count=$((count+1))
      if [ "$count" -eq 2 ] && [ "$RESET_NEW_DELAY" -gt 0 ]; then sleep "$RESET_NEW_DELAY"; fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sid-%d","configOptions":[{"id":"effort","name":"Reasoning Effort","category":"thought_level","type":"select","currentValue":"default","options":[{"value":"default","name":"Default"},{"value":"high","name":"High"}]}]}}\n' "$id" "$count"
      __INITIAL_NOTIFICATION__
      ;;
    *'"method":"session/set_config_option"'*)
      config_count=$((config_count+1))
      if [ "$config_count" -eq 2 ] && [ "$RESET_CONFIG_DELAY" -gt 0 ]; then sleep "$RESET_CONFIG_DELAY"; fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[]}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sid-%d","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}}}\n' "$count"
      if [ "$DELAY" -gt 0 ]; then sleep "$DELAY"; fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#
        .replace("__CAPTURE__", capture.to_str().expect("utf8 tmp path"))
        .replace("__DELAY__", &prompt_delay_secs.to_string())
        .replace("__RESET_NEW_DELAY__", &reset_new_delay_secs.to_string())
        .replace("__INITIAL_NOTIFICATION__", &initial_notification)
        .replace(
            "__RESET_CONFIG_DELAY__",
            &reset_config_delay_secs.to_string(),
        );
        std::fs::write(&script_path, script).expect("write fake agent script");
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake agent script");
        (script_path, capture)
    }

    #[cfg(unix)]
    async fn reset_with_deadline_for_test(
        client: &AcpClient,
        deadline: tokio::time::Instant,
    ) -> ResetSessionOutcome {
        let cmd_tx = client.cmd_tx.as_ref().expect("connection task running");
        let (respond_to, response) = oneshot::channel();
        cmd_tx
            .send(ClientCmd::ResetSession {
                text: "/new".into(),
                deadline,
                respond_to,
            })
            .await
            .expect("send reset command");
        tokio::time::timeout(std::time::Duration::from_secs(4), response)
            .await
            .expect("connection task must answer the reset")
            .expect("reset response channel open")
    }

    #[cfg(unix)]
    async fn assert_between_prompt_reset_refused(
        client: &mut AcpClient,
        capture: &std::path::Path,
    ) {
        let outcome = client.reset_session("/new").await.expect("reset_session");
        assert!(
            matches!(
                &outcome,
                ResetSessionOutcome::Failed { message }
                    if message.contains("agent work is still in flight")
            ),
            "between-prompt work must block a reset, got {outcome:?}"
        );

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let event = tokio::time::timeout_at(deadline, client.next_event())
                .await
                .expect("timed out waiting for reset refusal")
                .expect("event channel closed");
            match event {
                Event::PromptRejected { reason, text } => {
                    assert_eq!(reason, "agent_busy");
                    assert_eq!(text, "/new");
                    break;
                }
                Event::SessionCleared => {
                    panic!("a refused between-prompt reset must not emit SessionCleared")
                }
                _ => {}
            }
        }

        let wire = std::fs::read_to_string(capture).expect("read capture");
        assert_eq!(
            wire.matches("\"method\":\"session/new\"").count(),
            1,
            "a refused reset must not issue a second session/new;\nwire capture:\n{wire}"
        );
    }

    /// The deadline starts before enqueueing. If it has already expired
    /// when the connection loop dequeues the command, the stateful
    /// session/new must not be sent at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn expired_reset_deadline_does_not_send_session_new() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (script, capture) = write_reset_fake_agent(tmp.path(), 0, 0, 0);
        let config = reset_fake_spawn_config(&script, tmp.path());
        let client = AcpClient::spawn(config, AcpSessionId("reset-expired".into()))
            .await
            .expect("spawn scripted fake agent");

        let outcome = reset_with_deadline_for_test(&client, tokio::time::Instant::now()).await;
        assert!(
            matches!(outcome, ResetSessionOutcome::Failed { .. }),
            "an expired reset must fail, got {outcome:?}"
        );
        let wire = std::fs::read_to_string(&capture).expect("read capture");
        assert_eq!(
            wire.matches("\"method\":\"session/new\"").count(),
            1,
            "the expired command must not send a second session/new;\n{wire}"
        );

        let valid = reset_with_deadline_for_test(
            &client,
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .await;
        assert!(
            matches!(valid, ResetSessionOutcome::Reset { .. }),
            "the loop must continue after rejecting the expired command"
        );
        let _ = client.shutdown().await;
    }

    /// An ACP tool can remain open after its parent prompt has returned.
    /// Resetting while that tool is still producing events would attach its
    /// old-session updates to the fresh conversation.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn reset_between_prompts_with_open_tool_is_refused() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let initial_update = serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "between-prompt-tool",
            "title": "long-running tool",
            "kind": "other",
            "status": "in_progress",
            "rawInput": {},
        });
        let (script, capture) =
            write_reset_fake_agent_with_initial_update(tmp.path(), 0, 0, 0, Some(initial_update));
        let config = reset_fake_spawn_config(&script, tmp.path());
        let mut client = AcpClient::spawn(config, AcpSessionId("reset-open-tool".into()))
            .await
            .expect("spawn scripted fake agent");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let event = tokio::time::timeout_at(deadline, client.next_event())
                .await
                .expect("timed out waiting for open tool")
                .expect("event channel closed");
            if matches!(
                event,
                Event::ToolCallStarted { ref tool_call }
                    if tool_call.id == "between-prompt-tool"
            ) {
                break;
            }
        }

        assert_between_prompt_reset_refused(&mut client, &capture).await;
        let _ = client.shutdown().await;
    }

    /// A tracked async sub-agent outlives its parent prompt. Its tailer keeps
    /// publishing progress and completion, so a reset must wait until that
    /// tailer removes the agent from the between-prompt in-flight set.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn reset_between_prompts_with_background_agent_is_refused() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let transcript = tmp.path().join("background-agent.jsonl");
        std::fs::write(&transcript, "").expect("create background-agent transcript");
        let initial_update = serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "between-prompt-agent-tool",
            "_meta": {
                "claudeCode": {
                    "toolName": "Agent",
                    "toolResponse": {
                        "agentId": "between-prompt-agent",
                        "description": "keep working after the parent turn",
                        "prompt": "continue the delegated task",
                        "resolvedModel": "test-model",
                        "outputFile": transcript.to_str().expect("utf8 transcript path"),
                        "status": "async_launched",
                    },
                },
            },
        });
        let (script, capture) =
            write_reset_fake_agent_with_initial_update(tmp.path(), 0, 0, 0, Some(initial_update));
        let config = reset_fake_spawn_config(&script, tmp.path());
        let mut client = AcpClient::spawn(config, AcpSessionId("reset-background-agent".into()))
            .await
            .expect("spawn scripted fake agent");

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let event = tokio::time::timeout_at(deadline, client.next_event())
                .await
                .expect("timed out waiting for background-agent launch")
                .expect("event channel closed");
            if matches!(
                event,
                Event::BackgroundAgentLaunched { ref agent_id, .. }
                    if agent_id == "between-prompt-agent"
            ) {
                break;
            }
        }

        assert_between_prompt_reset_refused(&mut client, &capture).await;
        let _ = client.shutdown().await;
    }

    /// A `session/new` that never answers must release the real connection
    /// loop at the caller-created deadline. A second reset then proves the
    /// loop resumed draining commands instead of remaining parked on the
    /// abandoned request.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn reset_session_new_timeout_releases_the_connection_loop() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (script, _capture) = write_reset_fake_agent(tmp.path(), 0, 1, 0);
        let config = reset_fake_spawn_config(&script, tmp.path());
        let client = AcpClient::spawn(config, AcpSessionId("reset-timeout-new".into()))
            .await
            .expect("spawn scripted fake agent");

        let first = reset_with_deadline_for_test(
            &client,
            tokio::time::Instant::now() + std::time::Duration::from_millis(200),
        )
        .await;
        assert!(
            matches!(
                first,
                ResetSessionOutcome::Failed { ref message }
                    if message.contains("before the reset deadline")
            ),
            "the stalled session/new must fail at the inner deadline, got {first:?}"
        );

        let second = reset_with_deadline_for_test(
            &client,
            tokio::time::Instant::now() + std::time::Duration::from_secs(3),
        )
        .await;
        assert!(
            matches!(
                second,
                ResetSessionOutcome::Reset {
                    ref new_acp_session_id
                } if new_acp_session_id == "sid-3"
            ),
            "the connection loop must process a later reset, got {second:?}"
        );
        let _ = client.shutdown().await;
    }

    /// Once `session/new` returns, the reset is irreversible. A wedged
    /// best-effort config re-application must still release the command
    /// loop, adopt the fresh id, and report reset success; otherwise the
    /// client and runner would disagree about the live session.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn reset_config_timeout_commits_and_releases_the_connection_loop() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (script, capture) = write_reset_fake_agent(tmp.path(), 0, 0, 1);
        let mut config = reset_fake_spawn_config(&script, tmp.path());
        config.default_effort = Some("high".into());
        let client = AcpClient::spawn(config, AcpSessionId("reset-timeout-config".into()))
            .await
            .expect("spawn scripted fake agent");

        let first = reset_with_deadline_for_test(
            &client,
            tokio::time::Instant::now() + std::time::Duration::from_millis(200),
        )
        .await;
        assert!(
            matches!(
                first,
                ResetSessionOutcome::Reset {
                    ref new_acp_session_id
                } if new_acp_session_id == "sid-2"
            ),
            "a post-commit config timeout must preserve reset success, got {first:?}"
        );

        client
            .send_prompt("after config timeout", &[])
            .await
            .expect("queue follow-up prompt");
        let capture_deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let prompt_line = loop {
            let wire = std::fs::read_to_string(&capture).expect("read capture");
            if let Some(line) = wire
                .lines()
                .find(|line| line.contains("\"method\":\"session/prompt\""))
            {
                break line.to_string();
            }
            assert!(
                std::time::Instant::now() < capture_deadline,
                "connection loop did not process the follow-up prompt;\n{wire}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        assert!(
            prompt_line.contains("\"sessionId\":\"sid-2\""),
            "the follow-up prompt must use the committed fresh id: {prompt_line}"
        );
        let _ = client.shutdown().await;
    }

    /// #2979: a clear command on a profile with no native agent-side reset
    /// (codex-acp swallows `/new` as an unknown command) must drive a REAL
    /// conversation reset on the live worker: a second `session/new` that
    /// swaps the ACP session id, with `SessionCleared` +
    /// `SessionContextReset` + `AcpSessionAssigned` + a terminal `Stopped`
    /// emitted so the UI's boundary bookkeeping and context tracker follow.
    /// The raw alias text must NOT be forwarded as a `session/prompt`.
    /// (Baseline failure
    /// before the fix: the text-forward path issued exactly one
    /// `session/new` and a `session/prompt` carrying "/new".)
    #[cfg(unix)]
    #[tokio::test]
    async fn codex_clear_drives_fresh_session_new_on_live_worker() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (script, capture) = write_reset_fake_agent(tmp.path(), 0, 0, 0);
        let mut config = reset_fake_spawn_config(&script, tmp.path());
        // A configured default effort must survive the reset: spawn applies
        // it after its session/new, and the driven reset must re-apply it
        // after its own session/new (the fresh session starts on adapter
        // defaults). See the maintainer's open question 2 in #2979.
        config.default_effort = Some("high".into());
        let mut client = AcpClient::spawn(config, AcpSessionId("reset-2979".into()))
            .await
            .expect("spawn scripted fake agent");

        // What the service now does for a codex `/new` after publishing
        // UserPromptSent: drive the reset instead of forwarding the raw
        // text. The successful reset itself emits SessionCleared.
        let outcome = client.reset_session("/new").await.expect("reset_session");
        match &outcome {
            ResetSessionOutcome::Reset { new_acp_session_id } => {
                assert_eq!(
                    new_acp_session_id, "sid-2",
                    "the reset must swap onto the fresh session id"
                );
            }
            ResetSessionOutcome::Failed { message } => {
                panic!("reset must succeed against a live agent: {message}")
            }
        }

        // The reset's ordered boundary events: clear the transcript, reset
        // bookkeeping, assign the fresh id, then end the synthetic turn.
        let mut events = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let ev = tokio::time::timeout_at(deadline, client.next_event())
                .await
                .expect("timed out waiting for reset events")
                .expect("event channel closed");
            let stop = matches!(&ev, Event::Stopped { .. });
            events.push(ev);
            if stop {
                break;
            }
        }
        let cleared_pos = events
            .iter()
            .position(|e| matches!(e, Event::SessionCleared))
            .expect("a successful driven reset must emit SessionCleared");
        let reset_pos = events
            .iter()
            .position(|e| matches!(e, Event::SessionContextReset { .. }))
            .expect("reset must emit SessionContextReset");
        let assigned_pos = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    Event::AcpSessionAssigned { acp_session_id } if acp_session_id == "sid-2"
                )
            })
            .expect("reset must emit AcpSessionAssigned with the fresh id");
        assert!(
            cleared_pos < reset_pos && reset_pos < assigned_pos,
            "SessionCleared must precede SessionContextReset, which must \
             precede AcpSessionAssigned, got {events:?}"
        );
        assert!(
            matches!(events.last(), Some(Event::Stopped { reason }) if reason == "session_reset"),
            "the reset must end the clear turn with Stopped(session_reset), got {events:?}"
        );

        // A follow-up prompt must address the NEW session id.
        client.send_prompt("hello", &[]).await.expect("send prompt");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let ev = tokio::time::timeout_at(deadline, client.next_event())
                .await
                .expect("timed out waiting for follow-up turn to end")
                .expect("event channel closed");
            if matches!(&ev, Event::Stopped { .. }) {
                break;
            }
        }

        let wire = std::fs::read_to_string(&capture).expect("read capture");
        let new_count = wire.matches("\"method\":\"session/new\"").count();
        assert_eq!(
            new_count, 2,
            "a codex clear must issue a fresh session/new on the live worker \
             (got {new_count} session/new request(s));\nwire capture:\n{wire}"
        );
        assert!(
            !wire.contains("\"text\":\"/new\""),
            "the raw clear alias must not be forwarded as a session/prompt \
             (codex-acp would swallow it as an unknown command);\nwire capture:\n{wire}"
        );
        assert!(
            wire.contains("\"sessionId\":\"sid-2\""),
            "the follow-up prompt must address the swapped session id;\nwire capture:\n{wire}"
        );
        let effort_count = wire
            .matches("\"method\":\"session/set_config_option\"")
            .count();
        assert_eq!(
            effort_count, 2,
            "the configured default effort must be re-applied after the \
             reset's session/new, mirroring spawn (got {effort_count} \
             session/set_config_option request(s));\nwire capture:\n{wire}"
        );
        assert_eq!(
            wire.matches("\"value\":\"high\"").count(),
            2,
            "both applications must carry the configured effort value;\nwire capture:\n{wire}"
        );
        let _ = client.shutdown().await;
    }

    /// #2979: a reset requested while a `session/prompt` is in flight must
    /// be refused because resetting under the turn would orphan it on the old
    /// session id. The refusal must mirror the busy-Prompt path's
    /// `PromptRejected` so a raw API caller gets a terminal frame (retry
    /// pill) under the persisted UserPromptSent, not just an HTTP error.
    /// The success-only `SessionCleared` boundary must remain absent.
    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn reset_during_in_flight_prompt_is_refused_with_prompt_rejected() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // 3s prompt delay: long enough to land the reset mid-turn, short
        // enough to keep the test snappy.
        let (script, capture) = write_reset_fake_agent(tmp.path(), 3, 0, 0);
        let config = reset_fake_spawn_config(&script, tmp.path());
        let mut client = AcpClient::spawn(config, AcpSessionId("reset-busy-2979".into()))
            .await
            .expect("spawn scripted fake agent");

        client.send_prompt("hello", &[]).await.expect("send prompt");
        // The fake emits an agent_message_chunk as soon as it receives the
        // prompt; seeing it proves the connection task is inside the
        // in-flight select, so the reset below cannot race into the idle
        // loop and spuriously succeed.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let ev = tokio::time::timeout_at(deadline, client.next_event())
                .await
                .expect("timed out waiting for the turn to start")
                .expect("event channel closed");
            if matches!(&ev, Event::AgentMessageChunk { .. }) {
                break;
            }
        }

        let outcome = client.reset_session("/new").await.expect("reset_session");
        assert!(
            matches!(&outcome, ResetSessionOutcome::Failed { message } if message.contains("turn is in flight")),
            "a mid-turn reset must be refused, got {outcome:?}"
        );

        // The refusal emits PromptRejected(agent_busy) carrying the user's
        // clear invocation, then the in-flight turn still ends normally.
        let mut saw_rejected = false;
        let mut saw_cleared = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let ev = tokio::time::timeout_at(deadline, client.next_event())
                .await
                .expect("timed out waiting for PromptRejected + Stopped")
                .expect("event channel closed");
            match &ev {
                Event::PromptRejected { reason, text } => {
                    assert_eq!(reason, "agent_busy");
                    assert_eq!(text, "/new", "the retry pill needs the typed alias");
                    saw_rejected = true;
                }
                Event::SessionCleared => saw_cleared = true,
                Event::Stopped { .. } => break,
                _ => {}
            }
        }
        assert!(
            saw_rejected,
            "the mid-turn refusal must emit PromptRejected before the turn's Stopped"
        );
        assert!(
            !saw_cleared,
            "a busy reset must not emit the success-only SessionCleared boundary"
        );

        let wire = std::fs::read_to_string(&capture).expect("read capture");
        assert_eq!(
            wire.matches("\"method\":\"session/new\"").count(),
            1,
            "a refused reset must not have issued a second session/new;\nwire capture:\n{wire}"
        );
        let _ = client.shutdown().await;
    }
}
