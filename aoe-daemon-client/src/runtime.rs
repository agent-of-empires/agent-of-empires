//! Native handshake, snapshot and mutation types used over HTTP.

use serde::{Deserialize, Serialize};

use super::SessionResponse;

pub const RUNTIME_PROTOCOL_VERSION: u16 = 4;
pub const RUNTIME_EPOCH_HEADER: &str = "aoe-runtime-epoch";
pub const RUNTIME_REVISION_HEADER: &str = "aoe-runtime-revision";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCursor {
    pub epoch: String,
    pub revision: u64,
}

/// An acknowledged operation result; session rows come from runtime snapshots.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MutationReceipt<T> {
    pub cursor: RuntimeCursor,
    pub outcome: T,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum SessionMutation {
    Start(super::StartSessionBody),
    Restart(super::RestartSessionBody),
    Stop,
    StopAuxiliary(super::StopAuxiliaryBody),
    AbandonPurge(super::AbandonPurgeBody),
    Archive(super::UpdateArchiveBody),
    Access,
    Pin(super::UpdatePinBody),
    Favorite(super::UpdateFavoriteBody),
    Group(super::UpdateGroupBody),
    Notifications(super::UpdateNotificationsBody),
    Snooze(super::UpdateSnoozeBody),
    Unread(super::UpdateUnreadBody),
    DiffBase(super::UpdateDiffBaseBody),
}

impl SessionMutation {
    pub(crate) fn route(&self) -> &'static str {
        match self {
            Self::Start(_) => "start",
            Self::Restart(_) => "restart",
            Self::Stop => "stop",
            Self::StopAuxiliary(_) => "auxiliary/stop",
            Self::AbandonPurge(_) => "purge/abandon",
            Self::Archive(_) => "archive",
            Self::Access => "access",
            Self::Pin(_) => "pin",
            Self::Favorite(_) => "favorite",
            Self::Group(_) => "group",
            Self::Notifications(_) => "notifications",
            Self::Snooze(_) => "snooze",
            Self::Unread(_) => "unread",
            Self::DiffBase(_) => "diff-base",
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "scope", rename_all = "lowercase")]
pub enum ProjectTarget {
    Global,
    Profile { profile: String },
}

pub enum ProjectMutation {
    Create(super::CreateProjectBody),
    Update {
        target: ProjectTarget,
        name_or_path: String,
        patch: crate::ProjectPatch,
    },
    Remove {
        target: ProjectTarget,
        name_or_path: String,
        /// The client's absolute reading of a path-like argument, matched
        /// against entry paths while `name_or_path` stays a candidate name.
        anchored_path: Option<String>,
    },
}

pub enum ProfileMutation {
    Create(super::CreateProfileBody),
    Rename {
        name: String,
        body: super::RenameProfileBody,
    },
    Delete {
        name: String,
        query: super::DeleteProfileQuery,
    },
    SetDefault(super::DefaultProfileBody),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReloadFailureCode {
    ProfileEnumeration,
    ProfileData,
    Metadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum RuntimeHealth {
    Healthy,
    Degraded {
        code: ReloadFailureCode,
        profiles: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCapabilities {
    pub mutations: bool,
    pub native_interaction: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerLoadDiagnostic {
    pub path: String,
    pub message: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_diagnostics: Vec<OwnerLoadDiagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_instance_id: Option<String>,
    pub protocol_version: u16,
    /// The Daemon package's Cargo version, not the stamped binary version.
    /// `None` means unknown: the Daemon predates this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_version: Option<String>,
    /// Supports CLI selection intent and deferred terminal creation.
    #[serde(default)]
    pub cli_creation: bool,
    pub epoch: String,
    pub namespace: String,
    pub tmux_socket: Option<String>,
    pub local_owner: bool,
    pub profiles: Vec<String>,
    pub read_only: bool,
    pub cityhall_mode: bool,
    pub health: RuntimeHealth,
    pub interaction_capabilities: RuntimeCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectResponse {
    pub name: String,
    pub path: String,
    pub scope: crate::ProjectScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_base_branch: Option<String>,
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "crate::ProjectOverrides::is_empty")]
    pub overrides: crate::ProjectOverrides,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSnapshot {
    pub name: String,
    pub description: Option<String>,
    pub groups: Vec<crate::Group>,
    pub projects: Vec<ProjectResponse>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeContents {
    pub health: RuntimeHealth,
    pub capabilities: RuntimeCapabilities,
    pub default_profile: String,
    pub sessions: Vec<SessionResponse>,
    pub profiles: Vec<ProfileSnapshot>,
    pub workspace_ordering: Vec<String>,
    pub global_projects: Vec<ProjectResponse>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSnapshot {
    pub cursor: RuntimeCursor,
    #[serde(flatten)]
    pub contents: RuntimeContents,
}
