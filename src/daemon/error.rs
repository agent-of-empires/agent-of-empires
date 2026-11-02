//! Fixed error discriminators shared by native clients and daemon handlers.

use crate::session::ManagedResumeCause;
use reqwest::{header::HeaderMap, StatusCode};

pub const ERROR_CODE_HEADER: &str = "aoe-error-code";
pub const MANAGED_RESUME_CAUSE_HEADER: &str = "aoe-managed-resume-cause";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiErrorCode {
    ReadOnly,
    ElevationRequired,
    AccessPolicyDenied,
    CityhallMode,
    LifecycleLocked,
    LifecycleSuperseded,
    PendingTargetGone,
    TlsRequired,
    RuntimeEpochMismatch,
    ResumeFailed,
    ManagedResumeRefused(ManagedResumeCause),
    StartFailed,
    PurgeFailed,
    NoRevive,
    CreationTrustChanged,
    CreationCancelled,
    CreationNotPending,
    CreateHookFailed,
    SmartRenameSkipped,
    SmartRenameFailed,
    SmartRenameTimeout,
    GroupNotEmpty,
    SessionNotRunning,
    SessionArchived,
    SessionTrashed,
}

impl ApiErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::ElevationRequired => "elevation_required",
            Self::AccessPolicyDenied => "access_policy_denied",
            Self::CityhallMode => "cityhall_mode",
            Self::LifecycleLocked => "lifecycle_locked",
            Self::LifecycleSuperseded => "lifecycle_superseded",
            Self::PendingTargetGone => "pending_target_gone",
            Self::StartFailed => "start_failed",
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
            Self::SmartRenameSkipped => "smart_rename_skipped",
            Self::SmartRenameFailed => "smart_rename_failed",
            Self::SmartRenameTimeout => "smart_rename_timeout",
            Self::GroupNotEmpty => "group_not_empty",
            Self::SessionNotRunning => "session_not_running",
            Self::SessionArchived => "session_archived",
            Self::SessionTrashed => "session_trashed",
        }
    }

    pub const fn status(self) -> StatusCode {
        match self {
            Self::ReadOnly
            | Self::ElevationRequired
            | Self::AccessPolicyDenied
            | Self::CityhallMode => StatusCode::FORBIDDEN,
            Self::LifecycleLocked
            | Self::LifecycleSuperseded
            | Self::RuntimeEpochMismatch
            | Self::ResumeFailed
            | Self::ManagedResumeRefused(_)
            | Self::PurgeFailed
            | Self::NoRevive
            | Self::CreationTrustChanged
            | Self::CreationCancelled
            | Self::CreationNotPending
            | Self::GroupNotEmpty
            | Self::SessionNotRunning
            | Self::SessionArchived
            | Self::SessionTrashed => StatusCode::CONFLICT,
            Self::PendingTargetGone => StatusCode::NOT_FOUND,
            Self::TlsRequired => StatusCode::UPGRADE_REQUIRED,
            Self::CreateHookFailed => StatusCode::BAD_REQUEST,
            Self::SmartRenameSkipped => StatusCode::CONFLICT,
            Self::SmartRenameFailed => StatusCode::BAD_GATEWAY,
            Self::SmartRenameTimeout => StatusCode::GATEWAY_TIMEOUT,
            Self::StartFailed => StatusCode::INTERNAL_SERVER_ERROR,
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
            b"lifecycle_locked" => Self::LifecycleLocked,
            b"lifecycle_superseded" => Self::LifecycleSuperseded,
            b"pending_target_gone" if resolving_pending_target => Self::PendingTargetGone,
            b"tls_required" => Self::TlsRequired,
            b"runtime_epoch_mismatch" => Self::RuntimeEpochMismatch,
            b"resume_failed" => Self::ResumeFailed,
            b"managed_resume_refused" => Self::ManagedResumeRefused(managed_resume_cause(headers)),
            b"no_revive" => Self::NoRevive,
            b"creation_trust_changed" => Self::CreationTrustChanged,
            b"creation_cancelled" => Self::CreationCancelled,
            b"start_failed" => Self::StartFailed,
            b"purge_failed" => Self::PurgeFailed,
            b"creation_not_pending" => Self::CreationNotPending,
            b"create_hook_failed" => Self::CreateHookFailed,
            b"smart_rename_skipped" => Self::SmartRenameSkipped,
            b"smart_rename_failed" => Self::SmartRenameFailed,
            b"smart_rename_timeout" => Self::SmartRenameTimeout,
            b"group_not_empty" => Self::GroupNotEmpty,
            b"session_not_running" => Self::SessionNotRunning,
            b"session_archived" => Self::SessionArchived,
            b"session_trashed" => Self::SessionTrashed,
            _ => return None,
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
            _ => ManagedResumeCause::ExecutionContextUnattested,
        },
        _ => ManagedResumeCause::ExecutionContextUnattested,
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
                StatusCode::CONFLICT,
                "group_not_empty",
                false,
                Some(ApiErrorCode::GroupNotEmpty),
            ),
            (StatusCode::FORBIDDEN, "group_not_empty", false, None),
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
