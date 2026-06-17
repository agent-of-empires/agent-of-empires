//! Wire-format types shared between the structured view daemon (`aoe serve`)
//! and its HTTP / WebSocket clients (web frontend, CLI structured view verbs,
//! and the TUI structured view).
//!
//! Anything sent over the wire lives here so server, client, and TUI
//! cannot drift on the JSON shape: rename a field in one place and
//! the build breaks everywhere it's consumed.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::approvals::ApprovalDecision;
use super::state::{DiffComment, Event, PromptAttachmentKind};

/// One frame on the per-AppState structured view broadcast channel: the structured view
/// session id plus the typed structured view Event. Subscribed WebSocket
/// clients filter on the session id and serialise to JSON only at the
/// WS write boundary; in-process consumers (status listener,
/// acp_session_id listener) match on the typed enum directly so a
/// rename of an `Event` variant breaks the build instead of silently
/// breaking listener behaviour.
///
/// `Arc<Event>` so the broadcast clone-per-subscriber stays cheap even
/// as the number of WS clients grows.
#[derive(Debug, Clone)]
pub struct AcpBroadcastFrame {
    pub session_id: String,
    pub seq: u64,
    pub event: Arc<Event>,
}

impl Serialize for AcpBroadcastFrame {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Custom impl so the wire format stays the same (untagged
        // event JSON) without forcing every consumer to round-trip
        // through serde_json::Value.
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("AcpBroadcastFrame", 3)?;
        s.serialize_field("session_id", &self.session_id)?;
        s.serialize_field("seq", &self.seq)?;
        s.serialize_field("event", &*self.event)?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for AcpBroadcastFrame {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Mirror of the Serialize impl. Clients need to parse frames
        // streamed over WebSocket, so the type round-trips through
        // serde even though the server only emits it.
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
        })
    }
}

/// One attachment as the web composer uploads it: the raw base64 bytes
/// inline in the prompt POST. This is the untrusted request shape; the
/// server decodes it, sniffs the magic bytes, enforces size/MIME/count
/// caps and the agent's capability gate, then maps it to an ACP
/// `ContentBlock` for the agent and a metadata-only
/// `PromptAttachmentRef` for replay. Bytes never reach the event log.
/// See #1000 / #965.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptAttachmentUpload {
    pub kind: PromptAttachmentKind,
    pub mime_type: String,
    /// Standard base64 (no `data:` URL prefix). The client strips the
    /// prefix before sending.
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
}

/// `POST /api/sessions/{id}/acp/prompt/diff-comments` body.
///
/// The "Send diff comments" dialog sends the structured review (so the
/// transcript can re-render the rich card) alongside `assembled_markdown`,
/// the exact WYSIWYG prompt the user approved in the preview. The server
/// forwards `assembled_markdown` to the agent verbatim and records both
/// in an `Event::UserDiffCommentsPrompt`. The frontend owns markdown
/// assembly (sort, headings, code-fence sizing, repo prefixes); the
/// server does not re-derive it, so the card payload and the agent-visible
/// text can never disagree.
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
}

/// PascalCase JSON variants (`Allow`, `AllowAlways`, `Deny`,
/// `Cancelled`) matching the web frontend's approval flow.
/// `Cancelled` is server-internal (synthesized when the daemon sweeps
/// orphaned approvals on attach, see #1099); clients never POST it but
/// it can appear in `Event::ApprovalResolved` payloads broadcast back
/// over WS, and the wire enum mirrors the internal one for symmetry.
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
    /// Last seq the client has applied. The endpoint returns frames
    /// strictly newer than this. Defaults to 0 (full replay).
    #[serde(default)]
    pub since: u64,
    /// Max frames to return in this page. Omitted falls back to the
    /// server's default page size; the server also clamps to a hard
    /// max. Clients paginate by passing the previous response's
    /// `next_cursor` back as `since` while `has_more` is true.
    #[serde(default)]
    pub limit: Option<u64>,
    /// Backward (older-first paging) cursor. When set, the endpoint
    /// ignores `since` and returns the `limit` events with `seq < before`
    /// that sit closest below `before`, in ascending order, so the client
    /// can render recent-first and lazily page older history as the user
    /// scrolls up. The tail (most recent page) is requested with
    /// `before = u64::MAX`. `next_cursor` then carries the lowest seq of
    /// the page, passed back as `before` for the next-older page while
    /// `has_more` is true. Forward `since`/`limit` paging is unaffected.
    #[serde(default)]
    pub before: Option<u64>,
}

/// `GET /api/sessions/{id}/acp/replay` response.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReplayResponse {
    /// Frames the client missed, in publish order. Empty when the
    /// client is already caught up.
    pub frames: Vec<AcpBroadcastFrame>,
    /// True when the requested `since` predates what's still in the
    /// buffer (the client missed events that have since been evicted).
    /// Clients should treat the conversation log as truncated and
    /// request a fresh start, e.g. by reloading.
    pub lost: bool,
    /// Highest seq the buffer has seen, even if it's been evicted.
    /// Lets the client decide whether reloading is worth it.
    pub highest_seq: u64,
    /// Lowest seq still stored on disk for this session, or `None`
    /// when no events have been recorded yet. Lets clients display the
    /// retention window in status output and detect mid-flight prunes.
    #[serde(default)]
    pub lowest_seq: Option<u64>,
    /// Cursor to pass back as `since` for the next page: the highest
    /// seq this page consumed (including rows that failed to
    /// deserialise, so a corrupt row can't stall a paging loop).
    /// `None` for an empty page. Only meaningful with `has_more`.
    #[serde(default)]
    pub next_cursor: Option<u64>,
    /// True when more events exist beyond this page within the store.
    /// Clients keep paging (advancing `since` to `next_cursor`) while
    /// this is set. Always false for an unbounded (`limit`-less) reply.
    #[serde(default)]
    pub has_more: bool,
}

/// `GET /api/sessions/{id}/acp/files` response. Workspace file
/// list for the composer's `@`-mention picker, walked from the
/// session's project root and capped at 5000 entries.
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
    /// `seq` of the `SessionContextReset` event. The primer only
    /// includes events with `seq < before_seq` so post-reset noise
    /// (the reset notice itself, any subsequent prompts) stays out.
    pub before_seq: u64,
}

/// `GET /api/sessions/{id}/acp/context-primer` response.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContextPrimerResponse {
    /// Rendered markdown primer ready to drop into the composer.
    /// Empty string when there is no prior transcript to recap.
    pub primer: String,
    pub included_event_count: usize,
    pub included_turn_count: usize,
    /// True when older turns were dropped or the newest turn was
    /// truncated within itself to fit the budget. Frontend can surface
    /// this via a "transcript was abbreviated" hint.
    pub truncated: bool,
    pub max_chars: usize,
    /// The user's most recent `UserPromptSent` text WHEN the session
    /// ended in a non-success terminal state (rate-limit or startup
    /// error). The prompt never reached the agent, so it does not
    /// belong in the transcript recap; the frontend can drop it back
    /// into the composer as the user's pending request. None for
    /// normal recap cases. See #1281 / #1282.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unprocessed_prompt: Option<String>,
}

/// `POST /api/sessions/{id}/acp/switch-agent` body.
#[derive(Debug, Serialize, Deserialize)]
pub struct SwitchAgentRequest {
    /// Registry key of the target ACP agent (e.g. `"codex"`,
    /// `"opencode"`). Must exist in the structured view agent registry; an
    /// unknown name returns 400.
    pub target: String,
    /// Optional model override forwarded to the new agent. None falls
    /// back to the instance's existing `agent_model`.
    #[serde(default)]
    pub model: Option<String>,
    /// Why the switch happened, recorded verbatim in the `AgentSwitched`
    /// event and surfaced in the transcript divider. The rate-limit
    /// recovery flow sends `"rate_limited"`; an explicit user-initiated
    /// switch (composer control, `aoe acp switch-agent`) sends
    /// `"manual"`. Defaults to `"manual"` when omitted.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /api/sessions/{id}/acp/switch-agent` response.
#[derive(Debug, Serialize, Deserialize)]
pub struct SwitchAgentResponse {
    pub session_id: String,
    /// Registry key the session is now running.
    pub agent: String,
    /// Highest seq BEFORE the AgentSwitched event was emitted. The
    /// frontend uses this when fetching `/acp/context-primer` so
    /// the primer recaps the prior backend's transcript without
    /// including the handoff event itself.
    pub before_seq: u64,
    /// The seq the AgentSwitched event was assigned. The frontend
    /// awaits the reducer reaching this seq before showing the
    /// recovery composer prefill so the divider, state-clear, and
    /// primer prefill all land in order.
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
        // Text-only clients (and the CLI/TUI structured view HTTP client) send
        // `{"text":"..."}` with no attachments key; it must deserialise.
        let req: PromptRequest = serde_json::from_str(r#"{"text":"hello"}"#).unwrap();
        assert_eq!(req.text, "hello");
        assert!(req.attachments.is_empty());
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
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: AcpBroadcastFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session_id, "s-1");
        assert_eq!(back.seq, 42);
        assert!(matches!(*back.event, Event::ThinkingStarted));
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
        // A bare body (the rate-limit recovery modal's original shape)
        // still deserializes; model and reason are optional.
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
        // A pre-pagination client sends `{"since":N}` with no `limit`;
        // the `#[serde(default)]` must keep it parsing (None = server
        // default page).
        let query: ReplayQuery = serde_json::from_str(r#"{"since":42}"#).unwrap();
        assert_eq!(query.since, 42);
        assert_eq!(query.limit, None);
    }

    #[test]
    fn replay_response_defaults_paging_fields_when_absent() {
        // A pre-pagination response body has no `next_cursor`/`has_more`;
        // newer clients must still deserialize it with sane defaults.
        let response: ReplayResponse =
            serde_json::from_str(r#"{"frames":[],"lost":false,"highest_seq":0,"lowest_seq":null}"#)
                .unwrap();
        assert_eq!(response.next_cursor, None);
        assert!(!response.has_more);
    }
}
