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

use crate::git::error::GitError;
use crate::session::config::SessionConfig;
use crate::session::{
    duplicate_session_error, is_duplicate_session, EnsureReadyError, EnsureReadyOutcome, Instance,
    LifecycleOperation, Status, Storage,
};

use super::validate_display_label;
use super::validate_no_shell_injection;
use super::AppState;

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
pub use model::*;
pub use rename::*;
pub use search::*;
pub use send::*;
pub use update::*;

#[cfg(test)]
mod tests;
