//! Mid-turn steering: the request wire form and the outcomes an agent can
//! return when it is asked to take new input.

use agent_client_protocol::schema::v1::{ContentBlock, SessionId};
use agent_client_protocol::JsonRpcRequest;
use serde::{Deserialize, Serialize};

/// Params for the `_session/steering` extension request: apply a
/// follow-up message to the turn that is already running, rather than
/// queuing it as a separate `session/prompt`. See #2805.
///
/// `_meta.steering.idleBehavior = "promptRequired"` is the opt-in added
/// in claude-agent-acp 0.64.0 (upstream #903 / #919). Without it a steer
/// that arrives after the turn settled starts a detached turn whose
/// `PromptResponse` no request owns; with it the adapter leaves the
/// content untouched and says so, and AoE resends it as a normal prompt.
/// `agent_compat::supports_steering` is what guarantees the adapter
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

/// What the agent did with a steered message. Both success outcomes are
/// normal: the adapter, not AoE, adjudicates whether a turn was still
/// running when the steer landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SteerOutcome {
    /// Delivered into the running turn. The message's own output streams
    /// as ordinary `session/update` notifications and the turn's existing
    /// `PromptResponse` still owns the terminal `Stopped`.
    Injected,
    /// The turn settled before the steer was handled. The adapter
    /// guarantees the content was neither queued nor consumed, so it is
    /// safe (and required) to resend it as a normal `session/prompt`.
    PromptRequired,
    /// The adapter ignored the `promptRequired` opt-in and started a
    /// detached turn with the content anyway. Only reachable from an
    /// adapter that clears `supports_steering` but does not honor the
    /// contract, so it is a protocol violation rather than an expected
    /// state. The content IS consumed, so it must not be resent.
    StartedNewTurn,
    /// An outcome string this build does not know. Treated like
    /// `StartedNewTurn`: delivery is unproven either way, and resending
    /// risks duplicating the user's message.
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

    /// The steer wire contract (#2805). `sessionId` must be camelCase and
    /// the `_meta` opt-in must be spelled exactly as the adapter reads it:
    /// a typo in either silently degrades a racing steer back to
    /// `startedNewTurn`, the detached-turn bug the version floor exists to
    /// avoid, with no error to notice.
    #[test]
    fn steer_request_carries_the_prompt_required_opt_in() {
        let req = SteerRequest::new(
            SessionId::new("sess-1"),
            vec![ContentBlock::Text(TextContent::new("also check the tests"))],
        );
        let wire = serde_json::to_value(&req).unwrap();
        assert_eq!(wire["sessionId"], "sess-1");
        assert_eq!(wire["_meta"]["steering"]["idleBehavior"], "promptRequired");
        assert_eq!(wire["prompt"][0]["text"], "also check the tests");
    }

    /// An unrecognized outcome must land on `Unknown`, not on a success
    /// arm: `Unknown` is treated as "consumed, do not resend", which is
    /// the only safe reading when a future adapter adds an outcome this
    /// build has never seen.
    #[test]
    fn steer_outcome_maps_every_wire_form() {
        let cases = [
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
        ];
        for (value, expected) in cases {
            assert_eq!(SteerOutcome::from_response(&value), expected, "{value}");
        }
    }

    /// A steering-capable fake that emits the `/compact` start marker and
    /// then goes silent, mirroring what claude-agent-acp does for the 90
    /// to 170 seconds it spends summarizing. It answers `_session/steering`
    /// with the normal `Injected`-shaped success, so a daemon that DID
    /// steer would look like it worked; the test proves the request was
    /// never sent at all.
    #[cfg(unix)]
    fn write_compacting_fake_agent(
        dir: &std::path::Path,
        prompt_delay_secs: u32,
    ) -> (std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let capture = dir.join("capture.ndjson");
        let script_path = dir.join("fake-compacting-agent.sh");
        let script = r#"#!/bin/sh
CAPTURE=__CAPTURE__
DELAY=__DELAY__
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
      if [ "$DELAY" -gt 0 ]; then sleep "$DELAY"; fi
      printf '{"jsonrpc":"2.0","id":%s,"result":{"stopReason":"end_turn"}}\n' "$id"
      ;;
  esac
done
"#
        .replace("__CAPTURE__", capture.to_str().expect("utf8 tmp path"))
        .replace("__DELAY__", &prompt_delay_secs.to_string());
        std::fs::write(&script_path, script).expect("write fake agent script");
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake agent script");
        (script_path, capture)
    }

    /// #3219: a `/compact` turn only summarizes context, so a follow-up
    /// must not be steered into it. The adapter would answer `Injected`
    /// and swallow the message into a turn that never replies, and that
    /// outcome emits no Retry pill and re-dispatches nothing, so the
    /// message is simply gone. Both composers park a mid-compaction send
    /// locally; this covers the POST already in flight when the marker
    /// landed, and direct API callers.
    ///
    /// Asserting through the live prompt loop rather than a unit test on
    /// the predicate: the thing that can actually break is whether the
    /// compaction latch is applied by the time the follow-up reaches the
    /// `cmd_rx` arm, and only the real signal plumbing exercises that.
    #[cfg(unix)]
    #[tokio::test]
    async fn follow_up_during_compaction_is_rejected_instead_of_steered() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        // 3s prompt delay: long enough to land the follow-up inside the
        // silent compaction window, short enough to keep the test snappy.
        let (script, capture) = write_compacting_fake_agent(tmp.path(), 3);
        let mut config = reset_fake_spawn_config(&script, tmp.path());
        config.spec.description = "scripted compacting fake".into();
        let mut client = AcpClient::spawn(config, AcpSessionId("compact-3219".into()))
            .await
            .expect("spawn scripted fake agent");

        client
            .send_prompt("/compact", &[])
            .await
            .expect("send /compact");

        // Wait for the typed start event, not just the chunk: it proves
        // the lifecycle signal reached the watchdog and latched the
        // compaction phase, so the follow-up below cannot race ahead of
        // the latch and pass the steering gate for the wrong reason.
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
