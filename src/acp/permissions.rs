//! Permission UI bridge.

use chrono::Utc;

use super::approvals::{
    is_choice_list, is_destructive, Approval, ApprovalOption, Nonce, ResolvedApproval,
};
use super::state::ToolCall;

/// Build a fresh `Approval` for an incoming permission request.
pub fn build_approval(tool_call: ToolCall, options: Vec<ApprovalOption>) -> Approval {
    let destructive = is_destructive(&tool_call.name, &tool_call.args_preview);
    Approval {
        nonce: Nonce::new(),
        tool_call,
        destructive,
        choice: is_choice_list(&options),
        options,
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
    use crate::acp::approvals::ApprovalOptionKind;

    fn tool_call(name: &str, kind: &str, args_preview: &str) -> ToolCall {
        ToolCall {
            id: "tc".into(),
            name: name.into(),
            kind: kind.into(),
            args_preview: args_preview.into(),
            started_at: Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        }
    }

    #[test]
    fn build_approval_classifies_the_tool_call_and_its_options() {
        let destructive = build_approval(
            tool_call("Bash", "execute", r#"{"command":"rm -rf /tmp/x"}"#),
            Vec::new(),
        );
        assert!(destructive.destructive);
        assert!(!destructive.choice);
        assert!(destructive.resolved.is_none());
        assert!(!destructive.nonce.0.is_empty());

        let options: Vec<_> = ["Alpha", "Bravo"]
            .iter()
            .enumerate()
            .map(|(i, name)| ApprovalOption {
                option_id: format!("choice-{i}"),
                name: (*name).into(),
                kind: ApprovalOptionKind::AllowOnce,
            })
            .collect();
        let question = build_approval(tool_call("Pi select", "other", "{}"), options.clone());
        assert!(question.choice);
        assert!(!question.destructive);
        assert_eq!(question.options, options);
    }
}
