//! Versioned native handshake and full-state synchronization frames.

use serde::{Deserialize, Serialize};

use super::SessionResponse;

pub const RUNTIME_PROTOCOL_VERSION: u16 = 1;
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
    StopAuxiliary(crate::session::AuxiliaryTarget),
    Restore,
    AbandonPurge(super::AbandonPurgeBody),
    Archive(super::UpdateArchiveBody),
    Pin(super::UpdatePinBody),
    Favorite(super::UpdateFavoriteBody),
    Color(super::UpdateColorBody),
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
            Self::Restore => "restore",
            Self::AbandonPurge(_) => "purge/abandon",
            Self::Archive(_) => "archive",
            Self::Pin(_) => "pin",
            Self::Favorite(_) => "favorite",
            Self::Color(_) => "color",
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
        patch: crate::session::projects::ProjectPatch,
    },
    Remove {
        target: ProjectTarget,
        name_or_path: String,
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
pub struct RuntimeInfo {
    pub protocol_version: u16,
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
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(export, export_to = "../web/src/lib/apiWire.ts"))]
pub struct ProjectResponse {
    pub name: String,
    pub path: String,
    pub scope: crate::session::ProjectScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub default_base_branch: Option<String>,
    pub pinned: bool,
    #[serde(
        default,
        skip_serializing_if = "crate::session::ProjectOverrides::is_empty"
    )]
    #[cfg_attr(test, ts(as = "Option<crate::session::ProjectOverrides>", optional))]
    pub overrides: crate::session::ProjectOverrides,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSnapshot {
    pub name: String,
    pub description: Option<String>,
    pub groups: Vec<crate::session::Group>,
    pub projects: Vec<ProjectResponse>,
}

/// Live progress for one in-flight session creation.
///
/// Advisory state for the surface that asked for the creation, never canonical
/// session state: it stays off the snapshot, because hook stdout is the
/// documented secret channel for session environment values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreationProgress {
    pub session_id: String,
    /// Title and profile let the requesting surface attribute progress to the
    /// creation it submitted before the row appears in a snapshot.
    pub title: String,
    pub profile: String,
    pub phase: CreationPhase,
    /// Hook command currently running, when the phase runs commands.
    pub command: Option<String>,
    /// Bounded tail of the current phase output, oldest first.
    pub output: Vec<String>,
    /// A cancellation request is pending; the phase in flight still finishes.
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum RuntimeFrame<T = RuntimeSnapshot> {
    Hello(RuntimeInfo),
    Snapshot(T),
    /// Creation progress, sent only to streams the daemon authorized as the
    /// local owner. See [`CreationProgress`].
    Creation(Vec<CreationProgress>),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capabilities() -> RuntimeCapabilities {
        RuntimeCapabilities {
            mutations: true,
            native_interaction: true,
        }
    }

    /// The stream's handshake compares `protocol_version` by exact equality,
    /// so an accidental rename of the envelope or of a frame kind is not a
    /// degraded client, it is a client that cannot connect at all. Unlike the
    /// REST bodies, nothing here is generated into a second language, so this
    /// is the only thing standing between a rename and a silent break.
    #[test]
    fn the_runtime_envelope_is_what_a_connecting_client_matches_on() {
        let hello = RuntimeFrame::<RuntimeSnapshot>::Hello(RuntimeInfo {
            protocol_version: RUNTIME_PROTOCOL_VERSION,
            epoch: "e1".into(),
            namespace: "ns".into(),
            tmux_socket: None,
            local_owner: true,
            profiles: vec!["default".into()],
            read_only: false,
            cityhall_mode: false,
            health: RuntimeHealth::Healthy,
            interaction_capabilities: capabilities(),
        });
        let json = serde_json::to_value(&hello).unwrap();
        assert_eq!(json["kind"], "hello");
        assert_eq!(json["data"]["protocol_version"], 1);
        assert_eq!(json["data"]["epoch"], "e1");

        let creation = RuntimeFrame::<RuntimeSnapshot>::Creation(Vec::new());
        assert_eq!(serde_json::to_value(&creation).unwrap()["kind"], "creation");
    }

    /// The snapshot nests its cursor and flattens its contents, so a client
    /// reads `data.cursor.revision` but `data.sessions`. The asymmetry is easy
    /// to invert while editing either struct, and inverting it moves every
    /// field a subscriber reads.
    #[test]
    fn a_snapshot_nests_its_cursor_and_flattens_its_contents() {
        let snapshot = RuntimeSnapshot {
            cursor: RuntimeCursor {
                epoch: "e1".into(),
                revision: 7,
            },
            contents: RuntimeContents {
                health: RuntimeHealth::Healthy,
                capabilities: capabilities(),
                default_profile: "default".into(),
                sessions: Vec::new(),
                profiles: Vec::new(),
                workspace_ordering: Vec::new(),
                global_projects: Vec::new(),
            },
        };
        let json = serde_json::to_value(RuntimeFrame::Snapshot(snapshot)).unwrap();
        assert_eq!(json["kind"], "snapshot");
        assert_eq!(json["data"]["cursor"]["epoch"], "e1");
        assert_eq!(json["data"]["cursor"]["revision"], 7);
        assert_eq!(json["data"]["default_profile"], "default");
        assert!(
            json["data"]["sessions"].is_array(),
            "contents are flattened"
        );
        assert!(json["data"]["contents"].is_null());
    }
}
