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
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::reset_fake_spawn_config;
    use crate::acp::acp_client::AcpClient;
    use crate::acp::state::{AcpSessionId, Event};
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

    /// Emits the `/compact` start marker then goes silent, as
    /// claude-agent-acp does while summarizing. It answers
    /// `_session/steering` with a normal success, so a daemon that DID steer
    /// would look like it worked; the test proves nothing was sent.
    #[cfg(unix)]
    fn write_compacting_fake_agent(
        dir: &std::path::Path,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        let capture = dir.join("capture.ndjson");
        let script_path = dir.join("fake-compacting-agent.sh");
        let script = r#"#!/bin/sh
CAPTURE=__CAPTURE__

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$CAPTURE"
  id=$(printf '%s' "$line" | sed -En 's/.*"id":("[^"]*"|[0-9]+).*/\1/p')
  case $line in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":false},"_meta":{"steering":{"supported":true}}}}\n' "$id"
      ;;
    *'"method":"session/new"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessionId":"sid-1"}}\n' "$id"
      ;;
    *'"method":"_session/steering"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"outcome":"injected"}}\n' "$id"
      ;;
    *'"method":"session/prompt"'*)
      printf '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sid-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Compacting..."}}}}\n'
      while [ -d "__DIR__" ] && [ ! -f "$CAPTURE.prompt-release" ]; do sleep 0.01; done
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

    /// #3219: a `/compact` turn only summarizes, so a follow-up steered into
    /// it is answered `Injected` and swallowed by a turn that never replies,
    /// with no Retry pill and no re-dispatch. Driven through the live prompt
    /// loop because what can break is whether the compaction latch is applied
    /// by the time the follow-up reaches the `cmd_rx` arm.
    #[cfg(unix)]
    #[tokio::test]
    async fn follow_up_during_compaction_is_rejected_instead_of_steered() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let (script, capture) = write_compacting_fake_agent(tmp.path());
        let mut config = reset_fake_spawn_config(&script, tmp.path());
        config.spec.description = "scripted compacting fake".into();
        let mut client = AcpClient::spawn(config, AcpSessionId("compact-3219".into()))
            .await
            .expect("spawn scripted fake agent");

        client
            .send_prompt("/compact", &[])
            .await
            .expect("send /compact");

        // The typed event, not just the chunk: it proves the latch is set, so
        // the follow-up cannot pass the steering gate for the wrong reason.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let ev = tokio::time::timeout_at(deadline, client.next_event())
                .await
                .expect("timed out waiting for the compaction to start")
                .expect("event channel closed");
            if matches!(&ev, Event::ConversationCompactionStarted) {
                break;
            }
        }

        client
            .send_prompt("also check the tests", &[])
            .await
            .expect("send follow-up");

        let mut saw_rejected = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let ev = tokio::time::timeout_at(deadline, client.next_event())
                .await
                .expect("timed out waiting for PromptRejected + Stopped")
                .expect("event channel closed");
            match &ev {
                Event::PromptRejected { reason, text } => {
                    assert_eq!(reason, "agent_busy");
                    assert_eq!(
                        text, "also check the tests",
                        "the retry pill needs the text"
                    );
                    saw_rejected = true;
                    std::fs::write(tmp.path().join("capture.ndjson.prompt-release"), "release")
                        .unwrap();
                }
                Event::Stopped { .. } => break,
                _ => {}
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
        let _ = client.shutdown().await;
    }
}
