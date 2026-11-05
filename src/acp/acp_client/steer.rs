//! Mid-turn steering: the request wire form and the outcomes an agent can
//! return when it is asked to take new input.

use agent_client_protocol::schema::v1::{ContentBlock, SessionId};
use agent_client_protocol::JsonRpcRequest;
use serde::{Deserialize, Serialize};

/// Apply a follow-up to the turn already running rather than queuing it as a
/// separate `session/prompt` (#2805). Without the
/// `_meta.steering.idleBehavior = "promptRequired"` opt-in, a steer landing
/// after the turn settled starts a detached turn no request owns; with it the
/// adapter leaves the content alone and says so, and AoE resends it as a
/// normal prompt. `agent_compat::supports_steering` guarantees the adapter
/// honors the opt-in, so this always requests it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "_session/steering", response = serde_json::Value)]
#[serde(rename_all = "camelCase")]
pub(super) struct SteerRequest {
    session_id: SessionId,
    prompt: Vec<ContentBlock>,
    #[serde(rename = "_meta")]
    meta: serde_json::Value,
}

impl SteerRequest {
    pub(super) fn new(session_id: SessionId, prompt: Vec<ContentBlock>) -> Self {
        Self {
            session_id,
            prompt,
            meta: serde_json::json!({ "steering": { "idleBehavior": "promptRequired" } }),
        }
    }
}

/// Text of the first text block, for the retry pill on a refused prompt.
/// Attachments are not carried back into the pill; text is the retry hook
/// and this is a rare edge.
pub(super) fn first_text_block(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// What the agent did with a steered message. The adapter, not AoE,
/// adjudicates whether a turn was still running when the steer landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SteerOutcome {
    /// Delivered into the running turn, whose existing `PromptResponse` still
    /// owns the terminal `Stopped`.
    Injected,
    /// The turn settled first and the content was neither queued nor consumed,
    /// so it must be resent as a normal `session/prompt`.
    PromptRequired,
    /// The adapter ignored the opt-in and started a detached turn anyway: a
    /// protocol violation, but the content IS consumed, so never resend it.
    StartedNewTurn,
    /// An outcome this build does not know. Delivery is unproven either way,
    /// so it is treated like `StartedNewTurn` rather than risk a duplicate.
    Unknown,
}

impl SteerOutcome {
    pub(super) fn from_response(value: &serde_json::Value) -> Self {
        match value.get("outcome").and_then(serde_json::Value::as_str) {
            Some("injected") => Self::Injected,
            Some("promptRequired") => Self::PromptRequired,
            Some("startedNewTurn") => Self::StartedNewTurn,
            _ => Self::Unknown,
        }
    }
}

#[cfg(test)]
pub(super) static COMPACTION_FOLD_OBSERVED: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<usize, tokio::sync::oneshot::Sender<()>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::reset_fake_spawn_config;
    use crate::acp::state::Event;
    use agent_client_protocol::schema::v1::TextContent;

    /// The wire contract (#2805): a typo in `sessionId` or the `_meta` opt-in
    /// silently degrades a racing steer to `startedNewTurn`, with no error.
    #[test]
    fn steer_wire_forms() {
        let req = SteerRequest::new(
            SessionId::new("sess-1"),
            vec![ContentBlock::Text(TextContent::new("also check the tests"))],
        );
        let wire = serde_json::to_value(&req).unwrap();
        assert_eq!(wire["sessionId"], "sess-1");
        assert_eq!(wire["_meta"]["steering"]["idleBehavior"], "promptRequired");
        assert_eq!(wire["prompt"][0]["text"], "also check the tests");

        // An outcome this build has never seen must land on `Unknown`, which
        // reads as "consumed, do not resend", never on a success arm.
        {
            for (value, expected) in [
                (
                    serde_json::json!({"outcome": "injected"}),
                    SteerOutcome::Injected,
                ),
                (
                    serde_json::json!({"outcome": "promptRequired", "reason": "noRunningTurn"}),
                    SteerOutcome::PromptRequired,
                ),
                (
                    serde_json::json!({"outcome": "startedNewTurn"}),
                    SteerOutcome::StartedNewTurn,
                ),
                // Forward-compat and malformed shapes both fall to Unknown.
                (
                    serde_json::json!({"outcome": "teleported"}),
                    SteerOutcome::Unknown,
                ),
                (serde_json::json!({"outcome": 7}), SteerOutcome::Unknown),
                (serde_json::json!({}), SteerOutcome::Unknown),
                (serde_json::json!(null), SteerOutcome::Unknown),
            ] {
                assert_eq!(SteerOutcome::from_response(&value), expected, "{value}");
            }
        }
    }

    #[cfg(unix)]
    fn write_compacting_fake_agent(
        dir: &std::path::Path,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let capture = dir.join("capture.ndjson");
        let script_path = dir.join("fake-compacting-agent.sh");
        let script = r#"#!/bin/sh
CAPTURE=__CAPTURE__
new_count=0

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CAPTURE"
  id=$(printf '%s' "$line" | sed -En 's/.*"id":("[^"]*"|[0-9]+).*/\1/p')
  case $line in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":false},"_meta":{"steering":{"supported":true}}}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      new_count=$((new_count + 1))
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sid-%s"}}\n' "$id" "$new_count"
      if [ "$new_count" -gt 1 ]; then
        printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sid-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Compacting... from old conversation"}}}}\n'
      fi
      ;;
    *'"method":"_session/steering"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"outcome":"injected"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sid-%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Compacting..."}}}}\n' "$new_count"
      while [ -d "__DIR__" ] && [ ! -f "$CAPTURE.prompt-release" ]; do sleep 0.01; done
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sid-%s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Compacting completed."}}}}\n' "$new_count"
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#
        .replace("__CAPTURE__", capture.to_str().expect("utf8 tmp path"))
        .replace("__DIR__", dir.to_str().expect("utf8 tmp path"));
        std::fs::write(&script_path, script).expect("write fake agent script");
        (script_path, capture)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn follow_up_during_compaction_is_rejected_instead_of_steered() {
        managed_compaction_regression(None).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_compaction_start_survives_registry_and_lifecycle_contention() {
        for lifecycle in [false, true] {
            managed_compaction_regression(Some((lifecycle, true))).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_compaction_completion_survives_registry_and_lifecycle_contention() {
        for lifecycle in [false, true] {
            managed_compaction_regression(Some((lifecycle, false))).await;
        }
    }

    #[cfg(unix)]
    async fn release_captured_compaction_through_contention(
        client: &crate::acp::supervisor::test_support::managed::ManagedClient,
        captured: tokio::sync::oneshot::Receiver<()>,
        release: tokio::sync::oneshot::Sender<()>,
        lifecycle: bool,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(10), captured)
            .await
            .expect("actual Native compaction cue reaches its captured SDK boundary")
            .unwrap();
        let storage = crate::session::Storage::open_reference("test").unwrap();
        let registry = (!lifecycle).then(|| {
            crate::process::worker_registry::try_registry_lock(&client.id)
                .unwrap()
                .expect("hold actual original registry exclusion")
        });
        let native = lifecycle.then(|| {
            storage
                .acquire_instance_lifecycle_lock(&client.id)
                .expect("hold actual original lifecycle exclusion")
        });
        let state = client.actual().test_connection_state();
        let key = std::sync::Arc::as_ptr(&state) as usize;
        struct FoldObservation(usize);
        impl Drop for FoldObservation {
            fn drop(&mut self) {
                COMPACTION_FOLD_OBSERVED.lock().unwrap().remove(&self.0);
            }
        }
        let _observation = FoldObservation(key);
        let (folded, folded_wait) = tokio::sync::oneshot::channel();
        assert!(COMPACTION_FOLD_OBSERVED
            .lock()
            .unwrap()
            .insert(key, folded)
            .is_none());
        let (busy, observed) = tokio::sync::oneshot::channel();
        assert!(state
            .test_prompt_admission_busy
            .lock()
            .unwrap()
            .replace(busy)
            .is_none());
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), observed)
            .await
            .expect("the captured Native cue encounters actual guard contention")
            .unwrap();
        // Keep the real exclusion held after observed guard contention.
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_millis(3001)).await;
        let folded = tokio::time::timeout(std::time::Duration::from_secs(1), folded_wait).await;
        tokio::time::resume();
        folded
            .expect("runtime fold passes its former cutoff while exclusion remains held")
            .unwrap();
        drop(native);
        drop(registry);
    }

    #[cfg(unix)]
    async fn managed_compaction_regression(contended: Option<(bool, bool)>) {
        crate::acp::supervisor::test_support::managed::with_managed(move |scope| {
            Box::pin(async move {
                let tmp = scope.tempdir();
                let (script, capture) = write_compacting_fake_agent(tmp.path());
                let mut config = reset_fake_spawn_config(&script, tmp.path());
                config.spec.description = "scripted compacting fake".into();
                let mut client = scope
                    .spawn(config, "compact-3219")
                    .await
                    .expect("spawn real managed original with compacting Native");
                client.ready().await;
                for generation in 1..=2 {
                    if generation > 1 {
                        std::fs::remove_file(tmp.path().join("capture.ndjson.prompt-release"))
                            .unwrap();
                    }
                    let native_sid = format!("sid-{generation}");
                    let start_gate = contended.filter(|(_, start)| *start).map(|_| {
                        let (entered, captured) = tokio::sync::oneshot::channel();
                        let (release, released) = tokio::sync::oneshot::channel();
                        assert!(client
                            .actual()
                            .test_connection_state()
                            .test_session_update
                            .lock()
                            .unwrap()
                            .replace((SessionId::new(native_sid.as_str()), (entered, released)))
                            .is_none());
                        (captured, release)
                    });
                    let before_compaction = scope
                        .state
                        .acp_event_store
                        .highest_seq_checked(&client.id)
                        .unwrap();
                    client
                        .send_prompt("/compact", &[])
                        .await
                        .expect("send /compact");
                    if let (Some((lifecycle, _)), Some((captured, release))) =
                        (contended, start_gate)
                    {
                        release_captured_compaction_through_contention(
                            &client, captured, release, lifecycle,
                        )
                        .await;
                    }

                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        let started = scope
                            .state
                            .acp_event_store
                            .replay_from(&client.id, before_compaction)
                            .unwrap()
                            .iter()
                            .any(|stored| {
                                matches!(
                                    stored.event.as_ref(),
                                    Event::ConversationCompactionStarted
                                ) && stored.metadata.scope
                                    == crate::acp::event_sender::EventScope::SessionCurrent
                                    && stored
                                        .metadata
                                        .reported_session_id
                                        .as_ref()
                                        .map(|sid| sid.0.as_ref())
                                        == Some(native_sid.as_str())
                            });
                        if started {
                            break;
                        }
                        tokio::time::timeout_at(deadline, client.next_event())
                            .await
                            .expect("current Native compaction starts")
                            .expect("event channel closed");
                    }
                    let history = scope
                        .state
                        .acp_event_store
                        .replay_from(&client.id, 0)
                        .unwrap();
                    let mut projected = crate::acp::state::AcpState::new(
                        crate::acp::state::AcpSessionId(client.id.clone()),
                        crate::acp::state::AgentName("claude".into()),
                        None,
                    );
                    for stored in &history {
                        projected
                            .apply_scoped_event(stored.seq, &stored.event, &stored.metadata)
                            .unwrap();
                    }
                    projected.steering = true;
                    assert_eq!(
                        crate::acp::dispatch::decide(
                            &projected,
                            crate::acp::dispatch::WorkerLiveness {
                                running: true,
                                idle_dormant: false,
                                rate_limit_parked: false,
                            },
                        ),
                        crate::acp::dispatch::PromptDispatch::Queued {
                            reason: crate::acp::dispatch::QueueReason::Compacting,
                        },
                        "generation {generation}; active={}, compacting={}; frames={:?}; wire={}",
                        projected.turn_active,
                        projected.compacting,
                        history
                            .iter()
                            .map(|stored| (stored.seq, stored.event.as_ref(), &stored.metadata))
                            .collect::<Vec<_>>(),
                        std::fs::read_to_string(&capture).unwrap(),
                    );

                    let follow_up = format!("also check the tests in conversation {generation}");
                    client
                        .send_prompt(&follow_up, &[])
                        .await
                        .expect("send follow-up");

                    let mut completion_gate = contended.filter(|(_, start)| !*start).map(|_| {
                        let (entered, captured) = tokio::sync::oneshot::channel();
                        let (release, released) = tokio::sync::oneshot::channel();
                        assert!(client
                            .actual()
                            .test_connection_state()
                            .test_session_update
                            .lock()
                            .unwrap()
                            .replace((SessionId::new(native_sid.as_str()), (entered, released)))
                            .is_none());
                        (captured, release)
                    });
                    let mut saw_rejected = false;
                    let mut saw_stopped = false;
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        let ev = tokio::time::timeout_at(deadline, client.next_event())
                            .await
                            .expect("timed out waiting for PromptRejected + Stopped")
                            .expect("event channel closed");
                        match ev {
                            Event::PromptRejected { reason, text } => {
                                assert_eq!(reason, "agent_busy");
                                assert_eq!(
                                    text, follow_up,
                                    "the retry pill retains the refused input"
                                );
                                saw_rejected = true;
                                std::fs::write(
                                    tmp.path().join("capture.ndjson.prompt-release"),
                                    "release",
                                )
                                .unwrap();
                                if let (Some((lifecycle, _)), Some((captured, release))) =
                                    (contended, completion_gate.take())
                                {
                                    release_captured_compaction_through_contention(
                                        &client, captured, release, lifecycle,
                                    )
                                    .await;
                                }
                            }
                            Event::Stopped { .. } => saw_stopped = true,
                            _ => {}
                        }
                        let completed = scope
                            .state
                            .acp_event_store
                            .replay_from(&client.id, before_compaction)
                            .unwrap()
                            .iter()
                            .any(|stored| {
                                matches!(stored.event.as_ref(), Event::ConversationCompacted)
                                    && stored.metadata.scope
                                        == crate::acp::event_sender::EventScope::SessionCurrent
                                    && stored
                                        .metadata
                                        .reported_session_id
                                        .as_ref()
                                        .map(|sid| sid.0.as_ref())
                                        == Some(native_sid.as_str())
                            });
                        if saw_rejected && saw_stopped && completed {
                            break;
                        }
                    }
                    assert!(
                        saw_rejected,
                        "a follow-up refused during compaction must emit PromptRejected so the \
             user gets a Retry pill instead of a silently swallowed message"
                    );

                    let wire = std::fs::read_to_string(&capture).expect("read capture");
                    assert!(
            !wire.contains("\"method\":\"_session/steering\""),
            "the follow-up must not be steered into the compaction turn;\nwire capture:\n{wire}"
        );
                    let completed = scope
                        .state
                        .acp_event_store
                        .replay_from(&client.id, projected.last_seq)
                        .unwrap();
                    for stored in completed {
                        projected
                            .apply_scoped_event(stored.seq, &stored.event, &stored.metadata)
                            .unwrap();
                    }
                    assert!(!projected.compacting, "real completion releases compaction");
                    let expected_sid = format!("sid-{}", generation + 1);
                    assert!(matches!(
                        client.reset_session("/clear").await.unwrap(),
                        crate::acp::acp_client::ResetSessionOutcome::Reset { new_acp_session_id }
                            if new_acp_session_id == expected_sid
                    ));
                    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                    loop {
                        let event = tokio::time::timeout_at(deadline, client.next_event())
                            .await
                            .expect("old conversation update arrives after reset")
                            .expect("event channel closed");
                        if matches!(event, Event::AgentMessageChunk { text, .. }
                        if text == "Compacting... from old conversation")
                        {
                            break;
                        }
                    }
                    for stored in scope
                        .state
                        .acp_event_store
                        .replay_from(&client.id, projected.last_seq)
                        .unwrap()
                    {
                        projected
                            .apply_scoped_event(stored.seq, &stored.event, &stored.metadata)
                            .unwrap();
                    }
                    assert!(
                        !projected.compacting,
                        "old Native SID cannot block new conversation"
                    );
                }
                let _ = client.shutdown().await;
            })
        })
        .await;
    }
}
