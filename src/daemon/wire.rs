//! Shared wire types for the daemon REST API.

use serde::{Deserialize, Serialize};

use crate::session::SessionScope;
/// Which ACP `ContentBlock` an attachment maps to; the lowercase form is the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptAttachmentKind {
    Image,
    Audio,
    Resource,
}

impl PromptAttachmentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Resource => "resource",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "image" => Some(Self::Image),
            "audio" => Some(Self::Audio),
            "resource" => Some(Self::Resource),
            _ => None,
        }
    }
}

/// Metadata only: the blob is fetched lazily from `GET /acp/attachments/{id}` so the event
/// log stays small.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptAttachmentRef {
    pub id: String,
    pub kind: PromptAttachmentKind,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub size: u64,
}

/// One queued follow-up prompt, owned by the daemon so it drains with no tab open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedPromptEntry {
    /// Client-minted, stable across edits; also the optimistic-echo reconcile key.
    pub id: String,
    pub seq: u64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<PromptAttachmentRef>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_device: Option<String>,
}

/// Ephemeral worker lifecycle; never persisted to the event log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpWorkerState {
    #[default]
    Absent,
    Resuming,
    Running,
    Stopping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextResumeUnavailableReason {
    AgentUnsupported,
    SandboxUnsupported,
    CommandUnsupported,
    ForcedFresh,
    InvalidTarget,
    ForkPending,
    PreviousFailure,
    NoTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextResumeIndeterminateReason {
    RuntimeCheckRequired,
    AgentHandshakeRequired,
}

/// Whether context survives a future lifecycle transition, not current start eligibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ContextResumeAvailability {
    Available,
    Indeterminate {
        reason: ContextResumeIndeterminateReason,
    },
    Unavailable {
        reason: ContextResumeUnavailableReason,
    },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PendingApproval {
    pub nonce: String,
    pub tool_name: String,
    pub target: String,
    pub destructive: bool,
    /// Answers must be picked from the labeled options, not answered by kind.
    #[serde(default)]
    pub choice: bool,
}

/// Decoding requires only `id`; every other field defaults so an older daemon cannot break the list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResponse {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub project_path: String,
    #[serde(default)]
    pub artifact_dir: String,
    #[serde(default)]
    pub group_path: String,
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub dormant: bool,
    #[serde(default)]
    pub yolo_mode: bool,
    #[serde(default)]
    pub created_at: String,
    pub last_accessed_at: Option<String>,
    /// Unlike `last_accessed_at`, viewing or messaging a session leaves this alone.
    pub idle_entered_at: Option<String>,
    pub last_error: Option<String>,
    pub branch: Option<String>,
    pub main_repo_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    /// Overrides `base_branch`, the profile default, and auto-detection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_branch_override: Option<String>,
    #[serde(default)]
    pub is_sandboxed: bool,
    #[serde(default)]
    pub scratch: bool,
    #[serde(default)]
    pub favorited: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default)]
    pub urgent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snoozed_until: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trashed_at: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unread: bool,
    /// Single-repo worktrees only; use `has_cleanable_worktree` for delete cleanup.
    #[serde(default)]
    pub has_managed_worktree: bool,
    /// Covers single-repo worktrees and multi-repo workspaces.
    #[serde(default)]
    pub has_cleanable_worktree: bool,
    #[serde(default)]
    pub tie_workdir_to_name: bool,
    #[serde(default)]
    pub smart_rename: crate::session::smart_rename::SmartRenameState,
    #[serde(default)]
    pub default_name: bool,
    #[serde(default)]
    pub has_terminal: bool,
    #[serde(default)]
    pub profile: String,
    #[serde(default)]
    pub cleanup_defaults: CleanupDefaults,
    pub remote_owner: Option<String>,
    /// "owner@host", so same-named owners on different hosts never merge.
    pub remote_owner_key: Option<String>,
    /// `None` inherits the server-wide `web.notify_on_*` default.
    pub notify_on_waiting: Option<bool>,
    pub notify_on_idle: Option<bool>,
    pub notify_on_error: Option<bool>,
    #[serde(default, skip_serializing_if = "crate::session::View::is_terminal")]
    pub view: crate::session::View,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_resume: Option<ContextResumeAvailability>,
    #[serde(default)]
    pub acp_worker_state: AcpWorkerState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_approvals: Vec<PendingApproval>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<crate::acp::state::RateLimitInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_auto_resume: Option<bool>,
    #[serde(default)]
    pub acp_capable: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued_prompts: Vec<QueuedPromptEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acp_agent: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub acp_can_fork: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub keeps_context: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clear_aliases: Vec<String>,
    #[serde(default)]
    pub claude_fullscreen: bool,
    #[serde(default)]
    pub workspace_repos: Vec<WorkspaceRepoSummary>,
    /// Response-only; not persisted, so omitted from list and fetch responses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_summary: Option<PlanSummary>,
    /// Cleared once a `UserPromptSent` lands after the scheduling tool call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_wakeup_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_wakeup_reason: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub monitor_active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor_description: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct PlanSummary {
    pub current_step_title: Option<String>,
    pub completed: u32,
    pub total: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRepoSummary {
    pub name: String,
    pub source_path: String,
    pub branch: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CleanupDefaults {
    pub delete_worktree: bool,
    pub delete_branch: bool,
    pub delete_sandbox: bool,
    pub delete_to_trash: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionsEnvelope {
    pub sessions: Vec<SessionResponse>,
    #[serde(default)]
    pub workspace_ordering: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ListSessionsQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<SessionScope>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_response_decodes_with_only_an_id() {
        let row: SessionResponse = serde_json::from_str(r#"{"id":"a"}"#).unwrap();
        assert_eq!(row.id, "a");
        assert_eq!(row.status, "");
        assert_eq!(row.view, crate::session::View::Terminal);
        assert_eq!(row.acp_worker_state, AcpWorkerState::Absent);
        assert!(!row.cleanup_defaults.delete_to_trash);
        assert!(row.workspace_repos.is_empty());
        assert_eq!(row.context_resume, None);

        assert!(serde_json::from_str::<SessionResponse>(r#"{"title":"no id"}"#).is_err());
    }

    #[test]
    fn sessions_envelope_decodes_without_workspace_ordering() {
        let envelope: SessionsEnvelope =
            serde_json::from_str(r#"{"sessions":[{"id":"a"}]}"#).unwrap();
        assert_eq!(envelope.sessions.len(), 1);
        assert!(envelope.workspace_ordering.is_empty());
    }
}
