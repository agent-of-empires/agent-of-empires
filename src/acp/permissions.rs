//! Permission UI bridge.
//!
//! When an agent emits ACP `session/request_permission`, the structured view
//! creates an `Approval` (with a server-side `Nonce`) and surfaces it via
//! `state::Event::ApprovalRequested`. The client renders the approval card
//! and the user taps allow/deny. The client posts back with the nonce and
//! decision; the server resolves via `state::Event::ApprovalResolved`.
//!
//! This module isolates the bridge so the actor in `state.rs` doesn't have
//! to know about UI semantics.

use chrono::Utc;

use super::approvals::{is_destructive, Approval, Nonce, ResolvedApproval};
use super::state::ToolCall;

/// Build a fresh `Approval` for an incoming permission request. Generates
/// a server-side nonce and decides destructive/benign classification.
pub fn build_approval(tool_call: ToolCall) -> Approval {
    let destructive = is_destructive(&tool_call.name, &tool_call.args_preview);
    Approval {
        nonce: Nonce::new(),
        tool_call,
        destructive,
        requested_at: Utc::now(),
        resolved: None,
    }
}

/// Mark an approval as resolved with a decision and optional message.
pub fn resolve(
    approval: &mut Approval,
    decision: super::approvals::ApprovalDecision,
    message: Option<String>,
) {
    approval.resolved = Some(ResolvedApproval {
        decision,
        message,
        resolved_at: Utc::now(),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::approvals::ApprovalDecision;

    #[test]
    fn build_approval_marks_destructive_bash_rm() {
        let tc = ToolCall {
            id: "tc".into(),
            name: "Bash".into(),
            kind: "execute".into(),
            args_preview: r#"{"command":"rm -rf /tmp/x"}"#.into(),
            started_at: Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        };
        let a = build_approval(tc);
        assert!(a.destructive);
        assert!(a.resolved.is_none());
        assert!(!a.nonce.0.is_empty());
    }

    #[test]
    fn resolve_sets_decision_and_timestamp() {
        let tc = ToolCall {
            id: "tc".into(),
            name: "Read".into(),
            kind: "read".into(),
            args_preview: "{}".into(),
            started_at: Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        };
        let mut a = build_approval(tc);
        resolve(&mut a, ApprovalDecision::Allow, None);
        let resolved = a.resolved.unwrap();
        assert_eq!(resolved.decision, ApprovalDecision::Allow);
    }
}
