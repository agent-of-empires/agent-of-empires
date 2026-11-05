//! Driven conversation reset: its deadlines and the outcomes callers see.

/// Outer cap on a driven reset's `session/new` round trip (#2979).
pub(crate) const SESSION_RESET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The connection task's deadline for the whole reset sequence. Shorter than
/// `SESSION_RESET_TIMEOUT` so the task reports the specific failure and
/// resumes draining commands before the caller gives up.
pub(crate) const SESSION_RESET_IN_TASK_TIMEOUT: std::time::Duration =
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
    // `send_request` enqueues synchronously, so the request is built lazily:
    // a command that expired in the queue must send nothing.
    if tokio::time::Instant::now() >= deadline {
        return Err(ResetRequestError::TimedOut);
    }
    match tokio::time::timeout_at(deadline, request()).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => Err(ResetRequestError::Acp(error)),
        Err(_) => Err(ResetRequestError::TimedOut),
    }
}

#[derive(Debug)]
pub enum ResetSessionOutcome {
    /// The connection swapped onto a fresh ACP session.
    Reset { new_acp_session_id: String },
    /// Caller-visible failure; an issued native reset may still settle or already be applied.
    Failed { message: String },
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::acp::acp_client::commands::ClientCmd;
    use crate::acp::acp_client::test_helpers::reset_fake_spawn_config;
    use crate::acp::state::Event;
    use crate::acp::supervisor::test_support::managed::{ManagedClient as AcpClient, ManagedScope};
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::sync::oneshot;

    /// A scripted stdio agent (#2979): mints `sid-1`, `sid-2`, ... per
    /// `session/new` (each with a `thought_level` option), acks config
    /// requests, and answers each prompt with a chunk then a response. Hold
    /// flags gate the prompt response, the second `session/new`, or the second
    /// config request until the matching release file exists. Every request is
    /// captured before its gate.
    #[derive(Default)]
    struct FakeAgent {
        hold_prompt: bool,
        hold_reset_new: bool,
        hold_reset_config: bool,
    }

    impl FakeAgent {
        fn write(self, dir: &Path) -> (PathBuf, PathBuf) {
            let capture = dir.join("capture.ndjson");
            let script_path = dir.join("fake-reset-agent.sh");
            let script = r#"#!/bin/sh
CAPTURE=__CAPTURE__
HOLD=__HOLD__
RESET_NEW_HOLD=__RESET_NEW_HOLD__
RESET_CONFIG_HOLD=__RESET_CONFIG_HOLD__
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
      if [ "$count" -eq 2 ] && [ "$RESET_NEW_HOLD" = true ]; then
        while [ -d "__DIR__" ] && [ ! -f "$CAPTURE.new-release" ]; do sleep 0.01; done
      fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sid-%d","configOptions":[{"id":"effort","name":"Reasoning Effort","category":"thought_level","type":"select","currentValue":"default","options":[{"value":"default","name":"Default"},{"value":"high","name":"High"}]}]}}\n' "$id" "$count"
      ;;
    *'"method":"session/set_config_option"'*)
      config_count=$((config_count+1))
      if [ "$config_count" -eq 2 ] && [ "$RESET_CONFIG_HOLD" = true ]; then
        while [ -d "__DIR__" ] && [ ! -f "$CAPTURE.config-release" ]; do sleep 0.01; done
      fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[]}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sid-%d","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"working"}}}}\n' "$count"
      if [ "$HOLD" = true ]; then
        while [ -d "__DIR__" ] && [ ! -f "$CAPTURE.prompt-release" ]; do sleep 0.01; done
      fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#
            .replace("__CAPTURE__", capture.to_str().unwrap())
            .replace("__DIR__", dir.to_str().unwrap())
            .replace("__HOLD__", &self.hold_prompt.to_string())
            .replace("__RESET_NEW_HOLD__", &self.hold_reset_new.to_string())
            .replace("__RESET_CONFIG_HOLD__", &self.hold_reset_config.to_string());
            std::fs::write(&script_path, script).unwrap();
            (script_path, capture)
        }

        async fn spawn(
            self,
            scope: &mut ManagedScope,
            dir: &Path,
            name: &str,
            effort: Option<&str>,
        ) -> (AcpClient, PathBuf) {
            let (script, capture) = self.write(dir);
            let mut config = reset_fake_spawn_config(&script, dir);
            config.default_effort = effort.map(Into::into);
            let client = scope
                .spawn(config, name)
                .await
                .expect("spawn scripted fake agent");
            client.ready().await;
            (client, capture)
        }
    }

    fn requests(capture: &Path, method: &str) -> usize {
        let wire = std::fs::read_to_string(capture).unwrap();
        wire.matches(&format!("\"method\":\"{method}\"")).count()
    }

    async fn wait_for_captured_requests(capture: &Path, method: &str, count: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while requests(capture, method) < count {
            assert!(
                std::time::Instant::now() < deadline,
                "adapter did not receive {method}"
            );
            // Keeps paused time stationary until native I/O reaches the RPC.
            tokio::task::yield_now().await;
        }
    }

    async fn wait_for_captured_replies(capture: &Path, id: &str, count: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let wire = std::fs::read_to_string(capture).unwrap();
            let replies = wire
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .filter(|reply| reply["id"] == id && reply.get("method").is_none())
                .count();
            if replies >= count {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Native did not receive reply {id}: {wire}"
            );
            tokio::task::yield_now().await;
        }
    }

    /// Collect events until one matches `done`, returning all of them.
    async fn events_until(client: &mut AcpClient, done: impl Fn(&Event) -> bool) -> Vec<Event> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut events = Vec::new();
        loop {
            let event = tokio::time::timeout_at(deadline, client.next_event())
                .await
                .expect("timed out waiting for events")
                .expect("event channel closed");
            let finished = done(&event);
            events.push(event);
            if finished {
                return events;
            }
        }
    }

    async fn reset_with_deadline(
        client: &AcpClient,
        deadline: tokio::time::Instant,
    ) -> ResetSessionOutcome {
        let proposal = client.prepare_reset(deadline).await;
        reset_with_proposal(client, proposal, deadline).await
    }

    async fn reset_with_proposal(
        client: &AcpClient,
        proposal: std::sync::Arc<crate::acp::supervisor::LiveFreshProposal>,
        deadline: tokio::time::Instant,
    ) -> ResetSessionOutcome {
        let (respond_to, response) = oneshot::channel();
        client
            .cmd_tx
            .as_ref()
            .unwrap()
            .send(
                client
                    .queue_command(
                        ClientCmd::ResetSession {
                            text: "/new".into(),
                            deadline,
                            proposal,
                            respond_to,
                        },
                        Some(crate::acp::establishment::NativeOperationCorrelation::new(
                            None,
                        )),
                    )
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
        tokio::time::timeout_at(deadline + Duration::from_secs(4), response)
            .await
            .expect("connection task must answer the reset")
            .unwrap()
    }

    #[tokio::test]
    async fn foreign_native_observations_do_not_select_current_identity() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { let tmp = scope.tempdir();
    let (script, capture) = FakeAgent::default().write(tmp.path());
    let source = std::fs::read_to_string(&script).unwrap();
    let foreign = r#"      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"foreign","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"FOREIGN"}}}}\n'
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"foreign","update":{"sessionUpdate":"usage_update","used":9876,"size":10000}}}\n'
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"foreign","update":{"sessionUpdate":"current_mode_update","currentModeId":"foreign-mode"}}}\n'
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"foreign","update":{"sessionUpdate":"compaction_update","compactionId":"foreign-compaction","status":"in_progress"}}}\n'
          if [ "$count" -eq 2 ]; then
    printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sid-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"LATE"}}}}\n'
          fi
          printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sid-%d","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"OWN-CONTROL"}}}}\n' "$count"
    "#;
    std::fs::write(
        &script,
        source.replace(
            "      if [ \"$HOLD\" = true ]",
            &format!("{foreign}      if [ \"$HOLD\" = true ]"),
        ),
    )
    .unwrap();
    let mut client = scope.spawn(reset_fake_spawn_config(&script, tmp.path()), "native-guard")
    .await
    .unwrap();
    client.ready().await;
    let mut observed = Vec::new();
    for generation in 1..=2 {
        if generation == 2 {
            assert!(matches!(
                client.reset_session("/new").await.unwrap(),
                ResetSessionOutcome::Reset { .. }
            ));
            loop {
                let event = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    client.next_event(),
                )
                .await
                .unwrap()
                .unwrap();
                if matches!(event, Event::Stopped { .. }) {
                    break;
                }
            }
        }
        client.send_prompt("hello", &[]).await.unwrap();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(10), client.next_event())
                    .await
                    .unwrap()
                    .unwrap();
            match event {
                Event::AgentMessageChunk { text, .. } => observed.push(text),
                Event::Stopped { .. } => break,
                _ => {}
            }
        }
    }
    client.shutdown().await.unwrap();
    let wire = std::fs::read_to_string(capture).unwrap();
    let requests: Vec<serde_json::Value> = wire
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let prompt_ids: Vec<&str> = requests
        .iter()
        .filter(|request| request["method"] == "session/prompt")
        .map(|request| request["params"]["sessionId"].as_str().unwrap())
        .collect();
    assert_eq!(prompt_ids, ["sid-1", "sid-2"]);
    assert!(!requests
        .iter()
        .any(|request| request["method"] == "session/load"));
    assert_eq!(observed.iter().filter(|text| text.as_str() == "OWN-CONTROL").count(), 2);
    assert!(observed.iter().any(|text| text == "FOREIGN"));
    assert!(observed.iter().any(|text| text == "LATE"));
    let history = scope.state.acp_event_store.replay_from(&client.id, 0).unwrap();
    for stored in history {
        if stored.metadata.reported_session_id.as_ref().is_some_and(|id| id.0.as_ref() == "foreign")
            || matches!(stored.event.as_ref(), Event::AgentMessageChunk { text, .. } if text == "LATE") {
            assert_eq!(stored.metadata.scope, crate::acp::event_sender::EventScope::HistoryOnly);
            assert!(stored.metadata.native_origin.is_some());
            assert!(stored.metadata.reported_session_id.is_some());
        }
    } })).await;
    }

    fn write_early_identity_agent(dir: &std::path::Path) -> std::path::PathBuf {
        let script = dir.join("early-identity.sh");
        std::fs::write(&script, r#"#!/bin/sh
count=0
sid=''
update() {
  printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"%s"}}}}\n' "$1" "$2"
}
while IFS= read -r line; do
  printf '%s\n' "$line" >> "__CAPTURE__"
  id=$(printf '%s' "$line" | sed -En 's/.*"id":("[^"]*"|[0-9]+).*/\1/p')
  case $line in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"fork":{}}}}}\n' "$id" ;;
    *'"method":"session/new"'*)
      count=$((count+1)); sid="sid-$count"
      update "$sid" "early-$count"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$sid" ;;
    *'"method":"session/fork"'*)
      sid=child
      update "$sid" early-fork
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"%s"}}\n' "$id" "$sid" ;;
    *'"method":"session/load"'*)
      sid=$(printf '%s' "$line" | sed -En 's/.*"sessionId":"([^"]+)".*/\1/p')
      update "$sid" history
      printf '{"jsonrpc":"2.0","id":%s,"result":{}}\n' "$id" ;;
    *'"method":"session/prompt"'*)
      update "$sid" live
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id" ;;
  esac
done
"#.replace("__CAPTURE__", &dir.join("identity-capture.ndjson").to_string_lossy())).unwrap();
        script
    }

    async fn identity_events_until_stop(client: &mut AcpClient) -> Vec<Event> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut events = Vec::new();
        loop {
            let event = tokio::time::timeout_at(deadline, client.next_event())
                .await
                .unwrap()
                .unwrap();
            let stopped = matches!(event, Event::Stopped { .. });
            events.push(event);
            if stopped {
                return events;
            }
        }
    }

    fn identity_texts(events: &[Event]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::AgentMessageChunk { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn native_identity_preserves_pre_response_new_fork_and_load_updates() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| {
            Box::pin(async move {
                for (operation, imported, expected) in [
                    ("new", false, vec!["early-1", "live"]),
                    ("fork", false, vec!["early-fork", "live"]),
                    ("load", true, vec!["history", "live"]),
                    ("load", false, vec!["live"]),
                ] {
                    let temp = scope.tempdir();
                    let script = write_early_identity_agent(temp.path());
                    let mut config = reset_fake_spawn_config(&script, temp.path());
                    if operation == "fork" {
                        config.agent_key = "claude".into();
                        config.tool = "claude".into();
                        config.fork_from = Some("parent".into());
                    }
                    if operation == "load" {
                        config.stored_acp_session_id = Some("stored".into());
                    }
                    config.seed_history_replay = imported;
                    let mut client = scope
                        .spawn(config, format!("early-{operation}"))
                        .await
                        .unwrap();
                    client.ready().await;
                    client.send_prompt("hello", &[]).await.unwrap();
                    let events = identity_events_until_stop(&mut client).await;
                    client.shutdown().await.unwrap();
                    assert_eq!(
                        identity_texts(&events),
                        expected,
                        "operation={operation} imported={imported}"
                    );
                    scope.finish_conversation_case().await;
                }
            })
        })
        .await;
    }

    #[tokio::test]
    async fn native_identity_preserves_reset_prefix_once_in_default_clear_projection() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| {
            Box::pin(async move {
                let temp = scope.tempdir();
                let script = write_early_identity_agent(temp.path());
                let source = std::fs::read_to_string(&script).unwrap().replace(
                    "      update \"$sid\" \"early-$count\"",
                    r#"      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"%s","update":{"sessionUpdate":"plan","entries":[{"content":"native-plan-%s","priority":"medium","status":"pending"}]}}}\n' "$sid" "$count"
      update "$sid" "early-$count""#,
                );
                std::fs::write(&script, source).unwrap();
                let mut client = scope
                    .spawn(reset_fake_spawn_config(&script, temp.path()), "early-reset")
                    .await
                    .unwrap();
                client.ready().await;
                let owner = client.connected_runner().unwrap();
                let lease = client.lease.clone();
                let generation = scope.state.instances.read().await.iter()
                    .find(|row| row.id == client.id).unwrap().lifecycle_generation;
                let previous_session = client.selected_native_session();
                assert!(owner.birth_fingerprint.is_some(), "the real physical owner has a captured birth");
                client.send_prompt("first", &[]).await.unwrap();
                identity_events_until_stop(&mut client).await;
                let before = scope.state.session_service.fold_control_state(&client.id).await.unwrap();
                let old_plan = before.current_plan.as_ref().expect("the first real Native New published its plan");
                assert_eq!(old_plan.steps.iter().map(|step| step.title.as_str()).collect::<Vec<_>>(),
                    ["native-plan-1"]);
                let outcome = client.reset_session("/new").await.unwrap();
                let ResetSessionOutcome::Reset { new_acp_session_id } = outcome else {
                    panic!("genuine Native New must settle: {outcome:?}");
                };
                let events = identity_events_until_stop(&mut client).await;
                assert_eq!(identity_texts(&events), ["early-2"]);
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, Event::SessionCleared))
                        .count(),
                    1
                );
                assert!(events.iter().any(|event| matches!(event, Event::PlanUpdated { plan }
                    if plan.steps.iter().any(|step| step.title == "native-plan-2"))));
                let after_clear = scope.state.session_service.fold_control_state(&client.id).await.unwrap();
                let new_plan = after_clear.current_plan.as_ref()
                    .expect("the late certified clear must retain the genuine New pre-response plan");
                assert_eq!(new_plan.steps.iter().map(|step| step.title.as_str()).collect::<Vec<_>>(),
                    ["native-plan-2"], "the canonical reduced state retains only the new Native plan");
                assert_eq!(client.selected_native_session(), new_acp_session_id);
                assert_eq!(client.connected_runner(), Some(owner));
                assert_eq!(client.lease, lease);
                assert_eq!(scope.state.instances.read().await.iter()
                    .find(|row| row.id == client.id).unwrap().lifecycle_generation, generation);
                client
                    .send_prompt("after the selected reset", &[])
                    .await
                    .unwrap();
                let after = identity_events_until_stop(&mut client).await;
                assert_eq!(identity_texts(&after), ["live"]);
                let mut transcript = crate::acp::transcript::TranscriptModel::new();
                for stored in scope
                    .state
                    .acp_event_store
                    .replay_from(&client.id, 0)
                    .unwrap()
                {
                    transcript.apply_scoped_event(
                        stored.seq,
                        stored.event.as_ref(),
                        &stored.metadata,
                    );
                }
                assert_eq!(
                    transcript
                        .rows()
                        .iter()
                        .map(|row| row.text.matches("early-2").count())
                        .sum::<usize>(),
                    1
                );
                let mut visible = transcript.rows().to_vec();
                let clear = visible.iter().find(|row| {
                    row.kind == crate::acp::transcript::TranscriptRowKind::SessionCleared
                }).expect("the actual late clear has a transcript row");
                let cut = clear.clear_after_seq.expect("Fresh clear carries its certified original boundary");
                let clear_seq = clear.seq;
                let prefix = visible.iter().find(|row| row.text.contains("early-2"))
                    .expect("the actual New prefix retains its creation position");
                assert!(cut < prefix.seq && prefix.seq < clear_seq,
                    "the genuine prefix physically precedes the late clear but follows its certified cut");
                crate::acp::transcript::project_transcript_clear_boundaries(&mut visible);
                let clear_index = visible.iter().rposition(|row| {
                    row.kind == crate::acp::transcript::TranscriptRowKind::SessionCleared
                }).unwrap();
                let default_visible = &visible[clear_index + 1..];
                assert_eq!(default_visible.iter().map(|row| row.text.matches("early-2").count())
                    .sum::<usize>(), 1, "the default clear view must show the New prefix once");
                assert!(!default_visible.iter().any(|row| row.text.contains("early-1") || row.text == "first"));
                assert!(default_visible.iter().any(|row| row.text.contains("live")));
                let wire =
                    std::fs::read_to_string(temp.path().join("identity-capture.ndjson")).unwrap();
                let requests: Vec<serde_json::Value> = wire
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(
                    requests
                        .iter()
                        .filter(|request| request["method"] == "session/new")
                        .count(),
                    2
                );
                let prompt = requests
                    .iter()
                    .rev()
                    .find(|request| request["method"] == "session/prompt")
                    .unwrap();
                assert_eq!(prompt["params"]["sessionId"], new_acp_session_id);
                let prompts: Vec<_> = requests.iter()
                    .filter(|request| request["method"] == "session/prompt").collect();
                assert_eq!(prompts.len(), 2, "only the two explicit user inputs reach Native");
                assert_eq!(prompts[0]["params"]["sessionId"], previous_session);
                assert_eq!(prompts[1]["params"]["sessionId"], new_acp_session_id);
                client.shutdown().await.unwrap();
            })
        })
        .await;
    }

    #[tokio::test]
    async fn native_identity_rejects_foreign_callbacks_without_side_effects() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { let temp = scope.tempdir();
    let (script, capture) = FakeAgent::default().write(temp.path());
    let denied_file = temp.path().join("foreign-write");
    let denied_command = temp.path().join("foreign-terminal");
    let own_file = temp.path().join("own-write");
    let mut callbacks = Vec::new();
    for (method, params) in [
        (
            "fs/read_text_file",
            serde_json::json!({"sessionId":"foreign","path":script}),
        ),
        (
            "fs/write_text_file",
            serde_json::json!({"sessionId":"foreign","path":denied_file,"content":"foreign"}),
        ),
        (
            "terminal/create",
            serde_json::json!({"sessionId":"foreign","command":"/bin/sh","args":["-c",format!("touch '{}'", denied_command.display())]}),
        ),
        (
            "terminal/output",
            serde_json::json!({"sessionId":"foreign","terminalId":"foreign-terminal"}),
        ),
        (
            "terminal/wait_for_exit",
            serde_json::json!({"sessionId":"foreign","terminalId":"foreign-terminal"}),
        ),
        (
            "terminal/kill",
            serde_json::json!({"sessionId":"foreign","terminalId":"foreign-terminal"}),
        ),
        (
            "terminal/release",
            serde_json::json!({"sessionId":"foreign","terminalId":"foreign-terminal"}),
        ),
        (
            "session/request_permission",
            serde_json::json!({"sessionId":"foreign","toolCall":{"toolCallId":"foreign-tool","title":"Foreign request"},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]}),
        ),
        (
            "elicitation/create",
            serde_json::json!({"sessionId":"foreign","mode":"form","message":"Foreign question","requestedSchema":{"type":"object","properties":{}}}),
        ),
    ] {
        callbacks.push(
            serde_json::json!({"jsonrpc":"2.0","id":method,"method":method,"params":params}),
        );
    }
    callbacks.push(serde_json::json!({"jsonrpc":"2.0","id":"own","method":"fs/write_text_file","params":{"sessionId":"sid-1","path":own_file,"content":"own"}}));
    let injected = callbacks
        .iter()
        .map(|request| {
            format!(
                "      printf '%s\\n' '{}'\n",
                request.to_string().replace('\'', "'\"'\"'")
            )
        })
        .collect::<String>();
    let source = std::fs::read_to_string(&script).unwrap();
    std::fs::write(
        &script,
        source.replace(
            r#"    *'"method":"session/prompt"'*)"#,
            &format!("    *'\"method\":\"session/prompt\"'*)\n{injected}"),
        ),
    )
    .unwrap();
    let mut client = scope.spawn(reset_fake_spawn_config(&script, temp.path()), "callback-guard")
    .await
    .unwrap();
    client.ready().await;
    client.send_prompt("exercise callbacks", &[]).await.unwrap();
    let events = identity_events_until_stop(&mut client).await;
    for request in &callbacks {
        wait_for_captured_replies(&capture, request["id"].as_str().unwrap(), 1).await;
    }
    client.shutdown().await.unwrap();
    assert!(!denied_file.exists());
    assert!(!denied_command.exists());
    assert_eq!(std::fs::read_to_string(own_file).unwrap(), "own");
    assert!(!events
        .iter()
        .any(|event| matches!(event, Event::ApprovalRequested { .. } | Event::ElicitationRequested { .. })));
    let wire = std::fs::read_to_string(capture).unwrap();
    let replies: Vec<serde_json::Value> = wire
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for request in &callbacks[..callbacks.len() - 1] {
        assert!(
            replies
                .iter()
                .any(|reply| reply["id"] == request["id"] && reply["error"]["code"] == -32602),
            "missing identity denial for {request}: {wire}"
        );
    }
    assert!(
        replies
            .iter()
            .any(|reply| reply["id"] == "own" && reply.get("result").is_some()),
        "own callback failed: {wire}"
    ); })).await;
    }

    #[tokio::test]
    async fn native_identity_reset_uses_original_result_not_buffered_update_ids() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { for same_id in [false, true] {
        let temp = scope.tempdir();
        let script = write_early_identity_agent(temp.path());
        let reply = if same_id {
            r#"{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sid-1"}}"#
        } else {
            r#"{"jsonrpc":"2.0","id":%s,"error":{"code":-32603,"message":"reset rejected"}}"#
        };
        let source = std::fs::read_to_string(&script).unwrap().replace(
            "      update \"$sid\" \"early-$count\"",
            &format!("      if [ \"$count\" -eq 2 ]; then\n        update sid-1 retained\n        update sid-2 rejected\n        printf '{reply}\\n' \"$id\"\n        sid=sid-1\n        continue\n      fi\n      update \"$sid\" \"early-$count\""),
        );
        std::fs::write(&script, source).unwrap();
        let mut client = scope.spawn(reset_fake_spawn_config(&script, temp.path()), format!("reset-rollback-{same_id}"))
        .await
        .unwrap();
        client.ready().await;
        client.send_prompt("before", &[]).await.unwrap();
        identity_events_until_stop(&mut client).await;
        let outcome = client.reset_session("/new").await.unwrap();
        assert_eq!(matches!(&outcome, ResetSessionOutcome::Reset { .. }), same_id, "{outcome:?}");
        let events = identity_events_until_stop(&mut client).await;
        assert_eq!(identity_texts(&events), ["retained", "rejected"]);
        if same_id {
            assert!(events.iter().any(|event| matches!(event, Event::SessionCleared)));
            assert_eq!(client.selected_native_session(), "sid-1");
            client.send_prompt("after the genuine New result", &[]).await.unwrap();
            assert_eq!(identity_texts(&identity_events_until_stop(&mut client).await), ["live"]);
        } else {
            assert!(matches!(outcome, ResetSessionOutcome::Failed { .. }));
            assert!(!events.iter().any(|event| matches!(event,
                Event::SessionCleared | Event::AcpSessionAssigned { .. }
            )));
            assert!(client.send_prompt("after failed reset", &[]).await.is_err());
        }
        for stored in scope.state.acp_event_store.replay_from(&client.id, 0).unwrap() {
            if matches!(stored.event.as_ref(), Event::AgentMessageChunk { text, .. } if text == "retained" || text == "rejected") {
                assert_eq!(stored.metadata.scope, crate::acp::event_sender::EventScope::HistoryOnly);
            }
        }
        let wire = std::fs::read_to_string(temp.path().join("identity-capture.ndjson")).unwrap();
        assert_eq!(wire.matches("\"method\":\"session/new\"").count(), 2);
        assert_eq!(wire.matches("\"method\":\"session/prompt\"").count(), if same_id { 2 } else { 1 });
        client.shutdown().await.unwrap();
        scope.finish_conversation_case().await;
    } })).await;
    }

    #[tokio::test]
    async fn native_identity_admits_immediate_post_response_callbacks_before_config() {
        async fn assert_owned_callback(capture: &Path, written: &Path, sid: &str) {
            let id = format!("callback-{sid}");
            wait_for_captured_replies(capture, &id, 1).await;
            let wire = std::fs::read_to_string(capture).unwrap();
            let replies: Vec<serde_json::Value> = wire
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .filter(|reply: &serde_json::Value| {
                    reply["id"] == id && reply.get("method").is_none()
                })
                .collect();
            assert_eq!(replies.len(), 1, "{wire}");
            assert_eq!(replies[0]["result"], serde_json::json!({}), "{wire}");
            assert!(replies[0].get("error").is_none(), "{wire}");
            assert_eq!(std::fs::read_to_string(written).unwrap(), sid);
        }

        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { for fork in [false, true] {
        let temp = scope.tempdir();
        let script = write_early_identity_agent(temp.path());
        let written = temp.path().join("owned-callback");
        let response = "      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{\"sessionId\":\"%s\"}}\\n' \"$id\" \"$sid\" ;;";
        let replacement = format!(
            r#"      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"sessionId":"%s","configOptions":[{{"id":"effort","name":"Effort","category":"thought_level","type":"select","currentValue":"high","options":[{{"value":"high","name":"High"}}]}},{{"id":"mode","name":"Mode","category":"mode","type":"select","currentValue":"code","options":[{{"value":"code","name":"Code"}}]}}]}}}}\n' "$id" "$sid"
          printf '{{"jsonrpc":"2.0","id":"callback-%s","method":"fs/write_text_file","params":{{"sessionId":"%s","path":"{}","content":"%s"}}}}\n' "$sid" "$sid" "$sid"
          callback_pending="callback-$sid" ;;"#,
            written.display()
        );
        let source = std::fs::read_to_string(&script)
            .unwrap()
            .replace(response, &replacement)
            // Defaults settle only after Native consumes its reverse reply.
            .replace(
                "    *'\"method\":\"session/prompt\"'*)",
                r#"    *'"method":"session/set_config_option"'*)
      if [ -n "$callback_pending" ]; then
        pending_config=$id
      else
        printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[]}}\n' "$id"
      fi ;;
    *'"id":"callback-'*)
      [ "$id" = "\"$callback_pending\"" ] || exit 1
      callback_pending=''
      if [ -n "$pending_config" ]; then
        printf '{"jsonrpc":"2.0","id":%s,"result":{"configOptions":[]}}\n' "$pending_config"
        pending_config=''
      fi ;;
    *'"method":"session/prompt"'*)"#,
            )
            .replace("sid=\"sid-$count\"", "sid=\"native-$$-$count\"");
        std::fs::write(&script, source).unwrap();
        let mut config = reset_fake_spawn_config(&script, temp.path());
        config.default_mode = Some("code".into());
        config.default_effort = Some("high".into());
        if fork {
            config.agent_key = "claude".into();
            config.tool = "claude".into();
        }
        let capture = temp.path().join("identity-capture.ndjson");
        let mut client = if fork {
            let original = scope.spawn(config.clone(), "post-response-original").await.unwrap();
            original.ready().await;
            // Ready does not prove Native consumed its reverse reply.
            assert_owned_callback(&capture, &written, &original.selected_native_session()).await;
            scope.fork_after(original, config, "post-response-true").await
        } else {
            scope.spawn(config, "post-response-false").await.unwrap()
        };
        client.ready().await;
        client.send_prompt("before", &[]).await.unwrap();
        identity_events_until_stop(&mut client).await;
        let selected = client.selected_native_session();
        assert_owned_callback(&capture, &written, &selected).await;
        assert!(matches!(
            client.reset_session("/new").await.unwrap(),
            ResetSessionOutcome::Reset { .. }
        ));
        identity_events_until_stop(&mut client).await;
        let fresh = client.selected_native_session();
        assert_ne!(fresh, selected);
        assert_owned_callback(&capture, &written, &fresh).await;
        assert_eq!(requests(&capture, "session/prompt"), 1);
        client.send_prompt("after", &[]).await.unwrap();
        identity_events_until_stop(&mut client).await;
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            client.selected_native_session()
        );
        let wire = std::fs::read_to_string(&capture).unwrap();
        let prompts: Vec<serde_json::Value> = wire
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .filter(|request: &serde_json::Value| request["method"] == "session/prompt")
            .collect();
        assert_eq!(
            prompts.iter().map(|request| request["params"]["prompt"][0]["text"].as_str().unwrap()).collect::<Vec<_>>(),
            ["before", "after"]
        );
        client.shutdown().await.unwrap();
        scope.finish_conversation_case().await;
    } })).await;
    }

    #[tokio::test]
    async fn native_identity_pending_buffer_spans_runner_queue_and_fails_closed_on_overflow() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {
    // Admit the entire ordinary runner backlog. Exceed even the reserved
    // bootstrap/replay slots to prove overflow cannot publish partial history.
    let cap = crate::acp::control_protocol::MAX_CONTROL_QUEUE_FRAMES;
    let overflow = cap
        + crate::acp::control_protocol::CONTROL_BOOTSTRAP_RESERVED_FRAMES
        + crate::acp::control_protocol::CONTROL_REPLAY_RESERVED_FRAMES
        + 1;
    for count in [cap, overflow] {
        let temp = scope.tempdir();
        let script = write_early_identity_agent(temp.path());
        let source = std::fs::read_to_string(&script)
            .unwrap()
            .replace(
                "      update \"$sid\" \"early-$count\"",
                &format!("      i=0\n      while [ \"$i\" -lt {count} ]; do update \"$sid\" \"item-$i\"; i=$((i+1)); done"),
            );
        std::fs::write(&script, source).unwrap();
        let mut client = scope.spawn(reset_fake_spawn_config(&script, temp.path()), format!("pending-limit-{count}"))
        .await
        .unwrap();
        // Bound inactivity, not the total time to drain the backlog.
        let mut texts = Vec::new();
        let mut assigned = false;
        let mut failed = false;
        loop {
            let event = match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                client.next_event(),
            )
            .await
            {
                Ok(event) => event.unwrap(),
                Err(_) => {
                    panic!("pending backlog {count}: no event within 10s of the previous one")
                }
            };
            match event {
                Event::AgentMessageChunk { text, .. } => texts.push(text),
                Event::AcpSessionAssigned { .. } => assigned = true,
                // The runner can truncate pre-announcement replay before SDK
                // buffering. Its rejected replay seal is an establishment
                // failure, not an invented transport outage.
                Event::AgentStartupError { .. } => {
                    failed = true;
                    break;
                }
                Event::AcpConnectionUnavailable { source, cause, .. } => {
                    assert_eq!(source, crate::acp::state::AcpConnectionSource::Sdk);
                    assert_eq!(cause, crate::acp::state::AcpConnectionCause::ProtocolEnded);
                    failed = true;
                    break;
                }
                _ => {}
            }
            if texts.len() == cap && assigned {
                break;
            }
        }
        if count == cap {
            assert!(!failed);
            assert!(assigned);
            assert_eq!(
                texts,
                (0..cap).map(|i| format!("item-{i}")).collect::<Vec<_>>()
            );
            client.send_prompt("still live", &[]).await.unwrap();
            assert_eq!(
                identity_texts(&identity_events_until_stop(&mut client).await),
                ["live"]
            );
            let _ = client.shutdown().await;
        } else {
            assert!(failed);
            assert!(!assigned);
            assert!(
                texts.is_empty(),
                "pending backlog {count}: incomplete replay published {} text chunks before failure",
                texts.len()
            );
            assert!(client.send_prompt("overflow cannot issue work", &[]).await.is_err());
            let rows = crate::session::Storage::open_unwatched("test").unwrap()
                .load_instances_complete().unwrap();
            let row = rows.iter().find(|row| row.id == client.id).unwrap();
            assert!(matches!(&row.acp_establishment,
                Some(crate::acp::establishment::AcpEstablishment::Blocked { .. })),
                "overflow must durably block its original establishment: {:?}", row.acp_establishment);
            client.shutdown().await.expect("retire the exact original after overflow");
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while let Some(event) = client.next_event().await {
                    assert!(!matches!(event, Event::AgentMessageChunk { .. } | Event::AcpSessionAssigned { .. }));
                }
            }).await.expect("actual retirement closes the overflow observer");
            let capture = temp.path().join("identity-capture.ndjson");
            assert_eq!(requests(&capture, "session/new"), 1);
            for method in ["session/load", "session/fork", "session/prompt"] {
                assert_eq!(requests(&capture, method), 0, "overflow cannot issue {method}");
            }
        }
        scope.finish_conversation_case().await;
    } })).await;
    }

    #[tokio::test]
    async fn native_identity_failed_load_preserves_history_without_replacement() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { let temp = scope.tempdir();
    let script = write_early_identity_agent(temp.path());
    let release = temp.path().join("release-failed-load");
    let source = std::fs::read_to_string(&script).unwrap().replace(
        "      printf '{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{}}\\n' \"$id\" ;;",
        &format!(
            "      while [ ! -f '{}' ]; do sleep 0.01; done\n      printf '{{\"jsonrpc\":\"2.0\",\"id\":%s,\"error\":{{\"code\":-32603,\"message\":\"stored session unavailable\"}}}}\\n' \"$id\" ;;",
            release.display()
        ),
    );
    std::fs::write(&script, source).unwrap();
    let mut config = reset_fake_spawn_config(&script, temp.path());
    config.stored_acp_session_id = Some("stored".into());
    config.seed_history_replay = true;
    let mut client = scope.spawn(config, "fallback-order")
        .await
        .unwrap();
    std::fs::write(&release, "").unwrap();
    let events = events_until(&mut client, |event| matches!(event, Event::AgentStartupError { .. })).await;
    assert!(client.send_prompt("no fallback", &[]).await.is_err());
    assert!(!events.iter().any(|event| matches!(event,
        Event::SessionContextReset { .. } | Event::SessionCleared | Event::AcpSessionAssigned { .. }
    )));
    assert_eq!(identity_texts(&events), ["history"]);
    let wire = std::fs::read_to_string(temp.path().join("identity-capture.ndjson")).unwrap();
    assert_eq!(wire.matches("\"method\":\"session/new\"").count(), 1, "only the genuine original may issue New");
    assert_eq!(wire.matches("\"method\":\"session/load\"").count(), 1);
    assert_eq!(wire.matches("\"method\":\"session/prompt\"").count(), 0);
    client.shutdown().await.unwrap(); })).await;
    }

    #[tokio::test]
    async fn native_identity_accepted_callback_does_not_strand_reset_deadline_or_stop() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { let temp = scope.tempdir();
    let (script, capture) = FakeAgent::default().write(temp.path());
    let callback = serde_json::json!({"jsonrpc":"2.0","id":"pending-permission","method":"session/request_permission","params":{"sessionId":"sid-1","toolCall":{"toolCallId":"own-tool","title":"Wait for user"},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]}});
    let source = std::fs::read_to_string(&script).unwrap().replace(
        "    *'\"method\":\"session/prompt\"'*)",
        &format!("    *'\"method\":\"session/prompt\"'*)\n      if [ \"$count\" -eq 1 ]; then printf '%s\\n' '{}'; fi", callback.to_string().replace('\'', "'\"'\"'")),
    );
    std::fs::write(&script, source).unwrap();
    let mut client = scope.spawn(reset_fake_spawn_config(&script, temp.path()), "callback-deadline")
    .await
    .unwrap();
    client.ready().await;
    client.send_prompt("request a Current decision", &[]).await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut approval = false;
    let mut stopped = false;
    while !approval || !stopped {
        let event = tokio::time::timeout_at(deadline, client.next_event())
            .await
            .unwrap()
            .expect("Current callback and its prompt remain observable");
        approval |= matches!(event, Event::ApprovalRequested { .. });
        stopped |= matches!(event, Event::Stopped { .. });
    }
    let (write_pending, writing) = oneshot::channel();
    *client.actual().test_connection_state().test_fresh_write_pending.lock().unwrap() = Some(write_pending);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let proposal = client.prepare_reset(deadline).await;
    let result = {
        let reset = reset_with_proposal(&client, proposal, deadline);
        tokio::pin!(reset);
        tokio::select! {
            outcome = &mut reset => panic!("reset bypassed its accepted callback: {outcome:?}"),
            pending = writing => pending.expect("actual Fresh writer waits for the accepted callback"),
        }
        reset.await
    };
    assert!(matches!(result, ResetSessionOutcome::Failed { .. }));
    let terminals_before = scope.state.acp_event_store.replay_from(&client.id, 0).unwrap()
        .into_iter().filter(|stored| {
            matches!(stored.event.as_ref(), Event::Stopped { .. })
                && stored.metadata.native_origin.is_some()
                && stored.metadata.native_prompt_id.is_some()
        }).map(|stored| (stored.metadata.native_origin, stored.metadata.native_prompt_id))
        .collect::<Vec<_>>();
    assert_eq!(terminals_before.len(), 1, "the actual first prompt owns one Native terminal");
    client
        .cmd_tx
        .as_ref()
        .unwrap()
        .send(client.queue_command(
            ClientCmd::ForceStop,
            Some(crate::acp::establishment::NativeOperationCorrelation::new(None)),
        ).await.unwrap())
        .await
        .unwrap();
    client.cancel_prompt().await.unwrap();
    // ForceStop reserves terminal ownership for the supervisor's user_forced
    // publication. A later Cancel must still reach native transport, but
    // cannot invent a second private terminal. Flush commands, not events.
    let (done, flushed) = tokio::sync::oneshot::channel();
    client
        .cmd_tx
        .as_ref()
        .unwrap()
        .send(client.queue_command(ClientCmd::FlushForTest(done), None).await.unwrap())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), flushed)
        .await
        .unwrap()
        .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let wire = std::fs::read_to_string(&capture).unwrap();
        let requests: Vec<serde_json::Value> = wire
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        if requests
            .iter()
            .filter(|request| request["method"] == "session/cancel")
            .count()
            == 2
        {
            assert_eq!(
                requests
                    .iter()
                    .filter(|request| request["method"] == "session/new")
                    .count(),
                1
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "stop never reached native transport: {wire}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    client.shutdown().await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while let Some(event) = client.next_event().await {
            if matches!(event, Event::AgentStartupError { .. }) {
                panic!("retiring the accepted callback must not create a startup failure: {event:?}");
            }
        }
    })
    .await
    .unwrap();
    let terminals_after = scope.state.acp_event_store.replay_from(&client.id, 0).unwrap()
        .into_iter().filter(|stored| {
            matches!(stored.event.as_ref(), Event::Stopped { .. })
                && stored.metadata.native_origin.is_some()
                && stored.metadata.native_prompt_id.is_some()
        }).map(|stored| (stored.metadata.native_origin, stored.metadata.native_prompt_id))
        .collect::<Vec<_>>();
    assert_eq!(terminals_after, terminals_before,
        "ForceStop/shutdown may own host terminals; Cancel cannot duplicate the original Native terminal");
    })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unavailable_resume_stays_durably_blocked_without_replacement() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {
    use crate::acp::transcript::{TranscriptModel, TranscriptRowKind};
    for (name, stored, load_capable, load_fails, new_fails, fork) in [
        ("unavailable", true, false, false, false, false),
        ("initial", false, false, false, false, false),
        ("loaded", true, true, false, false, false),
        ("load_failed", true, true, true, false, false),
        ("initial_new_failed", false, false, false, true, false),
        ("unavailable_new_failed", true, false, false, true, false),
        ("load_and_new_failed", true, true, true, true, false),
        ("fork_unsupported", true, false, false, false, true),
        ("fork_unsupported_load_failed", true, true, true, false, true),
    ] {
        let tmp = scope.tempdir();
        let script = tmp.path().join("resume-agent.sh");
        let capture = tmp.path().join("resume-capture.ndjson");
        let error = r#""error":{"code":-32603,"message":"fixture refused establishment"}"#;
        let new_ok = r#""result":{"sessionId":"replacement-session"}"#;
        let script_body = r#"#!/bin/sh
    while IFS= read -r line; do
      printf '%s\n' "$line" >> '__CAPTURE__'
      id=$(printf '%s' "$line" | sed -En 's/.*"id":("[^"]*"|[0-9]+).*/\1/p')
      case $line in
        *'"method":"initialize"'*)
          printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":__LOAD_CAPABLE__}}}\n' "$id" ;;
        *'"method":"session/load"'*)
          printf '{"jsonrpc":"2.0","id":%s,__LOAD_REPLY__}\n' "$id" ;;
        *'"method":"session/new"'*)
          printf '{"jsonrpc":"2.0","id":%s,__NEW_REPLY__}\n' "$id" ;;
        *'"method":"session/prompt"'*)
          printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id" ;;
      esac
    done
    "#
        .replace("__CAPTURE__", capture.to_str().unwrap())
        .replace("__LOAD_CAPABLE__", &load_capable.to_string())
        .replace("__LOAD_REPLY__", if load_fails { error } else { r#""result":{}"# })
        .replace("__NEW_REPLY__", if new_fails { error } else { new_ok });
        std::fs::write(&script, script_body).unwrap();
        let mut config = reset_fake_spawn_config(&script, tmp.path());
        if fork {
            config.agent_key = "claude".into();
            config.tool = "claude".into();
        }
        let mut selected_source = None;
        let mut client = if stored || fork {
            let (original_script, _) = FakeAgent::default().write(tmp.path());
            let mut original_config = reset_fake_spawn_config(&original_script, tmp.path());
            original_config.agent_key = config.agent_key.clone();
            original_config.tool = config.tool.clone();
            let mut original = scope.spawn(original_config, format!("{name}-original")).await.unwrap();
            original.ready().await;
            use axum::response::IntoResponse;
            let response = crate::server::api::acp_prompt(
                axum::extract::State(original.app_state()),
                axum::extract::Path(original.id.clone()),
                Ok(axum::Json(crate::acp::protocol::PromptRequest {
                    text: "prior user turn".into(),
                    attachments: Vec::new(),
                    prompt_id: None,
                    no_revive: false,
                    queued_prompt_id: None,
                })),
            )
            .await
            .into_response();
            assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
            identity_events_until_stop(&mut original).await;
            selected_source = Some(original.selected_native_session());
            if fork {
                scope.fork_after(original, config, name).await
            } else {
                scope.retained_after(original, config, name).await
            }
        } else {
            scope.spawn(config, name).await.unwrap()
        };
        let succeeds = !new_fails && !fork && (!stored || (load_capable && !load_fails));
        let mut events = events_until(&mut client, |event| matches!(
            event, Event::AcpSessionAssigned { .. } | Event::AgentStartupError { .. }
        )).await;
        if succeeds {
            client.ready().await;
            client.send_prompt("continue", &[]).await.unwrap();
            events.extend(identity_events_until_stop(&mut client).await);
        } else {
            assert!(client.send_prompt("do not replace a blocked conversation", &[]).await.is_err());
            let rows = crate::session::Storage::open_unwatched("test").unwrap()
                .load_instances_complete().unwrap();
            let row = rows.iter().find(|row| row.id == client.id).unwrap();
            assert!(matches!(&row.acp_establishment,
                Some(crate::acp::establishment::AcpEstablishment::Blocked { .. })),
                "{name}: the actual durable postimage is {:?}", row.acp_establishment);
            if stored && !fork {
                assert_eq!(
                    row.selected_acp_target().unwrap(),
                    crate::acp::establishment::EstablishmentTarget::Load {
                        session_id: selected_source.clone().unwrap(),
                        import: false,
                    },
                    "{name}: failed resume must retain the exact durable Load target"
                );
            }
        }
        let position = |pred: fn(&Event) -> bool| events.iter().position(pred);
        let assignment = position(|e| matches!(e, Event::AcpSessionAssigned { .. }));
        assert_eq!(assignment.is_some(), succeeds, "{name}");
        assert_eq!(
            position(|event| matches!(event, Event::AgentStartupError { .. })).is_some(),
            !succeeds, "{name}"
        );
        assert_eq!(position(|event| matches!(event, Event::Stopped { .. })).is_some(), succeeds, "{name}");
        assert!(!events.iter().any(|event| matches!(event, Event::SessionContextReset { .. } | Event::SessionCleared)), "{name}");

        let mut transcript = TranscriptModel::new();
        for stored in scope.state.acp_event_store.replay_from(&client.id, 0).unwrap() {
            transcript.apply_scoped_event(stored.seq, stored.event.as_ref(), &stored.metadata);
        }
        let rows = transcript.rows();
        let dividers = rows
            .iter()
            .filter(|row| row.kind == TranscriptRowKind::ContextReset)
            .count();
        assert_eq!(dividers, 0, "{name}");
        if stored && !fork {
            for text in ["prior user turn", "working"] {
                assert!(rows.iter().any(|row| row.text.contains(text)), "{name}: {text}");
            }
        }

        let requests: Vec<serde_json::Value> = std::fs::read_to_string(capture)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let loads: Vec<_> = requests
            .iter()
            .filter(|r| r["method"] == "session/load")
            .collect();
        assert_eq!(loads.len(), usize::from(stored && !fork && load_capable), "{name}");
        if let Some(load) = loads.first() {
            assert_eq!(load["params"]["sessionId"].as_str(), selected_source.as_deref(), "{name}");
        }
        let news = requests
            .iter()
            .filter(|r| r["method"] == "session/new")
            .count();
        assert_eq!(news, usize::from(!stored && !fork), "{name}");
        assert!(!requests.iter().any(|request| request["method"] == "session/fork"), "{name}");
        client.shutdown().await.unwrap();
        scope.finish_conversation_case().await;
    } })).await;
    }

    /// The deadline starts before enqueueing, so an expired command never
    /// sends the stateful session/new, and the loop keeps serving commands.
    #[tokio::test]
    async fn expired_reset_deadline_does_not_send_session_new() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| {
            Box::pin(async move {
                let _env = crate::session::test_support::EnvGuard::read_lock();
                let tmp = scope.tempdir();
                let (client, capture) = FakeAgent::default()
                    .spawn(scope, tmp.path(), "reset-expired", None)
                    .await;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
                let proposal = client.prepare_reset(deadline).await;
                let expired = tokio::time::Instant::now();
                let outcome = client
                    .actual()
                    .reset_session("/new", proposal, expired, expired + Duration::from_secs(4))
                    .await;
                assert!(
                    matches!(outcome, Err(crate::acp::acp_client::AcpError::RegistryBusy)),
                    "an expired proposal must fail before queue admission: {outcome:?}"
                );
                assert_eq!(requests(&capture, "session/new"), 1);
                let valid = reset_with_deadline(
                    &client,
                    tokio::time::Instant::now() + Duration::from_secs(2),
                )
                .await;
                assert!(
                    matches!(valid, ResetSessionOutcome::Reset { .. }),
                    "{valid:?}"
                );
                let _ = client.shutdown().await;
            })
        })
        .await;
    }

    /// Reset must not reattribute pending tools or background work to a new conversation.
    #[tokio::test]
    #[serial_test::serial]
    async fn reset_between_prompts_with_open_work_is_refused() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {
    for kind in ["tool", "agent", "task"] {
        let dir = scope.tempdir();
        let native_id = uuid::Uuid::new_v4().to_string();
        let mut new_seen = false;
        let (observe, observed) = oneshot::channel();
        scope.configure_sdk(crate::acp::acp_client::SdkTestOptions {
            tracking: Some(observe), ..Default::default()
        });
        let (config, peer) = scope.native_peer(dir.path().into(), "claude", "claude", move |request| {
            let id = request.get("id")?;
            Some(match request["method"].as_str().unwrap() {
                "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}}),
                "session/new" => {
                    assert!(!new_seen, "busy reset cannot issue another Native New");
                    new_seen = true;
                    serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":native_id}})
                }
                "session/prompt" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}),
                "session/cancel" => return None,
                other => panic!("unexpected Native input: {other}"),
            })
        });
        let mut client = scope.spawn(config, format!("reset-open-{kind}")).await.unwrap();
        client.ready().await;
        let tracking = tokio::time::timeout(Duration::from_secs(10), observed).await
            .expect("the actual SDK exposes its existing resource trackers").unwrap();
        let selected = client.selected_native_session();
        let actual = client.actual();
        let store = actual.native_store.as_ref().expect("actual pinned Claude store").stores[0].clone();
        let project = store.join("projects/-managed");
        std::fs::create_dir_all(project.join("subagents")).unwrap();
        std::fs::write(project.join(format!("{selected}.jsonl")), "").unwrap();
        let transcript = project.join("subagents/between-prompt-agent.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let (update, is_started): (serde_json::Value, fn(&Event) -> bool) = match kind {
            "tool" => (
                serde_json::json!({
                    "sessionUpdate":"tool_call","toolCallId":"between-prompt-tool",
                    "title":"long-running tool","kind":"other","status":"in_progress","rawInput":{}
                }),
                |event| matches!(event, Event::ToolCallStarted { tool_call } if tool_call.id == "between-prompt-tool"),
            ),
            "agent" => (
                serde_json::json!({
                    "sessionUpdate":"tool_call_update","toolCallId":"between-prompt-agent-tool",
                    "_meta":{"claudeCode":{"toolName":"Agent","toolResponse":{
                        "agentId":"between-prompt-agent","description":"keep working after the parent turn",
                        "prompt":"continue the delegated task","resolvedModel":"test-model",
                        "outputFile":transcript,"status":"async_launched"
                    }}}
                }),
                |event| matches!(event, Event::BackgroundAgentLaunched { agent_id, .. } if agent_id == "between-prompt-agent"),
            ),
            "task" => (
                serde_json::json!({
                    "sessionUpdate":"tool_call_update","toolCallId":"between-prompt-task-tool",
                    "_meta":{"claudeCode":{"toolName":"Bash","toolResponse":{"backgroundTaskId":"between-prompt-task"}}}
                }),
                |event| matches!(event, Event::BackgroundTaskStarted { task_id, .. } if task_id == "between-prompt-task"),
            ),
            _ => unreachable!(),
        };
        peer.send(serde_json::json!({
            "jsonrpc":"2.0","method":"session/update","params":{"sessionId":selected,"update":update}
        })).await;
        events_until(&mut client, is_started).await;
        if kind != "tool" {
            let history = scope.state.acp_event_store.replay_from(&client.id, 0).unwrap();
            let start = history.iter().find(|stored| is_started(stored.event.as_ref())).unwrap();
            assert!(start.metadata.resource_witness.is_some(),
                "{kind}: raw display alone cannot prove live Native resource custody");
        }
        client.send_prompt("finish a foreground turn without finishing the work", &[]).await.unwrap();
        identity_events_until_stop(&mut client).await;
        match kind {
            "agent" => assert!(!tracking.agents.is_empty(),
                "the genuine background agent remains open across the foreground terminal"),
            "task" => assert!(!tracking.tasks.is_empty(),
                "the genuine background task remains open across the foreground terminal"),
            "tool" => {}
            _ => unreachable!(),
        }

        let outcome = client.reset_session("/new").await.unwrap();
        assert!(
            matches!(&outcome, ResetSessionOutcome::Failed { message }
                if message.contains("agent work is still in flight")),
            "{outcome:?}"
        );
        let events = events_until(&mut client, |e| {
            matches!(e, Event::PromptRejected { .. } | Event::SessionCleared | Event::Stopped { .. })
        })
        .await;
        assert!(matches!(
            events.last(),
            Some(Event::PromptRejected { reason, text }) if reason == "agent_busy" && text == "/new"
        ));
        assert_eq!(peer.requests().iter().filter(|method| method.as_str() == "session/new").count(), 1);
        let _ = client.shutdown().await;
    } })).await;
    }

    #[tokio::test]
    async fn retained_native_default_callbacks_finish_before_a_waiting_fresh() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| {
            Box::pin(async move {
                let temp = scope.tempdir();
                let (script, capture) = FakeAgent {
                    hold_reset_config: true,
                    ..FakeAgent::default()
                }
                .write(temp.path());
                let written = temp.path().join("native-default-callback.txt");
                let callback = serde_json::json!({
                    "jsonrpc": "2.0", "id": "default-write",
                    "method": "fs/write_text_file",
                    "params": {
                        "sessionId": "sid-2", "path": written,
                        "content": "original default still owns native duration"
                    }
                });
                let source = std::fs::read_to_string(&script).unwrap();
                let gate = format!(
                    r#"        while [ -d "{dir}" ] && [ ! -f "$CAPTURE.callback-release" ]; do sleep 0.01; done
        printf '%s\n' '{callback}'
        while IFS= read -r reply; do
          printf '%s\n' "$reply" >> "$CAPTURE"
          case $reply in *'"id":"default-write"'*) break ;; esac
        done
        while [ -d "{dir}" ] && [ ! -f "$CAPTURE.config-release" ]; do sleep 0.01; done"#,
                    dir = temp.path().display(),
                    callback = callback.to_string().replace('\'', "'\"'\"'"),
                );
                let held = format!(
                    "        while [ -d \"{}\" ] && [ ! -f \"$CAPTURE.config-release\" ]; do sleep 0.01; done",
                    temp.path().display()
                );
                assert!(source.contains(&held));
                std::fs::write(&script, source.replace(&held, &gate)).unwrap();
                let mut config = reset_fake_spawn_config(&script, temp.path());
                config.default_effort = Some("high".into());
                let mut client = scope.spawn(config, "default-callback-fresh").await.unwrap();
                client.ready().await;
                let owner = client.connected_runner().unwrap();
                {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
                let proposal = client.prepare_reset(deadline).await;
                let first = reset_with_proposal(&client, proposal, deadline);
                tokio::pin!(first);
                tokio::select! {
                    outcome = &mut first => panic!("default was not held: {outcome:?}"),
                    _ = wait_for_captured_requests(&capture, "session/set_config_option", 2) => {}
                }
                assert!(matches!(first.await, ResetSessionOutcome::Failed { .. }));
                }

                {
                let (write_pending, writing) = oneshot::channel();
                *client.actual().test_connection_state().test_fresh_write_pending.lock().unwrap() =
                    Some(write_pending);
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                let proposal = client.prepare_reset(deadline).await;
                let second = reset_with_proposal(&client, proposal, deadline);
                tokio::pin!(second);
                tokio::select! {
                    outcome = &mut second => panic!("Fresh bypassed original duration: {outcome:?}"),
                    pending = writing => pending.unwrap(),
                }
                std::fs::write(capture.with_extension("ndjson.callback-release"), "release").unwrap();
                let response = loop {
                    let wire = std::fs::read_to_string(&capture).unwrap();
                    if let Some(response) = wire.lines()
                        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                        .find(|line| line["id"] == "default-write" && line.get("method").is_none())
                    {
                        break response;
                    }
                    tokio::select! {
                        outcome = &mut second => panic!("Fresh cancelled its original callback: {outcome:?}"),
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                    }
                    assert!(tokio::time::Instant::now() < deadline, "Native callback response missing");
                };
                assert!(response.get("result").is_some(), "{response}");
                assert_eq!(std::fs::read_to_string(&written).unwrap(),
                    "original default still owns native duration");
                assert_eq!(requests(&capture, "session/new"), 2);
                std::fs::write(capture.with_extension("ndjson.config-release"), "release").unwrap();
                assert!(matches!(second.await, ResetSessionOutcome::Reset { .. }));
                }
                client.ready().await;
                assert_eq!(client.selected_native_session(), "sid-3");
                assert_eq!(client.connected_runner(), Some(owner));
                client.send_prompt("after the original callback settled", &[]).await.unwrap();
                events_until(&mut client, |event| {
                    matches!(event, Event::AgentMessageChunk { text, .. } if text == "working")
                }).await;
                identity_events_until_stop(&mut client).await;
                assert_eq!(requests(&capture, "session/new"), 3);
                assert_eq!(requests(&capture, "session/prompt"), 1);
                let _ = client.shutdown().await;
            })
        }).await;
    }

    /// Caller deadlines release command handling, not original establishment
    /// custody. A late Native New may settle, but cannot trigger a retry.
    #[tokio::test]
    #[serial_test::serial]
    async fn stalled_reset_request_releases_the_connection_loop() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { let _env = crate::session::test_support::EnvGuard::read_lock();
    for hold_new in [true, false] {
        let tmp = scope.tempdir();
        let agent = FakeAgent {
            hold_reset_new: hold_new,
            hold_reset_config: !hold_new,
            ..FakeAgent::default()
        };
        let effort = (!hold_new).then_some("high");
        let (mut client, capture) = agent.spawn(scope, tmp.path(), "reset-timeout", effort).await;
        let owner = client.connected_runner().unwrap();
        let (method, release) = if hold_new {
            ("session/new", "new-release")
        } else {
            ("session/set_config_option", "config-release")
        };

        let first = {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
            let proposal = client.prepare_reset(deadline).await;
            let first = reset_with_proposal(&client, proposal, deadline);
            tokio::pin!(first);
            tokio::select! {
                outcome = &mut first => panic!("reset finished before the RPC was held: {outcome:?}"),
                _ = wait_for_captured_requests(&capture, method, 2) => {}
            }
            first.await
        };
        assert!(matches!(&first, ResetSessionOutcome::Failed { .. }), "{first:?}");

        if hold_new {
            assert!(client.send_prompt("cannot replace a pending original", &[]).await.is_err());
            assert_eq!(requests(&capture, "session/new"), 2);
        }
        std::fs::write(
            tmp.path().join(format!("capture.ndjson.{release}")),
            "release",
        ).unwrap();
        let settled = events_until(&mut client, |event| {
            matches!(event, Event::Stopped { reason } if reason == "session_reset")
        }).await;
        assert!(settled.iter().any(|event| matches!(event, Event::SessionCleared)));
        client.ready().await;
        assert_eq!(client.selected_native_session(), "sid-2");
        assert_eq!(client.connected_runner(), Some(owner));
        client.send_prompt("after original settlement", &[]).await.unwrap();
        identity_events_until_stop(&mut client).await;
        let wire = std::fs::read_to_string(&capture).unwrap();
        let requests: Vec<serde_json::Value> = wire
            .lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        let news: Vec<_> = requests.iter()
            .filter(|request| request["method"] == "session/new").collect();
        assert_eq!(news.len(), 2, "caller timeout cannot issue a replacement New: {wire}");
        assert!(!requests.iter().any(|request| request["method"] == "$/cancel_request"
            && request["params"]["requestId"] == news[1]["id"]),
            "caller timeout cannot cancel original establishment: {wire}");
        let prompt = requests.iter().find(|request| request["method"] == "session/prompt").unwrap();
        assert_eq!(prompt["params"]["sessionId"], client.selected_native_session());
        let _ = client.shutdown().await;
        scope.finish_conversation_case().await;
    } })).await;
    }

    /// #2979: a clear on a profile without a native reset opens a fresh
    /// session on the live worker, emits the ordered boundary events,
    /// re-applies the configured effort, and never forwards the alias text.
    #[tokio::test]
    async fn codex_clear_drives_fresh_session_new_on_live_worker() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { let _env = crate::session::test_support::EnvGuard::read_lock();
    let tmp = scope.tempdir();
    let (mut client, capture) = FakeAgent::default().spawn(scope, tmp.path(), "reset-2979", Some("high"))
        .await;

    let outcome = client.reset_session("/new").await.unwrap();
    assert!(
        matches!(&outcome, ResetSessionOutcome::Reset { new_acp_session_id } if new_acp_session_id == "sid-2"),
        "{outcome:?}"
    );
    let events = events_until(&mut client, |e| matches!(e, Event::Stopped { .. })).await;
    let position = |pred: &dyn Fn(&Event) -> bool| events.iter().position(pred).unwrap();
    let cleared = position(&|e| matches!(e, Event::SessionCleared));
    let reset = position(&|e| matches!(e, Event::SessionContextReset { .. }));
    let assigned = position(
        &|e| matches!(e, Event::AcpSessionAssigned { acp_session_id, .. } if acp_session_id == "sid-2"),
    );
    assert!(cleared < reset && reset < assigned, "{events:?}");
    assert!(
        matches!(events.last(), Some(Event::Stopped { reason }) if reason == "session_reset")
    );

    client.send_prompt("hello", &[]).await.unwrap();
    events_until(&mut client, |e| matches!(e, Event::Stopped { .. })).await;

    let wire = std::fs::read_to_string(&capture).unwrap();
    assert_eq!(requests(&capture, "session/new"), 2, "{wire}");
    assert!(!wire.contains("\"text\":\"/new\""), "{wire}");
    assert!(wire.contains("\"sessionId\":\"sid-2\""), "{wire}");
    assert_eq!(requests(&capture, "session/set_config_option"), 2, "{wire}");
    assert_eq!(wire.matches("\"value\":\"high\"").count(), 2, "{wire}");
    let _ = client.shutdown().await; })).await;
    }

    /// #2979: resetting under an in-flight turn would orphan it, so it is
    /// refused with a retryable `PromptRejected` and no `SessionCleared`.
    #[tokio::test]
    #[serial_test::serial]
    async fn reset_during_in_flight_prompt_is_refused_with_prompt_rejected() {
        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move { let _env = crate::session::test_support::EnvGuard::read_lock();
    let tmp = scope.tempdir();
    let agent = FakeAgent {
        hold_prompt: true,
        ..FakeAgent::default()
    };
    let (mut client, capture) = agent.spawn(scope, tmp.path(), "reset-busy-2979", None).await;
    client.send_prompt("hello", &[]).await.unwrap();
    // The chunk proves the task is inside the in-flight select.
    events_until(&mut client, |e| {
        matches!(e, Event::AgentMessageChunk { .. })
    })
    .await;

    let outcome = client.reset_session("/new").await.unwrap();
    assert!(
        matches!(&outcome, ResetSessionOutcome::Failed { message } if message.contains("turn is in flight")),
        "{outcome:?}"
    );
    let rejected =
        events_until(&mut client, |e| matches!(e, Event::PromptRejected { .. })).await;
    assert!(matches!(
        rejected.last(),
        Some(Event::PromptRejected { reason, text }) if reason == "agent_busy" && text == "/new"
    ));
    std::fs::write(tmp.path().join("capture.ndjson.prompt-release"), "release").unwrap();
    let rest = events_until(&mut client, |e| matches!(e, Event::Stopped { .. })).await;
    assert!(!rejected
        .iter()
        .chain(&rest)
        .any(|e| matches!(e, Event::SessionCleared)));
    assert_eq!(requests(&capture, "session/new"), 1);
    let _ = client.shutdown().await; })).await;
    }
}
