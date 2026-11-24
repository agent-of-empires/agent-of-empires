//! Fixed error discriminators shared by native clients and daemon handlers.

use reqwest::{header::HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};

pub const ERROR_CODE_HEADER: &str = "aoe-error-code";
pub const MANAGED_RESUME_CAUSE_HEADER: &str = "aoe-managed-resume-cause";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiErrorCode {
    ReadOnly,
    ElevationRequired,
    AccessPolicyDenied,
    CityhallMode,
    LocalPluginSourceRequired,
    PluginPreviewFailed,
    PluginJobActive,
    PluginAlreadyInstalled,
    LifecycleLocked,
    TrashIncomplete,
    LifecycleSuperseded,
    PendingTargetGone,
    TlsRequired,
    RuntimeEpochMismatch,
    ResumeFailed,
    ManagedResumeRefused(ManagedResumeCause),
    StartFailed,
    SandboxMigrationDeferred,
    SandboxMigrationFailed,
    PurgeFailed,
    NoRevive,
    CreationTrustChanged,
    CreationCancelled,
    CreationNotPending,
    CreateHookFailed,
    CreateOutcomeUnknown,
    /// The project path is not a directory on the Daemon host.
    WorkingDirMissing,
    /// A worktree checkout named a branch that exists neither locally nor on a remote.
    BranchNotFound,
    AgentHooksNotApproved,
    SmartRenameSkipped,
    SmartRenameFailed,
    SmartRenameTimeout,
    GroupNotEmpty,
    SessionNotRunning,
    SessionArchived,
    SessionTrashed,
    AccountSwapUnavailable,
    ModelPinned,
    UnknownAgent,
    TerminalSwitchRefused(TerminalSwitchReason),
}

impl ApiErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::ElevationRequired => "elevation_required",
            Self::AccessPolicyDenied => "access_policy_denied",
            Self::CityhallMode => "cityhall_mode",
            Self::LocalPluginSourceRequired => "local_plugin_source_required",
            Self::PluginPreviewFailed => "preview_failed",
            Self::PluginJobActive => "plugin_job_active",
            Self::PluginAlreadyInstalled => "plugin_already_installed",
            Self::LifecycleLocked => "lifecycle_locked",
            Self::TrashIncomplete => "trash_incomplete",
            Self::LifecycleSuperseded => "lifecycle_superseded",
            Self::PendingTargetGone => "pending_target_gone",
            Self::StartFailed => "start_failed",
            Self::SandboxMigrationDeferred => "sandbox_migration_deferred",
            Self::SandboxMigrationFailed => "sandbox_migration_failed",
            Self::PurgeFailed => "purge_failed",
            Self::TlsRequired => "tls_required",
            Self::RuntimeEpochMismatch => "runtime_epoch_mismatch",
            Self::ResumeFailed => "resume_failed",
            Self::ManagedResumeRefused(_) => "managed_resume_refused",
            Self::NoRevive => "no_revive",
            Self::CreationTrustChanged => "creation_trust_changed",
            Self::CreationCancelled => "creation_cancelled",
            Self::CreationNotPending => "creation_not_pending",
            Self::CreateHookFailed => "create_hook_failed",
            Self::CreateOutcomeUnknown => "create_outcome_unknown",
            Self::WorkingDirMissing => "working_dir_missing",
            Self::BranchNotFound => "branch_not_found",
            Self::AgentHooksNotApproved => "agent_hooks_not_approved",
            Self::SmartRenameSkipped => "smart_rename_skipped",
            Self::SmartRenameFailed => "smart_rename_failed",
            Self::SmartRenameTimeout => "smart_rename_timeout",
            Self::GroupNotEmpty => "group_not_empty",
            Self::SessionNotRunning => "session_not_running",
            Self::SessionArchived => "session_archived",
            Self::SessionTrashed => "session_trashed",
            Self::AccountSwapUnavailable => "account_swap_unavailable",
            Self::ModelPinned => "model_pinned",
            Self::UnknownAgent => "unknown_agent",
            Self::TerminalSwitchRefused(reason) => reason.as_str(),
        }
    }

    pub const fn status(self) -> StatusCode {
        match self {
            Self::ReadOnly
            | Self::ElevationRequired
            | Self::AccessPolicyDenied
            | Self::LocalPluginSourceRequired
            | Self::CityhallMode => StatusCode::FORBIDDEN,
            Self::LifecycleLocked
            | Self::TrashIncomplete
            | Self::LifecycleSuperseded
            | Self::RuntimeEpochMismatch
            | Self::ResumeFailed
            | Self::ManagedResumeRefused(_)
            | Self::SandboxMigrationDeferred
            | Self::PurgeFailed
            | Self::NoRevive
            | Self::CreationTrustChanged
            | Self::CreationCancelled
            | Self::CreationNotPending
            | Self::CreateOutcomeUnknown
            | Self::AgentHooksNotApproved
            | Self::PluginJobActive
            | Self::PluginAlreadyInstalled
            | Self::GroupNotEmpty
            | Self::SessionNotRunning
            | Self::SessionArchived
            | Self::SessionTrashed
            | Self::AccountSwapUnavailable
            | Self::ModelPinned => StatusCode::CONFLICT,
            Self::PendingTargetGone => StatusCode::NOT_FOUND,
            Self::TlsRequired => StatusCode::UPGRADE_REQUIRED,
            Self::CreateHookFailed
            | Self::WorkingDirMissing
            | Self::BranchNotFound
            | Self::UnknownAgent => StatusCode::BAD_REQUEST,
            Self::SmartRenameSkipped => StatusCode::CONFLICT,
            Self::SmartRenameFailed | Self::PluginPreviewFailed => StatusCode::BAD_GATEWAY,
            Self::SmartRenameTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::StartFailed | Self::SandboxMigrationFailed => StatusCode::INTERNAL_SERVER_ERROR,
            Self::TerminalSwitchRefused(
                TerminalSwitchReason::HandoffPending | TerminalSwitchReason::ContextUnconfirmed,
            ) => StatusCode::SERVICE_UNAVAILABLE,
            Self::TerminalSwitchRefused(_) => StatusCode::CONFLICT,
        }
    }

    pub const fn sandbox_migration_message(self) -> Option<&'static str> {
        match self {
            Self::SandboxMigrationDeferred => Some(
                "Sandbox migration is pending. Stop other sandbox users, then retry. For bulk migration, stop the daemon and run `aoe migrate`.",
            ),
            Self::SandboxMigrationFailed => Some(
                "Sandbox migration failed. Apply the canonical state and check daemon logs before retrying.",
            ),
            _ => None,
        }
    }

    pub const fn header(self) -> [(&'static str, &'static str); 1] {
        [(ERROR_CODE_HEADER, self.as_str())]
    }

    /// Reject ambiguity and mismatched status or endpoint before assigning meaning.
    pub fn from_headers(
        status: StatusCode,
        headers: &HeaderMap,
        resolving_pending_target: bool,
    ) -> Option<Self> {
        let mut values = headers.get_all(ERROR_CODE_HEADER).iter();
        let value = values.next()?;
        if values.next().is_some() {
            return None;
        }
        let code = match value.as_bytes() {
            b"read_only" => Self::ReadOnly,
            b"elevation_required" => Self::ElevationRequired,
            b"access_policy_denied" => Self::AccessPolicyDenied,
            b"cityhall_mode" => Self::CityhallMode,
            b"local_plugin_source_required" => Self::LocalPluginSourceRequired,
            b"preview_failed" => Self::PluginPreviewFailed,
            b"plugin_job_active" => Self::PluginJobActive,
            b"plugin_already_installed" => Self::PluginAlreadyInstalled,
            b"lifecycle_locked" => Self::LifecycleLocked,
            b"trash_incomplete" => Self::TrashIncomplete,
            b"lifecycle_superseded" => Self::LifecycleSuperseded,
            b"pending_target_gone" if resolving_pending_target => Self::PendingTargetGone,
            b"tls_required" => Self::TlsRequired,
            b"runtime_epoch_mismatch" => Self::RuntimeEpochMismatch,
            b"create_outcome_unknown" => Self::CreateOutcomeUnknown,
            b"agent_hooks_not_approved" => Self::AgentHooksNotApproved,
            b"resume_failed" => Self::ResumeFailed,
            b"managed_resume_refused" => Self::ManagedResumeRefused(managed_resume_cause(headers)),
            b"no_revive" => Self::NoRevive,
            b"creation_trust_changed" => Self::CreationTrustChanged,
            b"creation_cancelled" => Self::CreationCancelled,
            b"start_failed" => Self::StartFailed,
            b"sandbox_migration_deferred" => Self::SandboxMigrationDeferred,
            b"sandbox_migration_failed" => Self::SandboxMigrationFailed,
            b"purge_failed" => Self::PurgeFailed,
            b"creation_not_pending" => Self::CreationNotPending,
            b"create_hook_failed" => Self::CreateHookFailed,
            b"working_dir_missing" => Self::WorkingDirMissing,
            b"branch_not_found" => Self::BranchNotFound,
            b"smart_rename_skipped" => Self::SmartRenameSkipped,
            b"smart_rename_failed" => Self::SmartRenameFailed,
            b"smart_rename_timeout" => Self::SmartRenameTimeout,
            b"group_not_empty" => Self::GroupNotEmpty,
            b"session_not_running" => Self::SessionNotRunning,
            b"session_archived" => Self::SessionArchived,
            b"session_trashed" => Self::SessionTrashed,
            b"account_swap_unavailable" => Self::AccountSwapUnavailable,
            b"model_pinned" => Self::ModelPinned,
            b"unknown_agent" => Self::UnknownAgent,
            other => Self::TerminalSwitchRefused(
                TerminalSwitchReason::ALL
                    .into_iter()
                    .find(|reason| reason.as_str().as_bytes() == other)?,
            ),
        };
        (status == code.status()).then_some(code)
    }
}

fn managed_resume_cause(headers: &HeaderMap) -> ManagedResumeCause {
    let mut values = headers.get_all(MANAGED_RESUME_CAUSE_HEADER).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => match value.as_bytes() {
            b"wrapper_contract_required" => ManagedResumeCause::WrapperContractRequired,
            b"unsupported_managed_arguments" => ManagedResumeCause::UnsupportedManagedArguments,
            b"opencode_schema_unattested" => ManagedResumeCause::OpenCodeSchemaUnattested,
            b"pi_transcript_binding_required" => ManagedResumeCause::PiTranscriptBindingRequired,
            b"omp_transcript_binding_required" => ManagedResumeCause::OmpTranscriptBindingRequired,
            b"codex_conversation_unverified" => ManagedResumeCause::CodexConversationUnverified,
            _ => ManagedResumeCause::ExecutionContextUnattested,
        },
        _ => ManagedResumeCause::ExecutionContextUnattested,
    }
}

macro_rules! fresh_recovery_footer {
    () => {
        concat!(
            "Or choose fresh (abandons this AoE association):\n",
            "aoe session set-session-id <session> \"\"\n",
            "Then start/restart; old history is not deleted or recovered."
        )
    };
}

macro_rules! recovery_message {
    ($reason:literal) => {
        concat!(
            $reason,
            "\nConversation preserved. Restore original context, then retry:\n",
            "aoe session set-session-id <session> <native-id>\n",
            "Use --store with the exact store/transcript where required.\n",
            fresh_recovery_footer!()
        )
    };
}

macro_rules! transcript_recovery_message {
    ($agent:literal) => {
        concat!(
            "Managed resume refused: ",
            $agent,
            " transcript file is not bound.\n",
            "The native session ID alone is not enough to authorize resume.\n",
            "This refusal does not delete the saved conversation.\n",
            "Manual transcript rebinding is not supported for sandboxed sessions.\n",
            "If store routing changed, restore the original store configuration.\n",
            "For host sessions, bind the exact existing transcript file:\n",
            "aoe session set-session-id <session> <native-id> --store /absolute/path/to/transcript.jsonl\n",
            "For ",
            $agent,
            ", --store must name the file, not its directory.\n",
            fresh_recovery_footer!()
        )
    };
}

/// Finite reasons safe to present without exposing native diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ManagedResumeCause {
    #[error("wrapper execution contract is missing")]
    WrapperContractRequired,
    #[error("managed command or arguments cannot be attested")]
    UnsupportedManagedArguments,
    #[error("OpenCode session schema cannot be attested")]
    OpenCodeSchemaUnattested,
    #[error("Pi exact transcript binding is missing")]
    PiTranscriptBindingRequired,
    #[error("OMP exact transcript binding is missing")]
    OmpTranscriptBindingRequired,
    #[error("Codex conversation cannot be verified in this context")]
    CodexConversationUnverified,
    #[error("native execution context cannot be attested")]
    ExecutionContextUnattested,
}

impl ManagedResumeCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WrapperContractRequired => "wrapper_contract_required",
            Self::UnsupportedManagedArguments => "unsupported_managed_arguments",
            Self::OpenCodeSchemaUnattested => "opencode_schema_unattested",
            Self::PiTranscriptBindingRequired => "pi_transcript_binding_required",
            Self::OmpTranscriptBindingRequired => "omp_transcript_binding_required",
            Self::CodexConversationUnverified => "codex_conversation_unverified",
            Self::ExecutionContextUnattested => "execution_context_unattested",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::WrapperContractRequired => recovery_message!(
                "Managed resume refused: wrapper contract missing.\n\
                 Restore truthful session.agent_execution_as and\n\
                 session.agent_config_dir in trusted global/profile config.\n\
                 Both must match the original agent and store."
            ),
            Self::UnsupportedManagedArguments => recovery_message!(
                "Managed resume refused: unsupported command or arguments.\n\
                 Remove native selectors and unsupported context overrides."
            ),
            Self::OpenCodeSchemaUnattested => recovery_message!(
                "Managed resume refused: OpenCode schema cannot be attested.\n\
                 Restore a verified release/build and its original database."
            ),
            Self::PiTranscriptBindingRequired => transcript_recovery_message!("Pi"),
            Self::OmpTranscriptBindingRequired => transcript_recovery_message!("OMP"),
            Self::CodexConversationUnverified => concat!(
                "Managed resume refused: cannot verify this Codex conversation here.\n",
                "It needs a rollout recording this ID and project, in a sandbox's private store\n",
                "or on a host session with no session.agent_config_dir, Codex profile, or\n",
                "alternate store routing. A store over AoE's scan limit cannot be verified.\n",
                "Conversation preserved. Restore that context, then retry.\n",
                "Or copy agent_session_id from aoe session show <session> --json and\n",
                "continue inside Codex (AoE drops this association):\n",
                "aoe session set-session-id <session> \"\"\n",
                "Restart, then run /resume <native-id> once Codex is idle."
            ),
            Self::ExecutionContextUnattested => recovery_message!(
                "Managed resume refused: execution context cannot be attested.\n\
                 Restore the original agent configuration and store."
            ),
        }
    }
}

/// Why a switch to Terminal view starts fresh, can't happen yet, or continues a
/// conversation other than the one in Structured view. Also the `AoE-Error-Code`
/// of a refused switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalSwitchReason {
    AgentNotResumable,
    NoConversationYet,
    /// With `continues`: Structured view has no conversation yet, so the switch
    /// resumes the Session's earlier one, from before Structured view.
    EarlierConversation,
    ConversationNotBound,
    Sandboxed,
    HandoffPending,
    NoTerminalTool,
    ContextUnconfirmed,
    /// Refusal only: the Session changed during the switch.
    SessionChanged,
    /// Check only: the Session isn't in Structured view.
    NotStructured,
    /// With `command_decides`: the launch command carries a native session selector.
    CommandSelectsConversation,
    /// With `blocked`: the command's native session selector and AoE's own conversation
    /// would both reach the launch, which refuses them together.
    CommandSelectorConflict,
}

impl TerminalSwitchReason {
    pub const ALL: [Self; 12] = [
        Self::AgentNotResumable,
        Self::NoConversationYet,
        Self::EarlierConversation,
        Self::ConversationNotBound,
        Self::Sandboxed,
        Self::HandoffPending,
        Self::NoTerminalTool,
        Self::ContextUnconfirmed,
        Self::SessionChanged,
        Self::NotStructured,
        Self::CommandSelectsConversation,
        Self::CommandSelectorConflict,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentNotResumable => "agent_not_resumable",
            Self::NoConversationYet => "no_conversation_yet",
            Self::EarlierConversation => "earlier_conversation",
            Self::ConversationNotBound => "conversation_not_bound",
            Self::Sandboxed => "sandboxed",
            Self::HandoffPending => "handoff_pending",
            Self::NoTerminalTool => "no_terminal_tool",
            Self::ContextUnconfirmed => "context_unconfirmed",
            Self::SessionChanged => "session_changed",
            Self::NotStructured => "not_structured",
            Self::CommandSelectsConversation => "command_selects_conversation",
            Self::CommandSelectorConflict => "command_selector_conflict",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    #[test]
    fn error_classification_rejects_ambiguous_or_out_of_context_headers() {
        for (status, value, resolving, expected) in [
            (
                StatusCode::FORBIDDEN,
                "read_only",
                false,
                Some(ApiErrorCode::ReadOnly),
            ),
            (StatusCode::CONFLICT, "read_only", false, None),
            (
                StatusCode::FORBIDDEN,
                "elevation_required",
                false,
                Some(ApiErrorCode::ElevationRequired),
            ),
            (StatusCode::UNAUTHORIZED, "elevation_required", false, None),
            (
                StatusCode::FORBIDDEN,
                "elevation_required, read_only",
                false,
                None,
            ),
            (
                StatusCode::FORBIDDEN,
                "read_only, cityhall_mode",
                false,
                None,
            ),
            (StatusCode::FORBIDDEN, "READ_ONLY", false, None),
            (StatusCode::NOT_FOUND, "pending_target_gone", false, None),
            (
                StatusCode::NOT_FOUND,
                "pending_target_gone",
                true,
                Some(ApiErrorCode::PendingTargetGone),
            ),
            (
                StatusCode::CONFLICT,
                "runtime_epoch_mismatch",
                false,
                Some(ApiErrorCode::RuntimeEpochMismatch),
            ),
            (
                StatusCode::CONFLICT,
                "no_revive",
                false,
                Some(ApiErrorCode::NoRevive),
            ),
            (StatusCode::FORBIDDEN, "no_revive", false, None),
            (
                StatusCode::CONFLICT,
                "lifecycle_superseded",
                false,
                Some(ApiErrorCode::LifecycleSuperseded),
            ),
            (
                StatusCode::CONFLICT,
                "session_not_running",
                false,
                Some(ApiErrorCode::SessionNotRunning),
            ),
            (
                StatusCode::CONFLICT,
                "session_archived",
                false,
                Some(ApiErrorCode::SessionArchived),
            ),
            (
                StatusCode::CONFLICT,
                "session_trashed",
                false,
                Some(ApiErrorCode::SessionTrashed),
            ),
            (StatusCode::FORBIDDEN, "session_archived", false, None),
            (
                StatusCode::CONFLICT,
                "managed_resume_refused",
                false,
                Some(ApiErrorCode::ManagedResumeRefused(
                    ManagedResumeCause::ExecutionContextUnattested,
                )),
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "managed_resume_refused",
                false,
                None,
            ),
            (
                StatusCode::BAD_REQUEST,
                "create_hook_failed",
                false,
                Some(ApiErrorCode::CreateHookFailed),
            ),
            (
                StatusCode::BAD_REQUEST,
                "working_dir_missing",
                false,
                Some(ApiErrorCode::WorkingDirMissing),
            ),
            (
                StatusCode::BAD_REQUEST,
                "branch_not_found",
                false,
                Some(ApiErrorCode::BranchNotFound),
            ),
            (StatusCode::CONFLICT, "branch_not_found", false, None),
            (
                StatusCode::CONFLICT,
                "group_not_empty",
                false,
                Some(ApiErrorCode::GroupNotEmpty),
            ),
            (StatusCode::FORBIDDEN, "group_not_empty", false, None),
            (
                StatusCode::BAD_GATEWAY,
                "preview_failed",
                false,
                Some(ApiErrorCode::PluginPreviewFailed),
            ),
            (StatusCode::CONFLICT, "preview_failed", false, None),
            (
                StatusCode::CONFLICT,
                "plugin_job_active",
                false,
                Some(ApiErrorCode::PluginJobActive),
            ),
            (StatusCode::BAD_GATEWAY, "plugin_job_active", false, None),
            (
                StatusCode::CONFLICT,
                "account_swap_unavailable",
                false,
                Some(ApiErrorCode::AccountSwapUnavailable),
            ),
            (
                StatusCode::CONFLICT,
                "model_pinned",
                false,
                Some(ApiErrorCode::ModelPinned),
            ),
            (StatusCode::BAD_REQUEST, "model_pinned", false, None),
            (
                StatusCode::BAD_REQUEST,
                "unknown_agent",
                false,
                Some(ApiErrorCode::UnknownAgent),
            ),
            (StatusCode::CONFLICT, "unknown_agent", false, None),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(ERROR_CODE_HEADER, HeaderValue::from_static(value));
            assert_eq!(
                ApiErrorCode::from_headers(status, &headers, resolving),
                expected
            );
            headers.append(ERROR_CODE_HEADER, HeaderValue::from_static(value));
            assert_eq!(
                ApiErrorCode::from_headers(status, &headers, resolving),
                None
            );
        }
    }

    #[test]
    fn every_terminal_switch_reason_decodes_only_with_its_status() {
        use serde::Deserialize;
        use TerminalSwitchReason::*;
        // Every variant in declaration order, from the derived Deserialize, so `ALL` can't drop one.
        let variants: Vec<_> = (0..)
            .map_while(|index| {
                TerminalSwitchReason::deserialize(serde::de::value::U32Deserializer::<
                    serde::de::value::Error,
                >::new(index))
                .ok()
            })
            .collect();
        assert_eq!(variants, TerminalSwitchReason::ALL);
        for reason in variants {
            let (wire, status) = match reason {
                AgentNotResumable => ("agent_not_resumable", StatusCode::CONFLICT),
                NoConversationYet => ("no_conversation_yet", StatusCode::CONFLICT),
                EarlierConversation => ("earlier_conversation", StatusCode::CONFLICT),
                ConversationNotBound => ("conversation_not_bound", StatusCode::CONFLICT),
                Sandboxed => ("sandboxed", StatusCode::CONFLICT),
                HandoffPending => ("handoff_pending", StatusCode::SERVICE_UNAVAILABLE),
                NoTerminalTool => ("no_terminal_tool", StatusCode::CONFLICT),
                ContextUnconfirmed => ("context_unconfirmed", StatusCode::SERVICE_UNAVAILABLE),
                SessionChanged => ("session_changed", StatusCode::CONFLICT),
                NotStructured => ("not_structured", StatusCode::CONFLICT),
                CommandSelectsConversation => {
                    ("command_selects_conversation", StatusCode::CONFLICT)
                }
                CommandSelectorConflict => ("command_selector_conflict", StatusCode::CONFLICT),
            };
            assert_eq!(reason.as_str(), wire);
            let code = ApiErrorCode::TerminalSwitchRefused(reason);
            let mut headers = HeaderMap::new();
            headers.insert(ERROR_CODE_HEADER, HeaderValue::from_static(wire));
            assert_eq!(
                ApiErrorCode::from_headers(status, &headers, false),
                Some(code),
                "{wire}"
            );
            let other = if status == StatusCode::CONFLICT {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::CONFLICT
            };
            assert_eq!(
                ApiErrorCode::from_headers(other, &headers, false),
                None,
                "{wire}"
            );
        }
    }

    #[test]
    fn managed_resume_cause_headers_cannot_expand_or_erase_a_refusal() {
        use ManagedResumeCause::*;
        for (value, expected) in [
            ("wrapper_contract_required", WrapperContractRequired),
            ("unsupported_managed_arguments", UnsupportedManagedArguments),
            ("opencode_schema_unattested", OpenCodeSchemaUnattested),
            (
                "pi_transcript_binding_required",
                PiTranscriptBindingRequired,
            ),
            (
                "omp_transcript_binding_required",
                OmpTranscriptBindingRequired,
            ),
            ("codex_conversation_unverified", CodexConversationUnverified),
            ("execution_context_unattested", ExecutionContextUnattested),
            ("secret-token", ExecutionContextUnattested),
            (
                "wrapper_contract_required, secret-token",
                ExecutionContextUnattested,
            ),
            (
                "omp_transcript_binding_required, execution_context_unattested",
                ExecutionContextUnattested,
            ),
            (
                "pi_transcript_binding_required, execution_context_unattested",
                ExecutionContextUnattested,
            ),
            ("", ExecutionContextUnattested),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(MANAGED_RESUME_CAUSE_HEADER, HeaderValue::from_static(value));
            assert_eq!(
                ApiErrorCode::from_headers(StatusCode::CONFLICT, &headers, false),
                None
            );
            headers.insert(
                ERROR_CODE_HEADER,
                HeaderValue::from_static("managed_resume_refused"),
            );
            assert_eq!(
                ApiErrorCode::from_headers(StatusCode::CONFLICT, &headers, false),
                Some(ApiErrorCode::ManagedResumeRefused(expected)),
            );
            assert_eq!(
                ApiErrorCode::from_headers(StatusCode::BAD_REQUEST, &headers, false),
                None
            );
            headers.append(MANAGED_RESUME_CAUSE_HEADER, HeaderValue::from_static(value));
            assert_eq!(
                ApiErrorCode::from_headers(StatusCode::CONFLICT, &headers, false),
                Some(ApiErrorCode::ManagedResumeRefused(
                    ExecutionContextUnattested
                )),
            );
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            ERROR_CODE_HEADER,
            HeaderValue::from_static("managed_resume_refused"),
        );
        headers.insert(
            MANAGED_RESUME_CAUSE_HEADER,
            HeaderValue::from_bytes(b"\xff").unwrap(),
        );
        assert_eq!(
            ApiErrorCode::from_headers(StatusCode::CONFLICT, &headers, false),
            Some(ApiErrorCode::ManagedResumeRefused(
                ExecutionContextUnattested
            )),
        );
    }
}
