//! Read-only runtime read endpoint: `GET /api/runtime/ws`, protocol version 2.
//!
//! One connection carries exactly two application frames — a Hello handshake and
//! a Snapshot — and the server then sends `Close(1000)`. The handler only reads
//! session, profile, group and project state; it never mutates it and never
//! consults a client-local filesystem.
//!
//! Authentication for the HTTP route is the router's existing credential gate, so
//! a caller without a valid credential never reaches the upgrade. The same two
//! frames are also served over the daemon's own UNIX socket, where the peer is
//! the same uid the daemon runs as, so that transport declares the local owner.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use tokio::net::UnixStream;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio_tungstenite::tungstenite;

use super::AppState;
use crate::session::Instance;

/// Wire protocol version. The client refuses anything else.
const PROTOCOL_VERSION: u16 = 2;

/// The trusted namespace this build publishes for itself, and the name the
/// UDS marker files carry.
pub(crate) const NAMESPACE: &str = if cfg!(debug_assertions) {
    "debug:agent-of-empires-dev"
} else {
    "release:agent-of-empires"
};

pub async fn runtime_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    // The daemon-wide middleware also accepts browser cookies and query tokens.
    // The runtime read contract accepts exactly one Authorization: Bearer header.
    if !has_bearer_header(&headers) {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    // CityHall lockdown must not be bypassable by opening this route directly.
    if let Some(response) = super::api::cityhall_block(&state) {
        return response;
    }
    ws.on_upgrade(move |socket| serve_runtime_read(socket, state))
        .into_response()
}

fn has_bearer_header(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    let bytes = token.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 4096
        && bytes
            .iter()
            .all(|byte| (0x21..=0x7e).contains(byte) && *byte != b'"' && *byte != b'\\')
}
async fn serve_runtime_read(socket: WebSocket, state: Arc<AppState>) {
    run_read(ReadSocket::Web(socket), state, Owner::remote(), true).await;
}

/// The same two frames over the daemon's own UNIX socket. The peer is admitted
/// by [`super::runtime_uds`], which checks the connecting uid before the
/// upgrade, so the declared owner is this process's own uid.
pub(crate) async fn serve_runtime_read_uds(
    socket: tokio_tungstenite::WebSocketStream<UnixStream>,
    state: Arc<AppState>,
) {
    if state.cityhall_mode {
        tracing::warn!(
            target: "runtime.uds",
            "refusing the local runtime read while CityHall lockdown is on"
        );
        return;
    }
    let uid = unsafe { libc::geteuid() };
    run_read(ReadSocket::Unix(socket), state, Owner::local(uid), false).await;
}

/// One producer for both transports: same single-flight sampler, same two
/// frames, same close. Only the socket and the declared owner differ.
enum ReadSocket {
    Web(WebSocket),
    Unix(tokio_tungstenite::WebSocketStream<UnixStream>),
}

impl ReadSocket {
    async fn send_text(&mut self, text: &str) -> Result<(), ()> {
        match self {
            ReadSocket::Web(socket) => socket
                .send(Message::Text(text.to_string().into()))
                .await
                .map_err(|_| ()),
            ReadSocket::Unix(socket) => socket
                .send(tungstenite::Message::Text(text.into()))
                .await
                .map_err(|_| ()),
        }
    }

    async fn wait_for_peer_close(&mut self) {
        match self {
            ReadSocket::Web(socket) => {
                while let Some(Ok(message)) = socket.next().await {
                    if matches!(message, Message::Close(_)) {
                        break;
                    }
                }
            }
            ReadSocket::Unix(socket) => {
                while let Some(Ok(message)) = socket.next().await {
                    if matches!(message, tungstenite::Message::Close(_)) {
                        break;
                    }
                }
            }
        }
    }
    async fn close(&mut self) {
        match self {
            ReadSocket::Web(socket) => {
                let _ = socket.send(Message::Close(None)).await;
            }
            ReadSocket::Unix(socket) => {
                let _ = socket.send(tungstenite::Message::Close(None)).await;
                let _ = socket.close(None).await;
            }
        }
    }
}

async fn run_read(
    mut socket: ReadSocket,
    state: Arc<AppState>,
    owner: Owner,
    close_after_frames: bool,
) {
    let runtime = &RUNTIME;
    // Single-flight: a second connection waits for the in-flight sample instead of
    // publishing a second revision, so one sample is one revision and one cursor step.
    let flight = runtime.flight.lock().await;

    let instances: Vec<Instance> = state.instances.read().await.clone();
    let active_profile = state.profile.clone();
    let sampled = tokio::task::spawn_blocking(move || {
        build_snapshot(runtime, &active_profile, &instances, owner)
    })
    .await;

    // The sample is the only work the flight serializes. A reader that stalls
    // mid-write must not keep the next one from sampling.
    drop(flight);

    let snapshot = match sampled {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::error!(target: "runtime.ws", %error, "runtime sample task failed");
            socket.close().await;
            return;
        }
    };
    let frames = [
        serde_json::to_string(&HelloFrame {
            kind: "hello",
            data: &snapshot.hello,
        }),
        serde_json::to_string(&SnapshotFrame {
            kind: "snapshot",
            data: &snapshot.data,
        }),
    ];
    for frame in frames {
        match frame {
            Ok(encoded) => {
                if socket.send_text(&encoded).await.is_err() {
                    return;
                }
            }
            Err(error) => {
                tracing::error!(target: "runtime.ws", %error, "runtime frame encode failed");
                socket.close().await;
                return;
            }
        }
    }
    if close_after_frames {
        socket.close().await;
    } else {
        socket.wait_for_peer_close().await;
    }
}

// ---------------------------------------------------------------------------
// Runtime identity and the status-freshness sampler
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct RuntimeIdentity {
    pub(crate) runtime_epoch: String,
    pub(crate) prebind_instance_id: String,
    pub(crate) runtime_instance_id: String,
}

/// Publication state of the freshness sampler. `successes` counts published
/// samples: the public observed revision is that count and the snapshot cursor is
/// that count plus one. `latched` is the overflow latch — once set no counter
/// moves again and the public projection stays unavailable.
#[derive(Default)]
struct Sampler {
    successes: u64,
    latched: bool,
}

struct RuntimeState {
    identity: RuntimeIdentity,
    sampler: Mutex<Sampler>,
    flight: tokio::sync::Mutex<()>,
}

impl RuntimeState {
    fn new() -> Self {
        let new_uuid = || uuid::Uuid::new_v4().to_string();
        Self {
            identity: RuntimeIdentity {
                runtime_epoch: new_uuid(),
                prebind_instance_id: new_uuid(),
                runtime_instance_id: new_uuid(),
            },
            sampler: Mutex::new(Sampler::default()),
            flight: tokio::sync::Mutex::new(()),
        }
    }
}

/// One runtime per daemon process: the epoch and both instance identities are
/// minted once and reused by every Hello this process emits.
static RUNTIME: LazyLock<RuntimeState> = LazyLock::new(RuntimeState::new);

/// The identities every Hello this process emits carries. The UDS publisher
/// writes the same three values into its marker files, so a client can prove
/// the daemon it reached is the one that published the socket.
pub(crate) fn identity() -> &'static RuntimeIdentity {
    &RUNTIME.identity
}

fn publish_freshness(runtime: &RuntimeState) -> (StatusFreshness, u64) {
    let mut sampler = runtime
        .sampler
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if sampler.latched {
        return (unavailable(), 0);
    }
    let Some(next) = sampler.successes.checked_add(1) else {
        sampler.latched = true;
        return (unavailable(), 0);
    };
    let Some(cursor) = next.checked_add(1) else {
        sampler.latched = true;
        return (unavailable(), 0);
    };
    sampler.successes = next;
    (
        StatusFreshness::Observed {
            revision: next,
            observed_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        },
        cursor,
    )
}

fn unavailable() -> StatusFreshness {
    StatusFreshness::Unavailable {
        revision: None,
        observed_at: None,
    }
}

// ---------------------------------------------------------------------------
// Snapshot assembly (blocking: every source is a filesystem read)
// ---------------------------------------------------------------------------

struct Sampled {
    hello: HelloData,
    data: SnapshotData,
}

/// Everything one profile contributes that is not derived from its sessions.
struct ProfileDisk {
    projects: Result<Vec<ProjectRead>, ()>,
    cleanup: CleanupDefaults,
}

fn build_snapshot(
    runtime: &RuntimeState,
    active_profile: &str,
    instances: &[Instance],
    owner: Owner,
) -> Sampled {
    // `local_owner` mirrors the declared owner so the two can never disagree.
    let local_owner = owner.is_local();
    let identity = &runtime.identity;
    let (freshness, cursor_revision) = publish_freshness(runtime);

    // Rows are projected before the disk reads so the session projection itself
    // never runs behind a filesystem call.
    let mut sessions: Vec<SessionRead> = instances.iter().map(SessionRead::from_instance).collect();

    // A profile that still holds sessions must appear in the snapshot even when
    // its directory is gone, otherwise every one of its rows would be unprojectable.
    let enumeration = crate::session::list_profiles_readonly();
    let enumeration_healthy = enumeration.is_ok();
    let mut names: BTreeSet<String> = enumeration.unwrap_or_default().into_iter().collect();
    names.extend(sessions.iter().map(|row| row.profile.clone()));

    let disk: BTreeMap<String, ProfileDisk> = names
        .iter()
        .map(|name| {
            (
                name.clone(),
                ProfileDisk {
                    projects: crate::session::projects::load_profile(name)
                        .map(|projects| {
                            projects
                                .into_iter()
                                .map(|project| ProjectRead {
                                    name: project.name,
                                    path: project.path,
                                    scope: ProjectScope::Profile,
                                    default_base_branch: project.default_base_branch,
                                })
                                .collect::<Vec<_>>()
                        })
                        .map_err(|_| ()),
                    cleanup: cleanup_defaults(),
                },
            )
        })
        .collect();

    let global_projects = match crate::session::projects::load_global() {
        Ok(projects) => Some(
            projects
                .into_iter()
                .map(|project| ProjectRead {
                    name: project.name,
                    path: project.path,
                    scope: ProjectScope::Global,
                    default_base_branch: project.default_base_branch,
                })
                .collect::<Vec<_>>(),
        ),
        Err(error) => {
            tracing::warn!(target: "runtime.ws", %error, "global project registry unreadable");
            None
        }
    };
    let global_metadata_healthy = global_projects.is_some();
    let global_projects = global_projects.unwrap_or_default();

    sessions.retain(|row| names.contains(&row.profile));
    sessions.sort_by(|left, right| left.id.cmp(&right.id));
    reconcile_legacy_rows(&mut sessions);
    for row in &mut sessions {
        row.cleanup_defaults = disk[&row.profile].cleanup;
    }

    let mut profile_reads = Vec::with_capacity(names.len());
    let mut profile_health = BTreeMap::new();
    for name in &names {
        let entry = &disk[name];
        let scoped: Vec<&SessionRead> =
            sessions.iter().filter(|row| &row.profile == name).collect();
        let mut projects = entry.projects.clone().unwrap_or_default();
        // Referential integrity: every session's project path is a member of its
        // own profile's project list.
        add_session_projects(&mut projects, &scoped, &global_projects);
        projects.sort_by(|left, right| (&left.name, &left.path).cmp(&(&right.name, &right.path)));
        projects.dedup_by(|left, right| left.name == right.name && left.path == right.path);
        let health = ProfileHealth {
            profile_enumeration: if entry.projects.is_ok() {
                ComponentHealth::Healthy
            } else {
                ComponentHealth::Degraded {
                    code: HealthCode::ProfileEnumeration,
                }
            },
            metadata: ComponentHealth::Healthy,
            profile_data: ComponentHealth::Healthy,
        };
        profile_health.insert(name.clone(), health);
        profile_reads.push(ProfileRead {
            name: name.clone(),
            groups: group_reads(&scoped),
            projects,
            health,
        });
    }

    let snapshot_health = SnapshotHealth {
        global_enumeration: component(enumeration_healthy, HealthCode::Enumeration),
        global_metadata: component(global_metadata_healthy, HealthCode::Metadata),
        profiles: profile_health,
    };
    let aggregate = aggregate_health(&snapshot_health);
    let default_profile = (!active_profile.is_empty() && names.contains(active_profile))
        .then(|| active_profile.to_string())
        .or_else(|| names.iter().min().cloned());

    Sampled {
        hello: HelloData {
            protocol_version: PROTOCOL_VERSION,
            runtime_epoch: identity.runtime_epoch.clone(),
            prebind_instance_id: identity.prebind_instance_id.clone(),
            runtime_instance_id: identity.runtime_instance_id.clone(),
            namespace: NAMESPACE.to_string(),
            // The transport decides: a TCP peer is remote, the daemon's own
            // socket peer is the local owner.
            owner,
            local_owner,
            health: aggregate,
            profiles: profile_reads.clone(),
            status_freshness: freshness.clone(),
        },
        data: SnapshotData {
            namespace: NAMESPACE.to_string(),
            cursor: Cursor {
                epoch: identity.runtime_epoch.clone(),
                revision: cursor_revision,
            },
            health: snapshot_health,
            default_profile,
            profiles: profile_reads,
            sessions,
            global_projects,
            status_freshness: freshness,
        },
    }
}

/// Reconcile the stored rows against the rules a read projects under, field by
/// field, so one legacy row cannot make the client refuse a whole snapshot.
///
/// A parent is kept only when it names a row of the same profile and the
/// relation it forms is acyclic; otherwise the child nests at the top level,
/// which is what the local `aoe list` shows for a parent it cannot resolve. A
/// project path is spelled the way the store itself compares paths, trailing
/// separators aside. Every other field is projected as stored, so the client's
/// validation stays fail-closed rather than learning to tolerate more.
fn reconcile_legacy_rows(sessions: &mut [SessionRead]) {
    for row in sessions.iter_mut() {
        row.project_path = comparable_project_path(&row.project_path);
    }
    let index: HashMap<&str, usize> = sessions
        .iter()
        .enumerate()
        .map(|(position, row)| (row.id.as_str(), position))
        .collect();
    let mut parents: HashMap<usize, &str> = sessions
        .iter()
        .enumerate()
        .filter_map(|(position, row)| {
            row.parent_session_id
                .as_deref()
                .map(|parent| (position, parent))
        })
        .collect();
    let severed = cycle_entries(&index, &mut parents);
    let resolved: Vec<bool> = sessions
        .iter()
        .enumerate()
        .map(|(position, row)| {
            !severed.contains(&position)
                && parents.get(&position).is_some_and(|parent| {
                    index
                        .get(parent)
                        .is_some_and(|parent| sessions[*parent].profile == row.profile)
                })
        })
        .collect();
    for (row, resolved) in sessions.iter_mut().zip(resolved) {
        if !resolved {
            row.parent_session_id = None;
        }
    }
}

/// The trailing-separator-free spelling the store compares project paths by
/// (`Storage` treats `/repo` and `/repo/` as one project).
fn comparable_project_path(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The row at which each parent cycle closes. The relation gives every row at
/// most one parent, so a walk from any row that enters a cycle meets that
/// cycle's entry again, and severing that one edge breaks the cycle while every
/// tail below it stays nested. Walks start in row order, and rows are already
/// sorted by id, so the entry chosen for a cycle is the same on every run.
fn cycle_entries(
    index: &HashMap<&str, usize>,
    parents: &mut HashMap<usize, &str>,
) -> HashSet<usize> {
    let mut entries = HashSet::new();
    for start in parents.keys().copied().collect::<BTreeSet<_>>() {
        let mut walked = HashSet::new();
        let mut cursor = Some(start);
        while let Some(row) = cursor {
            if !walked.insert(row) {
                // Severed as it is found, so the next walk down the same cycle
                // sees the break rather than reporting the cycle again.
                entries.insert(row);
                parents.remove(&row);
                break;
            }
            cursor = parents
                .get(&row)
                .and_then(|parent| index.get(parent).copied());
        }
    }
    entries
}

fn component(healthy: bool, code: HealthCode) -> ComponentHealth {
    if healthy {
        ComponentHealth::Healthy
    } else {
        ComponentHealth::Degraded { code }
    }
}

/// The Hello aggregate is a diagnostic roll-up of the same components, worst
/// first: `enumeration` > `profile_enumeration` > `metadata` > `profile_data`.
fn aggregate_health(health: &SnapshotHealth) -> AggregateHealth {
    let worst = [health.global_enumeration, health.global_metadata]
        .into_iter()
        .chain(health.profiles.values().flat_map(|profile| {
            [
                profile.profile_enumeration,
                profile.metadata,
                profile.profile_data,
            ]
        }))
        .find_map(|component| match component {
            ComponentHealth::Healthy => None,
            ComponentHealth::Degraded { code } => Some(code),
        });
    match worst {
        None => AggregateHealth::Healthy,
        Some(code) => AggregateHealth::Degraded { code },
    }
}

/// Cleanup defaults. The effective per-profile config is deliberately not
/// resolved here: `resolve_config` installs a process-global status-rule
/// registry, which a read-only snapshot must not do. No read command projects
/// these four flags, so the compiled defaults are what the wire carries.
fn cleanup_defaults() -> CleanupDefaults {
    let config = crate::session::config::Config::default();
    CleanupDefaults {
        delete_worktree: config.worktree.auto_cleanup,
        delete_branch: config.worktree.should_delete_branch_on_cleanup(),
        delete_sandbox: config.sandbox.auto_cleanup,
        delete_to_trash: config.session.delete_to_trash,
    }
}

/// Every group path used by a profile's sessions, plus the ancestor paths the
/// `children` relation implies, sorted bytewise.
fn group_reads(sessions: &[&SessionRead]) -> Vec<GroupRead> {
    let mut paths: BTreeSet<String> = BTreeSet::new();
    for session in sessions {
        if session.group_path.is_empty() {
            continue;
        }
        let components: Vec<&str> = session.group_path.split('/').collect();
        for index in 1..=components.len() {
            paths.insert(components[..index].join("/"));
        }
    }
    paths
        .iter()
        .map(|path| {
            let prefix = format!("{path}/");
            let children: BTreeSet<String> = paths
                .iter()
                .filter_map(|other| other.strip_prefix(&prefix))
                .filter_map(|tail| tail.split('/').next())
                .map(str::to_string)
                .collect();
            GroupRead {
                name: path.rsplit('/').next().unwrap_or_default().to_string(),
                path: path.clone(),
                children: children.into_iter().collect(),
            }
        })
        .collect()
}

/// Add a profile-scoped project for every session project path the registries do
/// not already cover, so no row points at an unknown project.
fn add_session_projects(
    projects: &mut Vec<ProjectRead>,
    sessions: &[&SessionRead],
    global: &[ProjectRead],
) {
    let known: BTreeSet<String> = projects
        .iter()
        .map(|project| project.path.clone())
        .chain(global.iter().map(|project| project.path.clone()))
        .collect();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for session in sessions {
        let path = session.project_path.clone();
        if !canonical_absolute_path(&path) || known.contains(&path) || !seen.insert(path.clone()) {
            continue;
        }
        projects.push(ProjectRead {
            name: path.rsplit('/').next().unwrap_or_default().to_string(),
            path,
            scope: ProjectScope::Profile,
            default_base_branch: None,
        });
    }
}

/// The one canonical absolute-path grammar the wire contract requires.
fn canonical_absolute_path(value: &str) -> bool {
    value == "/"
        || (value.starts_with('/')
            && value.split('/').skip(1).all(|component| {
                !component.is_empty()
                    && component != "."
                    && component != ".."
                    && valid_text(component)
            }))
}

/// Rejected everywhere a value can reach a rendered template: C0/C1 controls,
/// DEL, the Unicode line/paragraph separators and the bidi controls.
fn valid_text(value: &str) -> bool {
    !value.chars().any(|scalar| {
        matches!(scalar as u32,
            0x00..=0x1f | 0x7f..=0x9f | 0x2028 | 0x2029 | 0x202a..=0x202e | 0x2066..=0x2069)
    })
}

// ---------------------------------------------------------------------------
// Wire DTOs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct HelloFrame<'a> {
    kind: &'static str,
    data: &'a HelloData,
}

#[derive(Serialize)]
struct SnapshotFrame<'a> {
    kind: &'static str,
    data: &'a SnapshotData,
}

#[derive(Serialize)]
struct HelloData {
    protocol_version: u16,
    runtime_epoch: String,
    prebind_instance_id: String,
    runtime_instance_id: String,
    namespace: String,
    owner: Owner,
    local_owner: bool,
    health: AggregateHealth,
    profiles: Vec<ProfileRead>,
    status_freshness: StatusFreshness,
}

#[derive(Serialize)]
struct SnapshotData {
    namespace: String,
    cursor: Cursor,
    health: SnapshotHealth,
    default_profile: Option<String>,
    profiles: Vec<ProfileRead>,
    sessions: Vec<SessionRead>,
    global_projects: Vec<ProjectRead>,
    status_freshness: StatusFreshness,
}

#[derive(Serialize)]
struct Cursor {
    epoch: String,
    revision: u64,
}

#[derive(Serialize, Clone, Copy)]
struct Owner {
    kind: &'static str,
    uid: Option<u32>,
}

impl Owner {
    fn remote() -> Self {
        Self {
            kind: "remote",
            uid: None,
        }
    }

    fn local(uid: u32) -> Self {
        Self {
            kind: "local_owner",
            uid: Some(uid),
        }
    }

    fn is_local(&self) -> bool {
        self.uid.is_some()
    }
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AggregateHealth {
    Healthy,
    Degraded { code: HealthCode },
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HealthCode {
    Enumeration,
    ProfileEnumeration,
    Metadata,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ComponentHealth {
    Healthy,
    Degraded { code: HealthCode },
}

#[derive(Serialize, Clone, Copy)]
struct ProfileHealth {
    profile_enumeration: ComponentHealth,
    metadata: ComponentHealth,
    profile_data: ComponentHealth,
}

#[derive(Serialize)]
struct SnapshotHealth {
    global_enumeration: ComponentHealth,
    global_metadata: ComponentHealth,
    profiles: BTreeMap<String, ProfileHealth>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StatusFreshness {
    /// Published together with a committed sample, so this is the first thing a
    /// client ever sees; the unobserved state has no server-side producer.
    Observed { revision: u64, observed_at: String },
    Unavailable {
        revision: Option<u64>,
        observed_at: Option<String>,
    },
}

#[derive(Serialize, Clone)]
struct ProfileRead {
    name: String,
    groups: Vec<GroupRead>,
    projects: Vec<ProjectRead>,
    health: ProfileHealth,
}

#[derive(Serialize, Clone)]
struct GroupRead {
    name: String,
    path: String,
    children: Vec<String>,
}

#[derive(Serialize, Clone)]
struct ProjectRead {
    name: String,
    path: String,
    scope: ProjectScope,
    default_base_branch: Option<String>,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProjectScope {
    Global,
    Profile,
}

#[derive(Serialize, Clone, Copy)]
struct CleanupDefaults {
    delete_worktree: bool,
    delete_branch: bool,
    delete_sandbox: bool,
    delete_to_trash: bool,
}

#[derive(Serialize, Clone)]
struct WorktreeRead {
    branch: String,
    main_repo_path: String,
    managed_by_aoe: bool,
    base_branch: Option<String>,
}

#[derive(Serialize, Clone)]
struct WorkspaceRepo {
    name: String,
    source_path: String,
    branch: String,
}

#[derive(Serialize, Clone)]
struct SessionRead {
    id: String,
    title: String,
    project_path: String,
    group_path: String,
    tool: String,
    command: String,
    profile: String,
    status: &'static str,
    state: &'static str,
    created_at: String,
    last_accessed_at: Option<String>,
    idle_entered_at: Option<String>,
    last_error: Option<String>,
    archived_at: Option<String>,
    trashed_at: Option<String>,
    active_snoozed_until: Option<String>,
    pinned_at: Option<String>,
    agent_session_id: Option<String>,
    parent_session_id: Option<String>,
    has_terminal: bool,
    has_worktree_info: bool,
    has_managed_worktree: bool,
    has_cleanable_worktree: bool,
    worktree: Option<WorktreeRead>,
    workspace_repos: Vec<WorkspaceRepo>,
    /// Replaced by the per-profile config during assembly.
    cleanup_defaults: CleanupDefaults,
}

impl SessionRead {
    fn from_instance(inst: &Instance) -> Self {
        let mut workspace_repos: Vec<WorkspaceRepo> = inst
            .workspace_info
            .as_ref()
            .map(|info| {
                info.repos
                    .iter()
                    .map(|repo| WorkspaceRepo {
                        name: repo.name.clone(),
                        source_path: repo.source_path.clone(),
                        branch: repo.branch.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        workspace_repos.sort_by(|left, right| {
            (&left.name, &left.source_path).cmp(&(&right.name, &right.source_path))
        });

        Self {
            id: inst.id.clone(),
            title: inst.title.clone(),
            project_path: inst.project_path.clone(),
            group_path: inst.group_path.clone(),
            tool: inst.tool.clone(),
            command: inst.command.clone(),
            profile: inst.source_profile.clone(),
            status: inst.status.wire_str(),
            state: wire_state(inst),
            created_at: format_timestamp(inst.created_at),
            last_accessed_at: timestamp(inst.last_accessed_at),
            idle_entered_at: timestamp(inst.idle_entered_at),
            last_error: inst.last_error.clone(),
            archived_at: timestamp(inst.archived_at),
            trashed_at: timestamp(inst.trashed_at),
            // Only while the snooze is active; a persisted past date is not one.
            active_snoozed_until: if inst.is_snoozed() {
                timestamp(inst.snoozed_until)
            } else {
                None
            },
            pinned_at: timestamp(inst.pinned_at),
            agent_session_id: inst.agent_session_id.clone(),
            parent_session_id: inst.parent_session_id.clone(),
            has_terminal: inst.terminal_info.is_some(),
            has_worktree_info: inst.worktree_info.is_some(),
            has_managed_worktree: inst
                .worktree_info
                .as_ref()
                .is_some_and(|worktree| worktree.managed_by_aoe),
            has_cleanable_worktree: inst.has_managed_worktree_or_workspace(),
            worktree: inst.worktree_info.as_ref().map(|worktree| WorktreeRead {
                branch: worktree.branch.clone(),
                main_repo_path: worktree.main_repo_path.clone(),
                managed_by_aoe: worktree.managed_by_aoe,
                base_branch: worktree.base_branch.clone(),
            }),
            workspace_repos,
            cleanup_defaults: CleanupDefaults {
                delete_worktree: true,
                delete_branch: false,
                delete_sandbox: true,
                delete_to_trash: true,
            },
        }
    }
}

fn wire_state(inst: &Instance) -> &'static str {
    if inst.trashed_at.is_some() {
        "trashed"
    } else if inst.archived_at.is_some() {
        "archived"
    } else {
        "live"
    }
}

/// One canonical timestamp spelling: RFC 3339 UTC seconds with a `Z` zone and no
/// fractional part.
fn format_timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn timestamp(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(format_timestamp)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::cli::runtime_read::dto::{
        parse_hello, parse_snapshot, validate_cross_message, validate_snapshot,
    };

    /// Points the app dir at an empty temporary XDG base for the duration of one
    /// test, so a snapshot is assembled from fixtures rather than the developer's
    /// real profiles and project registries.
    struct TempHome {
        _dir: tempfile::TempDir,
        previous: Option<OsString>,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("temp home");
            let previous = std::env::var_os("XDG_CONFIG_HOME");
            std::env::set_var("XDG_CONFIG_HOME", dir.path());
            Self {
                _dir: dir,
                previous,
            }
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    fn instance(id: &str, profile: &str) -> Instance {
        let mut inst = Instance::new(id, "/repo");
        inst.source_profile = profile.to_string();
        inst.title = format!("Session {id}");
        inst.tool = "claude".into();
        inst.group_path = "alpha/beta".into();
        inst.status = crate::session::Status::Waiting;
        inst
    }

    fn hello_frame(sampled: &Sampled) -> Vec<u8> {
        serde_json::to_vec(&HelloFrame {
            kind: "hello",
            data: &sampled.hello,
        })
        .expect("hello encodes")
    }

    fn snapshot_frame(sampled: &Sampled) -> Vec<u8> {
        serde_json::to_vec(&SnapshotFrame {
            kind: "snapshot",
            data: &sampled.data,
        })
        .expect("snapshot encodes")
    }

    fn observed_revision(sampled: &Sampled) -> u64 {
        match &sampled.hello.status_freshness {
            StatusFreshness::Observed { revision, .. } => *revision,
            other => panic!("expected an observed freshness, got {other:?}"),
        }
    }

    /// The two frames the server emits, decoded by the client's real decoders and
    /// validators: any field-name, member-order or invariant drift fails here.
    #[test]
    #[serial_test::serial]
    fn emitted_frames_satisfy_the_client_wire_contract() {
        let _home = TempHome::new();
        let instances = vec![instance("a", "main"), instance("b", "main")];
        let sampled = build_snapshot(&RuntimeState::new(), "main", &instances, Owner::remote());

        let hello = parse_hello(&hello_frame(&sampled)).expect("client accepts the Hello");
        let snapshot =
            parse_snapshot(&snapshot_frame(&sampled)).expect("client accepts the Snapshot");
        validate_snapshot(&snapshot).expect("Snapshot is well formed");
        validate_cross_message(&hello, &snapshot, None).expect("Hello and Snapshot agree");

        assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
        assert_eq!(hello.namespace, snapshot.namespace);
        assert_eq!(hello.runtime_epoch, snapshot.cursor.epoch);
    }

    #[test]
    #[serial_test::serial]
    fn hello_declares_a_remote_owner_and_observed_freshness() {
        let _home = TempHome::new();
        let sampled = build_snapshot(
            &RuntimeState::new(),
            "main",
            &[instance("a", "main")],
            Owner::remote(),
        );
        let value: serde_json::Value = serde_json::to_value(&sampled.hello).expect("hello encodes");

        assert_eq!(value["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(value["local_owner"], serde_json::json!(false));
        assert_eq!(
            value["owner"],
            serde_json::json!({"kind": "remote", "uid": null})
        );
        assert_eq!(value["namespace"], NAMESPACE);
        let freshness = &value["status_freshness"];
        assert_eq!(freshness["kind"], "observed");
        assert!(freshness["revision"].as_u64().expect("revision") >= 1);
        let observed_at = freshness["observed_at"].as_str().expect("timestamp");
        assert!(observed_at.ends_with('Z') && !observed_at.contains('.'));
    }

    /// The cursor is the independent counter: it is the observed revision plus
    /// one, and each committed sample advances both by exactly one.
    #[test]
    #[serial_test::serial]
    fn each_sample_advances_revision_and_cursor_together() {
        let _home = TempHome::new();
        let runtime = RuntimeState::new();
        let first = build_snapshot(&runtime, "main", &[], Owner::remote());
        let second = build_snapshot(&runtime, "main", &[], Owner::remote());

        assert_eq!(observed_revision(&first), 1);
        assert_eq!(observed_revision(&second), 2);
        assert_eq!(first.data.cursor.revision, 2);
        assert_eq!(second.data.cursor.revision, 3);
        assert_eq!(first.data.cursor.epoch, second.data.cursor.epoch);
    }

    /// Once the counter would wrap, the projection latches to unavailable and no
    /// counter moves again.
    #[test]
    fn a_latched_sampler_stops_publishing() {
        let runtime = RuntimeState::new();
        runtime.sampler.lock().unwrap().successes = u64::MAX;
        let (freshness, cursor) = publish_freshness(&runtime);

        assert!(matches!(freshness, StatusFreshness::Unavailable { .. }));
        assert_eq!(cursor, 0);
        assert!(runtime.sampler.lock().unwrap().latched);
        let (again, cursor) = publish_freshness(&runtime);
        assert!(matches!(again, StatusFreshness::Unavailable { .. }));
        assert_eq!(cursor, 0);
    }

    #[test]
    fn group_children_are_the_sorted_immediate_child_names() {
        let rows: Vec<SessionRead> = vec![
            {
                let mut row = SessionRead::from_instance(&instance("a", "main"));
                row.group_path = "alpha/beta".into();
                row
            },
            {
                let mut row = SessionRead::from_instance(&instance("b", "main"));
                row.group_path = "alpha/gamma".into();
                row
            },
        ];
        let scoped: Vec<&SessionRead> = rows.iter().collect();
        let groups = group_reads(&scoped);

        let paths: Vec<&str> = groups.iter().map(|group| group.path.as_str()).collect();
        assert_eq!(paths, vec!["alpha", "alpha/beta", "alpha/gamma"]);
        assert_eq!(groups[0].name, "alpha");
        assert_eq!(groups[0].children, vec!["beta", "gamma"]);
        assert!(groups[1].children.is_empty());
    }

    /// Referential integrity: a session whose project path is in no registry
    /// still gets a same-profile project, or the client rejects the snapshot.
    #[test]
    fn every_session_project_path_is_reachable_within_its_profile() {
        let rows: Vec<SessionRead> = vec![SessionRead::from_instance(&instance("a", "main"))];
        let scoped: Vec<&SessionRead> = rows.iter().collect();
        let mut projects = Vec::new();
        add_session_projects(&mut projects, &scoped, &[]);

        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].path, "/repo");
        assert_eq!(projects[0].name, "repo");
        assert_eq!(projects[0].scope, ProjectScope::Profile);
    }

    /// `Instance::new` mints its own id, so a fixture row sets the one the test
    /// reads back.
    fn named(id: &str, profile: &str) -> Instance {
        let mut inst = instance(id, profile);
        inst.id = id.to_string();
        inst
    }

    fn row(id: &str, parent: Option<&str>, path: &str) -> SessionRead {
        let mut row = SessionRead::from_instance(&named(id, "main"));
        row.parent_session_id = parent.map(str::to_string);
        row.project_path = path.to_string();
        row
    }

    /// One stored row the read cannot project must not cost every other row its
    /// read: an orphan parent nests at the top level and a legacy trailing
    /// separator is spelled the way the store compares paths, and the client
    /// then accepts the snapshot unchanged.
    #[test]
    #[serial_test::serial]
    fn a_legacy_row_is_reconciled_and_the_client_still_accepts_the_snapshot() {
        let _home = TempHome::new();
        let mut orphan = named("orphan", "main");
        orphan.parent_session_id = Some("deleted".into());
        let mut trailing = named("trailing", "main");
        trailing.project_path = "/repo/".into();
        let instances = vec![named("a", "main"), orphan, trailing];

        let sampled = build_snapshot(&RuntimeState::new(), "main", &instances, Owner::remote());

        let snapshot =
            parse_snapshot(&snapshot_frame(&sampled)).expect("client accepts the Snapshot");
        validate_snapshot(&snapshot).expect("the reconciled snapshot is projectable");
        let reconciled: Vec<(&str, Option<&str>, &str)> = snapshot
            .sessions
            .iter()
            .map(|row| {
                (
                    row.id.as_str(),
                    row.parent_session_id.as_deref(),
                    row.project_path.as_str(),
                )
            })
            .collect();
        assert_eq!(
            reconciled,
            vec![
                ("a", None, "/repo"),
                ("orphan", None, "/repo"),
                ("trailing", None, "/repo"),
            ]
        );
    }

    /// A parent that names a row of another profile, and a cycle, are the two
    /// relations the client refuses. Both are severed at the row that closes
    /// them, and both choices are the same on every run.
    #[test]
    fn cross_profile_parents_and_cycles_are_severed_at_the_closing_row() {
        let mut cross = row("a", Some("b"), "/repo");
        cross.profile = "other".into();
        let mut cycle = vec![row("x", Some("y"), "/repo"), row("y", Some("x"), "/repo")];
        cycle.push(row("z", Some("x"), "/repo"));
        let mut rows = vec![cross];
        rows.append(&mut cycle);

        reconcile_legacy_rows(&mut rows);

        let parents: Vec<(&str, Option<&str>)> = rows
            .iter()
            .map(|row| (row.id.as_str(), row.parent_session_id.as_deref()))
            .collect();
        assert_eq!(
            parents,
            vec![("a", None), ("x", None), ("y", Some("x")), ("z", Some("x"))]
        );
    }

    #[test]
    fn canonical_paths_reject_traversal_and_control_bytes() {
        assert!(canonical_absolute_path("/repo/one"));
        assert!(!canonical_absolute_path("repo"));
        assert!(!canonical_absolute_path("/repo/../etc"));
        assert!(!canonical_absolute_path("/repo//one"));
        assert!(!canonical_absolute_path("/repo/\u{202e}"));
    }
    #[test]
    fn runtime_ws_requires_one_bearer_header() {
        let mut headers = HeaderMap::new();
        assert!(!has_bearer_header(&headers));
        headers.append(header::AUTHORIZATION, "Bearer token".parse().unwrap());
        assert!(has_bearer_header(&headers));
        headers.append(header::AUTHORIZATION, "Bearer second".parse().unwrap());
        assert!(!has_bearer_header(&headers));
    }
}
