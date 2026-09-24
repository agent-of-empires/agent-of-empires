//! ACP-specific request, response, and broadcast wire types shared by the
//! structured view daemon and its clients.

use std::borrow::Cow;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::approvals::ApprovalDecision;
use super::state::{DiffComment, Event};
use crate::daemon::PromptAttachmentKind;

/// `BackgroundAgentLaunched::output_file` is a host filesystem path: persisted
/// in event_json so a restarted daemon can re-tail the sub-agent, but never
/// sent to clients. Every place an `Event` is serialized for a client goes
/// through this so the invariant holds structurally.
fn strip_transcript_path(event: &Event) -> Cow<'_, Event> {
    match event {
        Event::BackgroundAgentLaunched { output_file, .. } if !output_file.is_empty() => {
            let mut stripped = event.clone();
            if let Event::BackgroundAgentLaunched { output_file, .. } = &mut stripped {
                output_file.clear();
            }
            Cow::Owned(stripped)
        }
        _ => Cow::Borrowed(event),
    }
}

/// One frame on the per-AppState structured view broadcast channel: the structured view
/// session id plus the typed structured view Event.
#[derive(Debug, Clone)]
pub struct AcpBroadcastFrame {
    pub session_id: String,
    pub seq: u64,
    pub event: Arc<Event>,
    /// Which installed worker produced this frame, for in-process
    /// consumers that must reject a replaced worker's queued frames. Never serialized.
    pub worker_generation: Option<u64>,
}

impl Serialize for AcpBroadcastFrame {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("AcpBroadcastFrame", 3)?;
        s.serialize_field("session_id", &self.session_id)?;
        s.serialize_field("seq", &self.seq)?;
        s.serialize_field("event", &*strip_transcript_path(&self.event))?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for AcpBroadcastFrame {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Mirror of the Serialize impl.
        #[derive(Deserialize)]
        struct Wire {
            session_id: String,
            seq: u64,
            event: Event,
        }
        let w = Wire::deserialize(deserializer)?;
        Ok(AcpBroadcastFrame {
            session_id: w.session_id,
            seq: w.seq,
            event: Arc::new(w.event),
            worker_generation: None,
        })
    }
}

/// One attachment as the web composer uploads it: the raw base64 bytes
/// inline in the prompt POST.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptAttachmentUpload {
    pub kind: PromptAttachmentKind,
    pub mime_type: String,
    /// Standard base64 (no `data:` URL prefix).
    pub data: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// `POST /api/sessions/{id}/acp/prompt` body.
#[derive(Debug, Serialize, Deserialize)]
pub struct PromptRequest {
    pub text: String,
    /// `#[serde(default)]` so text-only clients (and the TUI structured view
    /// verb) keep working unchanged.
    #[serde(default)]
    pub attachments: Vec<PromptAttachmentUpload>,
    /// Optional client-minted stable id for this prompt, threaded into the
    /// emitted `Event::UserPromptSent` so a client can reconcile its
    /// optimistic transcript row by id.
    #[serde(default, alias = "id")]
    pub prompt_id: Option<String>,
    /// Refuse rather than wake an archived/snoozed/idle-dormant session or
    /// queue behind a stopped worker. Set by `aoe send --no-revive`; other
    /// callers never set it and keep the default revive-as-needed behavior.
    #[serde(default)]
    pub no_revive: bool,
}

/// `POST /api/sessions/{id}/acp/prompt/diff-comments` body.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffCommentsPromptRequest {
    pub intro: String,
    pub outro: String,
    pub is_multi_repo: bool,
    pub comments: Vec<DiffComment>,
    pub assembled_markdown: String,
}

/// `POST /api/sessions/{id}/acp/approvals/{nonce}` body.
#[derive(Debug, Serialize, Deserialize)]
pub struct ResolveApprovalRequest {
    pub decision: ApprovalDecisionWire,
    /// The `option_id` the user picked off the agent's own labels, for an
    /// approval the client rendered as an answer list (`Approval.choice`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub option_id: Option<String>,
}

/// PascalCase JSON variants (`Allow`, `AllowAlways`, `Deny`,
/// `Cancelled`) matching the web frontend's approval flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ApprovalDecisionWire {
    Allow,
    AllowAlways,
    Deny,
    Cancelled,
}

impl From<ApprovalDecisionWire> for ApprovalDecision {
    fn from(d: ApprovalDecisionWire) -> Self {
        match d {
            ApprovalDecisionWire::Allow => ApprovalDecision::Allow,
            ApprovalDecisionWire::AllowAlways => ApprovalDecision::AllowAlways,
            ApprovalDecisionWire::Deny => ApprovalDecision::Deny,
            ApprovalDecisionWire::Cancelled => ApprovalDecision::Cancelled,
        }
    }
}

impl From<ApprovalDecision> for ApprovalDecisionWire {
    fn from(d: ApprovalDecision) -> Self {
        match d {
            ApprovalDecision::Allow => ApprovalDecisionWire::Allow,
            ApprovalDecision::AllowAlways => ApprovalDecisionWire::AllowAlways,
            ApprovalDecision::Deny => ApprovalDecisionWire::Deny,
            ApprovalDecision::Cancelled => ApprovalDecisionWire::Cancelled,
        }
    }
}

/// `GET /api/sessions/{id}/acp/replay?since=N` query string.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ReplayQuery {
    /// Last seq the client has applied.
    #[serde(default)]
    pub since: u64,
    /// Max frames to return in this page.
    #[serde(default)]
    pub limit: Option<u64>,
    /// Backward (older-first paging) cursor.
    #[serde(default)]
    pub before: Option<u64>,
    /// Optional projection selector.
    #[serde(default)]
    pub view: Option<String>,
}

/// `GET /api/sessions/{id}/acp/replay` response.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReplayResponse {
    /// Frames the client missed, in publish order.
    pub frames: Vec<AcpBroadcastFrame>,
    /// True when the requested `since` predates what's still in the
    /// buffer (the client missed events that have since been evicted).
    pub lost: bool,
    /// Highest seq the buffer has seen, even if it's been evicted.
    pub highest_seq: u64,
    /// Lowest seq still stored on disk for this session, or `None`
    /// when no events have been recorded yet.
    #[serde(default)]
    pub lowest_seq: Option<u64>,
    /// Cursor to pass back as `since` for the next page: the highest
    /// seq this page consumed (including rows that failed to
    /// deserialise, so a corrupt row can't stall a paging loop).
    #[serde(default)]
    pub next_cursor: Option<u64>,
    /// True when more events exist beyond this page within the store.
    #[serde(default)]
    pub has_more: bool,
    /// Present only when the request passed `view=rows`: the selected page
    /// of events folded through `TranscriptModel`, in place of the raw
    /// `frames`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<crate::acp::transcript::TranscriptRow>>,
}

/// `GET /api/sessions/{id}/acp/files` response.
#[derive(Debug, Serialize, Deserialize)]
pub struct FilesResponse {
    /// Relative paths (POSIX-style), sorted.
    pub files: Vec<String>,
    /// True when the walk hit the 5000-entry cap and stopped early.
    pub truncated: bool,
}

/// `GET /api/sessions/{id}/acp/context-primer?before_seq=N` query.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContextPrimerQuery {
    /// `seq` of the `SessionContextReset` event.
    pub before_seq: u64,
}

/// `GET /api/sessions/{id}/acp/context-primer` response.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContextPrimerResponse {
    /// Rendered markdown primer ready to drop into the composer.
    pub primer: String,
    pub included_event_count: usize,
    pub included_turn_count: usize,
    /// True when older turns were dropped or the newest turn was
    /// truncated within itself to fit the budget.
    pub truncated: bool,
    pub max_chars: usize,
    /// The user's most recent `UserPromptSent` text WHEN the session
    /// ended in a non-success terminal state (rate-limit or startup
    /// error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unprocessed_prompt: Option<String>,
}

/// `POST /api/sessions/{id}/acp/switch-agent` body.
#[derive(Debug, Serialize, Deserialize)]
pub struct SwitchAgentRequest {
    /// Registry key or configured custom ACP agent name (e.g.
    /// `"codex"`, `"opencode"`, `"my-custom-bridge"`).
    pub target: String,
    /// Optional model override forwarded to the new agent.
    #[serde(default)]
    pub model: Option<String>,
    /// Why the switch happened, recorded verbatim in the `AgentSwitched`
    /// event and surfaced in the transcript divider.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /api/sessions/{id}/acp/switch-agent` response.
#[derive(Debug, Serialize, Deserialize)]
pub struct SwitchAgentResponse {
    pub session_id: String,
    /// Registry key the session is now running.
    pub agent: String,
    /// Highest seq BEFORE the AgentSwitched event was emitted.
    pub before_seq: u64,
    /// The seq the AgentSwitched event was assigned.
    pub switch_seq: u64,
    /// Owned so the client side can deserialize the response (a
    /// `&'static str` field is not `DeserializeOwned`).
    pub status: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_request_defaults_attachments_when_absent() {
        let req: PromptRequest = serde_json::from_str(r#"{"text":"hello"}"#).unwrap();
        assert_eq!(req.text, "hello");
        assert!(req.attachments.is_empty());
    }

    #[test]
    fn prompt_request_accepts_prompt_id_and_id_alias() {
        let cases: [(&str, Option<&str>); 3] = [
            (r#"{"text":"hi"}"#, None),
            (r#"{"text":"hi","prompt_id":"cmp-1"}"#, Some("cmp-1")),
            (r#"{"text":"hi","id":"cmp-2"}"#, Some("cmp-2")),
        ];
        for (json, expect) in cases {
            let req: PromptRequest = serde_json::from_str(json).unwrap();
            assert_eq!(req.prompt_id.as_deref(), expect, "{json}");
        }
    }

    #[test]
    fn prompt_attachment_upload_roundtrips() {
        let req: PromptRequest = serde_json::from_str(
            r#"{"text":"see this","attachments":[{"kind":"image","mime_type":"image/png","data":"aGk=","name":"a.png"}]}"#,
        )
        .unwrap();
        assert_eq!(req.attachments.len(), 1);
        let att = &req.attachments[0];
        assert_eq!(att.kind, PromptAttachmentKind::Image);
        assert_eq!(att.mime_type, "image/png");
        assert_eq!(att.data, "aGk=");
        assert_eq!(att.name.as_deref(), Some("a.png"));
    }

    #[test]
    fn broadcast_frame_roundtrips_through_json() {
        let frame = AcpBroadcastFrame {
            session_id: "s-1".into(),
            seq: 42,
            event: Arc::new(Event::ThinkingStarted),
            worker_generation: Some(7),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: AcpBroadcastFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, "s-1");
        assert_eq!(back.seq, 42);
        assert!(matches!(*back.event, Event::ThinkingStarted));
        assert!(!json.contains("worker_generation"));
        assert_eq!(back.worker_generation, None);
    }

    #[test]
    fn broadcast_frame_strips_background_agent_output_file() {
        let frame = AcpBroadcastFrame {
            session_id: "s-1".into(),
            seq: 1,
            event: Arc::new(Event::BackgroundAgentLaunched {
                agent_id: "a1".into(),
                tool_call_id: "tc1".into(),
                description: "d".into(),
                prompt: "p".into(),
                model: "m".into(),
                output_file: "/home/user/.aoe/transcripts/a1.jsonl".into(),
                started_at: chrono::Utc::now(),
            }),
            worker_generation: None,
        };
        let json = serde_json::to_string(&frame).unwrap();
        assert!(
            !json.contains("transcripts"),
            "transcript path must not reach the client: {json}"
        );
        let back: AcpBroadcastFrame = serde_json::from_str(&json).unwrap();
        match &*back.event {
            Event::BackgroundAgentLaunched { output_file, .. } => assert_eq!(output_file, ""),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn approval_decision_wire_pascalcase() {
        let json = serde_json::to_string(&ApprovalDecisionWire::AllowAlways).unwrap();
        assert_eq!(json, "\"AllowAlways\"");
        let back: ApprovalDecisionWire = serde_json::from_str("\"Deny\"").unwrap();
        assert!(matches!(back, ApprovalDecisionWire::Deny));
    }

    #[test]
    fn resolve_approval_request_decision_field() {
        let body = serde_json::json!({ "decision": "Allow" });
        let parsed: ResolveApprovalRequest = serde_json::from_value(body).unwrap();
        assert!(matches!(parsed.decision, ApprovalDecisionWire::Allow));
    }

    #[test]
    fn switch_agent_request_optional_fields_default_to_none() {
        let body = serde_json::json!({ "target": "codex" });
        let parsed: SwitchAgentRequest = serde_json::from_value(body).unwrap();
        assert_eq!(parsed.target, "codex");
        assert!(parsed.model.is_none());
        assert!(parsed.reason.is_none());
    }

    #[test]
    fn switch_agent_request_carries_reason() {
        let body = serde_json::json!({ "target": "claude", "reason": "manual" });
        let parsed: SwitchAgentRequest = serde_json::from_value(body).unwrap();
        assert_eq!(parsed.reason.as_deref(), Some("manual"));
    }

    #[test]
    fn replay_query_defaults_limit_when_absent() {
        let query: ReplayQuery = serde_json::from_str(r#"{"since":42}"#).unwrap();
        assert_eq!(query.since, 42);
        assert_eq!(query.limit, None);
    }

    #[test]
    fn replay_response_defaults_paging_fields_when_absent() {
        let response: ReplayResponse =
            serde_json::from_str(r#"{"frames":[],"lost":false,"highest_seq":0,"lowest_seq":null}"#)
                .unwrap();
        assert_eq!(response.next_cursor, None);
        assert!(!response.has_more);
    }
}
