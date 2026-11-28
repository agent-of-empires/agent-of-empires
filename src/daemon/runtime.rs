//! Root-only runtime stream frames and UI mutation routing.

use serde::{Deserialize, Serialize};

use super::{OwnerLoadDiagnostic, ProfileMutation, RuntimeInfo, RuntimeSnapshot};

pub(crate) enum NamespaceMutation {
    CollapseGroup(super::CollapseGroupBody),
    MoveGroup(super::MoveGroupBody),
    DeleteGroup(super::DeleteGroupBody),
    Reorder(super::ReorderBody),
    Profile(ProfileMutation),
}

#[derive(Debug)]
pub(crate) enum NamespaceOutcome {
    Committed,
    DeletedGroup(super::DeleteGroupOutcome),
    Reordered(super::ReorderOutcome),
}

/// Live progress for one in-flight session creation.
///
/// Advisory state for the surface that asked for the creation, never canonical
/// session state: it stays off the snapshot, because hook stdout is the
/// documented secret channel for session environment values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreationProgress {
    pub session_id: String,
    /// Only this key or the known daemon id correlates progress to its caller.
    /// The caller's idempotency key, when it supplied one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_key: Option<String>,
    pub title: String,
    pub profile: String,
    pub phase: CreationPhase,
    /// Hook command currently running, when the phase runs commands.
    pub command: Option<String>,
    /// Bounded tail of the current phase output, oldest first.
    pub output: Vec<String>,
    /// A cancellation request is pending; the phase in flight still finishes, but an image pull stops.
    pub cancelled: bool,
}

/// Ordered phases of one daemon-owned creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CreationPhase {
    Reserving,
    Provisioning,
    CreateHooks,
    LaunchHooks,
}

/// Advisory progress of a daemon-owned sandbox preparation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProgress {
    pub operation_id: String,
    pub session_id: String,
    pub profile: String,
    pub activity: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum RuntimeFrame<T = RuntimeSnapshot> {
    Hello(RuntimeInfo),
    Snapshot(T),
    /// Creation progress, sent only to streams the daemon authorized as the
    /// local owner. See [`CreationProgress`].
    Creation(Vec<CreationProgress>),
    Sandbox {
        epoch: String,
        progress: Vec<SandboxProgress>,
    },
    OwnerDiagnostics {
        epoch: String,
        diagnostics: Vec<OwnerLoadDiagnostic>,
    },
}
