//! Fixed error discriminators shared by native clients and daemon handlers.

use reqwest::{header::HeaderMap, StatusCode};

pub const ERROR_CODE_HEADER: &str = "aoe-error-code";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiErrorCode {
    ReadOnly,
    AccessPolicyDenied,
    CityhallMode,
    LifecycleLocked,
    PendingTargetGone,
    TlsRequired,
    RuntimeEpochMismatch,
    ResumeFailed,
    NoRevive,
    CreationTrustChanged,
    CreationCancelled,
    CreationNotPending,
    CreateHookFailed,
}

impl ApiErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::AccessPolicyDenied => "access_policy_denied",
            Self::CityhallMode => "cityhall_mode",
            Self::LifecycleLocked => "lifecycle_locked",
            Self::PendingTargetGone => "pending_target_gone",
            Self::TlsRequired => "tls_required",
            Self::RuntimeEpochMismatch => "runtime_epoch_mismatch",
            Self::ResumeFailed => "resume_failed",
            Self::NoRevive => "no_revive",
            Self::CreationTrustChanged => "creation_trust_changed",
            Self::CreationCancelled => "creation_cancelled",
            Self::CreationNotPending => "creation_not_pending",
            Self::CreateHookFailed => "create_hook_failed",
        }
    }

    pub const fn status(self) -> StatusCode {
        match self {
            Self::ReadOnly | Self::AccessPolicyDenied | Self::CityhallMode => StatusCode::FORBIDDEN,
            Self::LifecycleLocked
            | Self::RuntimeEpochMismatch
            | Self::ResumeFailed
            | Self::NoRevive
            | Self::CreationTrustChanged
            | Self::CreationCancelled
            | Self::CreationNotPending => StatusCode::CONFLICT,
            Self::PendingTargetGone => StatusCode::NOT_FOUND,
            Self::TlsRequired => StatusCode::UPGRADE_REQUIRED,
            Self::CreateHookFailed => StatusCode::BAD_REQUEST,
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
            b"access_policy_denied" => Self::AccessPolicyDenied,
            b"cityhall_mode" => Self::CityhallMode,
            b"lifecycle_locked" => Self::LifecycleLocked,
            b"pending_target_gone" if resolving_pending_target => Self::PendingTargetGone,
            b"tls_required" => Self::TlsRequired,
            b"runtime_epoch_mismatch" => Self::RuntimeEpochMismatch,
            b"resume_failed" => Self::ResumeFailed,
            b"no_revive" => Self::NoRevive,
            b"creation_trust_changed" => Self::CreationTrustChanged,
            b"creation_cancelled" => Self::CreationCancelled,
            b"creation_not_pending" => Self::CreationNotPending,
            b"create_hook_failed" => Self::CreateHookFailed,
            _ => return None,
        };
        (status == code.status()).then_some(code)
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
                StatusCode::BAD_REQUEST,
                "create_hook_failed",
                false,
                Some(ApiErrorCode::CreateHookFailed),
            ),
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
}
