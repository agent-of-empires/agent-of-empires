//! Wire-format types shared between the cockpit daemon (`aoe serve`)
//! and its HTTP / WebSocket clients (web frontend, CLI cockpit verbs,
//! and the TUI cockpit view).
//!
//! Anything sent over the wire lives here so server, client, and TUI
//! cannot drift on the JSON shape: rename a field in one place and
//! the build breaks everywhere it's consumed.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::approvals::ApprovalDecision;
use super::state::{DiffComment, Event};

/// One frame on the per-AppState cockpit broadcast channel: the cockpit
/// session id plus the typed cockpit Event. Subscribed WebSocket
/// clients filter on the session id and serialise to JSON only at the
/// WS write boundary; in-process consumers (status listener,
/// acp_session_id listener) match on the typed enum directly so a
/// rename of an `Event` variant breaks the build instead of silently
/// breaking listener behaviour.
///
/// `Arc<Event>` so the broadcast clone-per-subscriber stays cheap even
/// as the number of WS clients grows.
#[derive(Debug, Clone)]
pub struct CockpitBroadcastFrame {
    pub session_id: String,
    pub seq: u64,
    pub event: Arc<Event>,
}

impl Serialize for CockpitBroadcastFrame {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Custom impl so the wire format stays the same (untagged
        // event JSON) without forcing every consumer to round-trip
        // through serde_json::Value.
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("CockpitBroadcastFrame", 3)?;
        s.serialize_field("session_id", &self.session_id)?;
        s.serialize_field("seq", &self.seq)?;
        s.serialize_field("event", &*self.event)?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for CockpitBroadcastFrame {
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
        Ok(CockpitBroadcastFrame {
            session_id: w.session_id,
            seq: w.seq,
            event: Arc::new(w.event),
        })
    }
}

/// `POST /api/sessions/{id}/cockpit/prompt` body.
#[derive(Debug, Serialize, Deserialize)]
pub struct PromptRequest {
    pub text: String,
}

/// `POST /api/sessions/{id}/cockpit/prompt/diff-comments` body.
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

/// `POST /api/sessions/{id}/cockpit/approvals/{nonce}` body.
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

/// `GET /api/sessions/{id}/cockpit/replay?since=N` query string.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ReplayQuery {
    /// Last seq the client has applied. The endpoint returns frames
    /// strictly newer than this. Defaults to 0 (full replay).
    #[serde(default)]
    pub since: u64,
}

/// `GET /api/sessions/{id}/cockpit/replay` response.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReplayResponse {
    /// Frames the client missed, in publish order. Empty when the
    /// client is already caught up.
    pub frames: Vec<CockpitBroadcastFrame>,
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
}

/// `GET /api/sessions/{id}/cockpit/context-primer?before_seq=N` query.
#[derive(Debug, Serialize, Deserialize)]
pub struct ContextPrimerQuery {
    /// `seq` of the `SessionContextReset` event. The primer only
    /// includes events with `seq < before_seq` so post-reset noise
    /// (the reset notice itself, any subsequent prompts) stays out.
    pub before_seq: u64,
}

/// `GET /api/sessions/{id}/cockpit/context-primer` response.
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

/// `POST /api/sessions/{id}/cockpit/switch-agent` body.
#[derive(Debug, Serialize, Deserialize)]
pub struct SwitchAgentRequest {
    /// Registry key of the target ACP agent (e.g. `"codex"`,
    /// `"opencode"`). Must exist in the cockpit agent registry; an
    /// unknown name returns 400.
    pub target: String,
    /// Optional model override forwarded to the new agent. None falls
    /// back to the instance's existing `cockpit_model`.
    #[serde(default)]
    pub model: Option<String>,
}

/// `POST /api/sessions/{id}/cockpit/switch-agent` response.
#[derive(Debug, Serialize, Deserialize)]
pub struct SwitchAgentResponse {
    pub session_id: String,
    /// Registry key the session is now running.
    pub agent: String,
    /// Highest seq BEFORE the AgentSwitched event was emitted. The
    /// frontend uses this when fetching `/cockpit/context-primer` so
    /// the primer recaps the prior backend's transcript without
    /// including the handoff event itself.
    pub before_seq: u64,
    /// The seq the AgentSwitched event was assigned. The frontend
    /// awaits the reducer reaching this seq before showing the
    /// recovery composer prefill so the divider, state-clear, and
    /// primer prefill all land in order.
    pub switch_seq: u64,
    pub status: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcast_frame_roundtrips_through_json() {
        let frame = CockpitBroadcastFrame {
            session_id: "s-1".into(),
            seq: 42,
            event: Arc::new(Event::ThinkingStarted),
        };
        let json = serde_json::to_string(&frame).unwrap();
        let back: CockpitBroadcastFrame = serde_json::from_str(&json).unwrap();
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
}
