//! Session CRUD, ensure-* lifecycle endpoints, and per-file diff handlers.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::daemon::{
    CleanupDefaults, ContextResumeAvailability, ContextResumeIndeterminateReason,
    ContextResumeUnavailableReason, CreateSessionBody, DeleteSessionBody, ListSessionsQuery,
    PendingApproval, PlanSummary, PurgeOutcome, SessionResponse, TrashSessionBody, Tristate,
    UpdateArchiveBody, UpdateColorBody, UpdateDiffBaseBody, UpdateFavoriteBody, UpdateGroupBody,
    UpdateNotificationsBody, UpdatePinBody, UpdateSnoozeBody, UpdateUnreadBody,
    WorkspaceRepoSummary,
};

use crate::git::error::GitError;
use crate::session::config::SessionConfig;
use crate::session::{
    duplicate_session_error, is_duplicate_session, EnsureReadyError, EnsureReadyOutcome, Instance,
    LifecycleOperation, Status, Storage, TerminalContextResume,
};

use super::validate_no_shell_injection;
use super::AppState;
use super::{api_error, session_not_found, validate_display_label};

mod artifacts;
mod create;
mod delete;
mod diff;
mod ensure;
mod lifecycle;
mod list;
mod model;
mod rename;
mod search;
mod send;
mod update;

pub use artifacts::*;
pub use create::*;
pub use delete::*;
pub use diff::*;
pub use ensure::*;
pub use lifecycle::*;
pub use list::*;
use model::*;
pub use rename::*;
pub use search::*;
pub use send::*;
pub use update::*;

/// Shell-metacharacter gate for the free-form spawn fields a client can set
/// on a session, shared by the create and restart routes so the two cannot
/// drift.
///
/// `command_override` is deliberately absent: it is an `argv[0]` program path
/// rather than shell input, and the create route gates it on the agent
/// registry and the ACP identity check instead (#7), never on metacharacters.
pub(super) fn validate_shell_fields(checks: &[(&str, &str)]) -> Result<(), String> {
    checks
        .iter()
        .find_map(|(value, name)| validate_no_shell_injection(value, name).err())
        .map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests;
