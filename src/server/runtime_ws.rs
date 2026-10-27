//! Read-only runtime read endpoint: `GET /api/runtime/read/ws`.
//!
//! One connection carries a Hello and a Snapshot, then the server closes. Reads
//! never mutate session, profile, group or project state.
//!
//! Authentication is the router's existing credential gate, so a caller without
//! a valid credential never reaches the upgrade. The same route serves the TCP
//! listener and the daemon's Unix socket.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
#[cfg(test)]
use tokio::net::UnixStream;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
#[cfg(test)]
use futures_util::SinkExt;
use futures_util::StreamExt;

use serde::Serialize;
#[cfg(test)]
use tokio_tungstenite::tungstenite;

use super::AppState;
use crate::cli::runtime_read::dto::{
    wire_timestamp, GroupRead, ProjectRead, ProjectScope, SessionRead,
};
#[cfg(any(test, debug_assertions))]
use crate::session::Instance;
use crate::session::{GroupTree, Storage};

/// Wire version with physical profile aliases and listed membership.
pub(crate) const PROTOCOL_VERSION: u16 = 4;
/// Whole-connection budget shared by both transports and the read client.
pub(crate) const CONNECTION_BUDGET: Duration = Duration::from_secs(15);
/// Shared incoming and outgoing frame/message ceiling for both transports.
pub(crate) const MESSAGE_LIMIT: usize = crate::cli::runtime_read::APPLICATION_LIMIT;
/// The trusted namespace this build publishes for itself.
pub(crate) const NAMESPACE: &str = if cfg!(debug_assertions) {
    "debug:agent-of-empires-dev"
} else {
    "release:agent-of-empires"
};

/// Bound snapshot production and frame writes, not idle close handshakes.
pub(crate) const RUNTIME_READ_CONCURRENCY: usize = 8;

pub async fn runtime_ws(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    // CityHall lockdown must not be bypassable by opening this route directly.
    if let Some(response) = super::api::cityhall_block(&state) {
        return response;
    }
    // Bound the connection, including a peer that stops reading or closing.
    ws.max_message_size(MESSAGE_LIMIT)
        .max_frame_size(crate::cli::runtime_read::APPLICATION_LIMIT)
        .on_upgrade(move |socket| async move {
            if tokio::time::timeout(CONNECTION_BUDGET, serve_runtime_read(socket, state))
                .await
                .is_err()
            {
                tracing::warn!(target: "runtime.ws", "runtime read exceeded its budget");
            }
        })
        .into_response()
}

async fn serve_runtime_read(socket: WebSocket, state: Arc<AppState>) {
    run_read(ReadSocket::Web(socket), state, Owner::remote(), true).await;
}

/// Frame transports for `run_read`; the Unix one exists only to drive it in tests.
enum ReadSocket {
    Web(WebSocket),
    #[cfg(test)]
    Unix(tokio_tungstenite::WebSocketStream<UnixStream>),
}

impl ReadSocket {
    async fn send_text(&mut self, text: String) -> Result<(), ()> {
        match self {
            ReadSocket::Web(socket) => socket
                .send(Message::Text(text.into()))
                .await
                .map_err(|_| ()),
            #[cfg(test)]
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
            #[cfg(test)]
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
            #[cfg(test)]
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
    let Ok(admitted) = state.runtime_read_semaphore.clone().acquire_owned().await else {
        return;
    };
    let runtime = &RUNTIME;
    let (sessions, cache, observed_at) = {
        let instances = state.instances.read().await;
        let cache = state
            .runtime_read_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rows = Vec::with_capacity(instances.len() + cache.alias_only_instances.len());
        rows.extend(
            instances
                .iter()
                .chain(cache.alias_only_instances.iter())
                .map(SessionRead::from_instance),
        );
        let metadata = super::reload::RuntimeReadCache {
            inventory: cache.inventory.clone(),
            alias_only_instances: Vec::new(),
            health: cache.health.clone(),
        };
        (rows, metadata, runtime.pinned_now.unwrap_or_else(Utc::now))
    };
    // A cancelled blocking sample retains admission until its worker exits.
    let sampled = tokio::task::spawn_blocking(move || {
        let _flight = runtime
            .flight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sample = build_snapshot(runtime, sessions, owner, observed_at, &cache);
        (sample, admitted)
    })
    .await;

    let (snapshot, admitted) = match sampled {
        Ok(sample) => sample,
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
    drop(snapshot);
    for frame in frames {
        match frame {
            Ok(encoded) => {
                // Refuse whole frames rather than truncate a listing.
                if encoded.len() > MESSAGE_LIMIT {
                    tracing::warn!(
                        target: "runtime.ws",
                        bytes = encoded.len(),
                        "runtime frame is over the client's message ceiling; closing without sending"
                    );
                    socket.close().await;
                    return;
                }
                if socket.send_text(encoded).await.is_err() {
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
    drop(admitted);
    if close_after_frames {
        socket.close().await;
    } else {
        socket.wait_for_peer_close().await;
    }
}

// Recorded wire fixtures.

/// Wire frames and identities for the fixture recorder.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub struct RecordedExchange {
    /// The Hello frame, as the producer's own serialiser wrote it.
    pub hello: Vec<u8>,
    /// The Snapshot frame, likewise.
    pub snapshot: Vec<u8>,
}

/// Recorded local transport declares the peer UID; remote transport does not.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub enum RecordedOwner {
    Local { uid: u32 },
    Remote,
}

/// Pin minted identities and observation time for deterministic recorded bytes.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub struct RecordingPins {
    pub runtime_epoch: String,
    pub prebind_instance_id: String,
    pub runtime_instance_id: String,
    /// The instant the observed freshness is stamped with.
    pub observed_at: DateTime<Utc>,
}

#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub fn record_exchange(
    instances: &[Instance],
    owner: RecordedOwner,
    pins: &RecordingPins,
) -> RecordedExchange {
    let owner = match owner {
        RecordedOwner::Local { uid } => Owner::local(uid),
        RecordedOwner::Remote => Owner::remote(),
    };
    let runtime = RuntimeState {
        identity: RuntimeIdentity {
            runtime_epoch: pins.runtime_epoch.clone(),
            prebind_instance_id: pins.prebind_instance_id.clone(),
            runtime_instance_id: pins.runtime_instance_id.clone(),
        },
        sampler: Mutex::new(Sampler::default()),
        flight: Mutex::new(()),
        pinned_now: Some(pins.observed_at),
    };
    let sampled = build_snapshot(
        &runtime,
        instances.iter().map(SessionRead::from_instance).collect(),
        owner,
        pins.observed_at,
        &crate::server::reload::RuntimeReadCache::accepted_inventory(),
    );
    let encode = |frame: Result<String, serde_json::Error>| {
        frame.expect("a recorded frame encodes").into_bytes()
    };
    RecordedExchange {
        hello: encode(serde_json::to_string(&HelloFrame {
            kind: "hello",
            data: &sampled.hello,
        })),
        snapshot: encode(serde_json::to_string(&SnapshotFrame {
            kind: "snapshot",
            data: &sampled.data,
        })),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RuntimeIdentity {
    pub(crate) runtime_epoch: String,
    pub(crate) prebind_instance_id: String,
    pub(crate) runtime_instance_id: String,
}

/// Published revisions count successful samples; overflow latches unavailable freshness.
#[derive(Default)]
struct Sampler {
    successes: u64,
    latched: bool,
}

struct RuntimeState {
    identity: RuntimeIdentity,
    sampler: Mutex<Sampler>,
    /// The blocking worker owns sample exclusion, even if its caller is cancelled.
    flight: Mutex<()>,
    /// Recordings may pin time; production uses the wall clock.
    pinned_now: Option<DateTime<Utc>>,
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
            flight: Mutex::new(()),
            pinned_now: None,
        }
    }
}

/// One runtime per daemon process: the epoch and both instance identities are
/// minted once and reused by every Hello this process emits.
static RUNTIME: LazyLock<RuntimeState> = LazyLock::new(RuntimeState::new);

/// Handshake identities shared with the local publisher's marker pair.
#[cfg(target_os = "linux")]
pub(crate) fn identity() -> &'static RuntimeIdentity {
    &RUNTIME.identity
}

/// Keep the latched cursor nonzero so unavailable freshness is not a malformed snapshot.
const LATCHED_CURSOR: u64 = u64::MAX;

fn publish_freshness(runtime: &RuntimeState, observed_at: DateTime<Utc>) -> (StatusFreshness, u64) {
    let mut sampler = runtime
        .sampler
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if sampler.latched {
        return (unavailable(), LATCHED_CURSOR);
    }
    let Some(next) = sampler.successes.checked_add(1) else {
        sampler.latched = true;
        return (unavailable(), LATCHED_CURSOR);
    };
    let Some(cursor) = next.checked_add(1) else {
        sampler.latched = true;
        return (unavailable(), LATCHED_CURSOR);
    };
    sampler.successes = next;
    (
        StatusFreshness::Observed {
            revision: next,
            observed_at: wire_timestamp(observed_at),
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

struct Sampled {
    hello: HelloData,
    data: SnapshotData,
}

/// Everything one profile contributes that is not derived from its sessions.
struct ProfileDisk {
    projects: Result<Vec<ProjectRead>, ()>,
    groups: Result<Vec<crate::session::Group>, ()>,
}

fn build_snapshot(
    runtime: &RuntimeState,
    mut sessions: Vec<SessionRead>,
    owner: Owner,
    observed_at: DateTime<Utc>,
    cache: &super::reload::RuntimeReadCache,
) -> Sampled {
    // `local_owner` mirrors the declared owner so the two can never disagree.
    let local_owner = owner.is_local();
    let identity = &runtime.identity;
    let (freshness, cursor_revision) = publish_freshness(runtime, observed_at);

    let enumeration = super::reload::selectable_profiles();
    let enumeration_healthy = enumeration.is_ok();
    let mut current = enumeration.unwrap_or_default();
    // A retired cached owner keeps its key. Omitting a conflicting store can expose another conflict.
    loop {
        let mut changed = false;
        let mut position = 0;
        while position < current.len() {
            if current[position].listed {
                position += 1;
                continue;
            }
            let identity = current[position].identity;
            let reserved = |name: &str| {
                cache
                    .inventory
                    .iter()
                    .find(|profile| profile.name == name && profile.identity != identity)
                    .is_some_and(|owner| {
                        sessions.iter().any(|row| row.profile == owner.name)
                            && !current
                                .iter()
                                .any(|profile| profile.identity == owner.identity)
                    })
            };
            if reserved(&current[position].name) {
                let replacement = current[position]
                    .aliases
                    .iter()
                    .position(|name| !reserved(name));
                changed = true;
                if let Some(replacement) = replacement {
                    let profile = &mut current[position];
                    std::mem::swap(&mut profile.name, &mut profile.aliases[replacement]);
                } else {
                    current.remove(position);
                    continue;
                }
            }
            position += 1;
        }
        if !changed {
            break;
        }
    }
    let mut accepted_now = HashSet::new();
    if !cache.health.enumeration_failed {
        for profile in current.iter().filter(|profile| profile.listed) {
            if cache.inventory.iter().any(|prior| {
                prior.identity == profile.identity
                    || prior.name == profile.name
                    || prior.aliases.contains(&profile.name)
            }) {
                continue;
            }
            let Ok(mut rows) = Storage::open_unwatched(&profile.name)
                .and_then(|storage| storage.load_instances_readonly())
            else {
                continue;
            };
            let cached_ids: HashSet<&str> = sessions
                .iter()
                .filter(|row| row.profile == profile.name)
                .map(|row| row.id.as_str())
                .collect();
            rows.retain(|row| !cached_ids.contains(row.id.as_str()));
            drop(cached_ids);
            sessions.extend(rows.iter().map(SessionRead::from_instance));
            accepted_now.insert(profile.identity);
        }
    }
    for row in &mut sessions {
        if let Some(accepted) = cache
            .inventory
            .iter()
            .find(|profile| profile.name == row.profile)
        {
            let representative = current
                .iter()
                .find(|profile| {
                    profile.name == accepted.name && profile.identity == accepted.identity
                })
                .or_else(|| {
                    current
                        .iter()
                        .find(|profile| profile.identity == accepted.identity)
                });
            if let Some(profile) = representative.filter(|profile| profile.name != row.profile) {
                row.profile.clone_from(&profile.name);
            }
        }
    }
    let mut inventory: BTreeMap<String, Option<super::reload::SelectableProfile>> = current
        .into_iter()
        .map(|profile| (profile.name.clone(), Some(profile)))
        .collect();
    for row in &sessions {
        inventory.entry(row.profile.clone()).or_insert(None);
    }
    for name in &cache.health.unreadable_profiles {
        let represented = cache
            .inventory
            .iter()
            .find(|profile| &profile.name == name)
            .is_some_and(|accepted| {
                inventory
                    .values()
                    .flatten()
                    .any(|profile| profile.identity == accepted.identity)
            });
        let selected_alias = inventory
            .values()
            .flatten()
            .any(|profile| profile.aliases.contains(name));
        if !represented && !selected_alias {
            inventory.entry(name.clone()).or_insert(None);
        }
    }
    // Retained row keys own ambiguous selections until an accepted reload.
    for profile in inventory.values_mut().flatten() {
        profile
            .aliases
            .retain(|alias| !sessions.iter().any(|row| &row.profile == alias));
    }
    let disk: BTreeMap<String, ProfileDisk> = inventory
        .iter()
        .map(|(name, profile)| {
            let accepted = profile.as_ref().is_some_and(|profile| {
                profile.listed
                    || cache
                        .inventory
                        .iter()
                        .any(|prior| prior.identity == profile.identity)
            });
            let external = profile.as_ref().is_some_and(|profile| !profile.listed);
            let projects = if accepted && external {
                let owned = name.clone();
                alias_sample(name, "projects", move || load_profile_projects(&owned))
            } else if accepted {
                load_profile_projects(name)
            } else {
                Err(())
            };
            let groups = if accepted && external {
                let owned = name.clone();
                alias_sample(name, "groups", move || load_profile_groups(&owned))
            } else if accepted {
                load_profile_groups(name)
            } else {
                Err(())
            };
            (name.clone(), ProfileDisk { projects, groups })
        })
        .collect();

    let global_projects = match crate::session::projects::load_global() {
        Ok(projects) => Some(
            projects
                .into_iter()
                .map(|project| ProjectRead {
                    merge_key: crate::session::projects::canonical_key(&project.path),
                    name: project.name,
                    path: project.path,
                    scope: ProjectScope::Global,
                    default_base_branch: project.default_base_branch,
                    registered: true,
                })
                .collect::<Vec<_>>(),
        ),
        Err(error) => {
            tracing::warn!(target: "runtime.ws", %error, "global project registry unreadable");
            None
        }
    };
    let mut global_metadata_healthy = global_projects.is_some();
    let mut global_projects = global_projects.unwrap_or_default();
    drop_unusable_projects(&mut global_projects);
    if crate::cli::runtime_read::dto::validate_projects(&global_projects, ProjectScope::Global)
        .is_err()
    {
        tracing::warn!(target: "runtime.ws", "stored global projects violate the runtime read contract");
        global_projects.clear();
        global_metadata_healthy = false;
    }

    let listed_names: HashSet<&str> = inventory
        .iter()
        .filter(|(name, profile)| {
            profile
                .as_ref()
                .map(|profile| profile.listed)
                .or_else(|| {
                    cache
                        .inventory
                        .iter()
                        .find(|profile| &profile.name == *name)
                        .map(|profile| profile.listed)
                })
                .unwrap_or(true)
        })
        .map(|(name, _)| name.as_str())
        .collect();
    reconcile_legacy_rows(&mut sessions);
    let rejected: HashSet<String> =
        crate::cli::runtime_read::dto::invalid_session_profiles(&sessions)
            .into_iter()
            .map(str::to_owned)
            .collect();
    for profile in &rejected {
        tracing::warn!(target: "runtime.ws", ?profile, "stored sessions violate the runtime read contract");
    }
    sessions.retain(|row| !rejected.contains(&row.profile));

    let mut profile_reads = Vec::with_capacity(inventory.len());
    let mut profile_health = BTreeMap::new();
    for (name, entry) in disk {
        let projects_healthy = entry.projects.is_ok();
        let mut groups_healthy = entry.groups.is_ok();
        let scoped: Vec<&SessionRead> = sessions.iter().filter(|row| row.profile == name).collect();
        let mut projects = entry.projects.unwrap_or_default();
        drop_unusable_projects(&mut projects);
        // Session project paths resolve through a profile or global registry.
        add_session_projects(&mut projects, &scoped, &global_projects);
        // Registry order, not a canonical sort: that is the order a local
        // `aoe project list` prints, and the wire carries the presentation.
        let mut identities: HashSet<(String, String)> = HashSet::new();
        projects.retain(|project| identities.insert((project.name.clone(), project.path.clone())));
        let stored_groups = entry.groups.unwrap_or_default();
        let tree = GroupTree::new_with_group_paths(
            scoped.iter().map(|row| row.group_path.as_str()),
            &stored_groups,
        );
        let mut groups = group_reads(&tree);
        if crate::cli::runtime_read::dto::validate_groups(&groups).is_err() {
            tracing::warn!(target: "runtime.ws", ?name, "stored groups violate the runtime read contract");
            groups = group_reads(&GroupTree::new_with_group_paths(
                scoped.iter().map(|row| row.group_path.as_str()),
                &[],
            ));
            groups_healthy = false;
        }
        let health = ProfileHealth {
            profile_enumeration: component(
                groups_healthy && crate::session::valid_profile_name(&name),
                HealthCode::ProfileEnumeration,
            ),
            metadata: component(projects_healthy, HealthCode::Metadata),
            profile_data: component(
                !rejected.contains(&name)
                    && !cache.health.enumeration_failed
                    && inventory[&name].as_ref().is_some_and(|profile| {
                        accepted_now.contains(&profile.identity)
                            || cache.inventory.iter().any(|accepted| {
                                accepted.identity == profile.identity
                                    && !cache.health.unreadable_profiles.contains(&accepted.name)
                            })
                    }),
                HealthCode::ProfileEnumeration,
            ),
        };
        profile_health.insert(name.clone(), health);
        profile_reads.push(ProfileRead {
            listed: listed_names.contains(name.as_str()),
            aliases: inventory[&name]
                .as_ref()
                .map(|profile| profile.aliases.clone())
                .unwrap_or_default(),
            name,
            groups,
            projects,
            health,
        });
    }

    let snapshot_health = SnapshotHealth {
        global_enumeration: component(enumeration_healthy, HealthCode::Enumeration),
        global_metadata: component(global_metadata_healthy, HealthCode::Metadata),
        profiles: profile_health,
    };
    let aggregate = aggregate_health(&snapshot_health, &profile_reads);
    let config = if crate::session::app_dir_exists() {
        crate::session::config::Config::load_or_warn()
    } else {
        crate::session::config::Config::default()
    };
    let resolved = if !config.default_profile.is_empty() {
        Some(config.default_profile)
    } else {
        inventory
            .iter()
            .find(|(_, profile)| profile.as_ref().is_some_and(|profile| profile.listed))
            .map(|(name, _)| name.clone())
    };
    let default_profile = resolved
        .as_ref()
        .filter(|name| {
            profile_reads
                .iter()
                .any(|profile| &profile.name == *name || profile.aliases.contains(*name))
        })
        .cloned();

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
            profiles: profile_reads
                .iter()
                .map(|profile| ProfileHello {
                    name: profile.name.clone(),
                    listed: profile.listed,
                    aliases: profile.aliases.clone(),
                    health: profile.health,
                })
                .collect(),
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
            resolved_default_profile: resolved,
            profiles: profile_reads,
            sessions,
            global_projects,
            status_freshness: freshness,
        },
    }
}

/// Omit stored paths that cannot represent an absolute native directory.
fn load_profile_projects(name: &str) -> Result<Vec<ProjectRead>, ()> {
    crate::session::projects::load_profile(name)
        .map(|projects| {
            projects
                .into_iter()
                .map(|project| ProjectRead {
                    merge_key: crate::session::projects::canonical_key(&project.path),
                    name: project.name,
                    path: project.path,
                    scope: ProjectScope::Profile,
                    default_base_branch: project.default_base_branch,
                    registered: true,
                })
                .collect()
        })
        .map_err(|_| ())
}

fn load_profile_groups(name: &str) -> Result<Vec<crate::session::Group>, ()> {
    Storage::open_inventory_reference(name, crate::file_watch::FileWatchService::noop())
        .and_then(|storage| storage.load_groups_readonly())
        .map_err(|_| ())
}

/// A stale alias read is unavailable, never mixed into a fresh snapshot.
fn alias_sample<T>(
    name: &str,
    kind: &'static str,
    work: impl FnOnce() -> Result<T, ()> + Send + 'static,
) -> Result<T, ()>
where
    T: Clone + Send + Sync + 'static,
{
    let sampled = super::alias_io::sample(name, kind, work);
    match sampled.value {
        Some(value) if sampled.fresh => (*value).clone(),
        _ => Err(()),
    }
}

fn drop_unusable_projects(projects: &mut Vec<ProjectRead>) {
    projects
        .retain(|project| crate::cli::runtime_read::dto::valid_stored_project_path(&project.path));
}

/// Preserve scoped orphans and sever cycles.
fn reconcile_legacy_rows(sessions: &mut [SessionRead]) {
    let index: HashMap<(&str, &str), usize> = sessions
        .iter()
        .enumerate()
        .map(|(position, row)| ((row.profile.as_str(), row.id.as_str()), position))
        .collect();
    let mut parents: HashMap<usize, usize> = sessions
        .iter()
        .enumerate()
        .filter_map(|(position, row)| {
            row.parent_session_id
                .as_deref()
                .and_then(|parent| index.get(&(row.profile.as_str(), parent)).copied())
                .map(|parent| (position, parent))
        })
        .collect();
    let severed = cycle_entries(&mut parents);
    for (position, row) in sessions.iter_mut().enumerate() {
        if severed.contains(&position) {
            row.parent_session_id = None;
        }
    }
}

/// Sever the first cycle entry reached from sorted starting rows.
fn cycle_entries(parents: &mut HashMap<usize, usize>) -> HashSet<usize> {
    use std::collections::hash_map::Entry;

    let mut entries = HashSet::new();
    let mut walked = HashMap::with_capacity(parents.len());
    let mut starts: Vec<_> = parents.keys().copied().collect();
    starts.sort_unstable();
    for start in starts {
        let mut cursor = Some(start);
        while let Some(row) = cursor {
            match walked.entry(row) {
                Entry::Occupied(visit) => {
                    if *visit.get() == start {
                        entries.insert(row);
                        parents.remove(&row);
                    }
                    break;
                }
                Entry::Vacant(visit) => {
                    visit.insert(start);
                }
            }
            cursor = parents.get(&row).copied();
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

/// Aggregate reads depend only on listed stores.
fn aggregate_health(health: &SnapshotHealth, profiles: &[ProfileRead]) -> AggregateHealth {
    let worst = [health.global_enumeration, health.global_metadata]
        .into_iter()
        .chain(
            profiles
                .iter()
                .filter(|profile| profile.listed)
                .flat_map(|profile| {
                    [
                        profile.health.profile_enumeration,
                        profile.health.metadata,
                        profile.health.profile_data,
                    ]
                }),
        )
        .find_map(|component| match component {
            ComponentHealth::Healthy => None,
            ComponentHealth::Degraded { code } => Some(code),
        });
    match worst {
        None => AggregateHealth::Healthy,
        Some(code) => AggregateHealth::Degraded { code },
    }
}

/// The profile's group inventory, in the order the tree holds them: the
/// registry's insertion order, then the groups its sessions imply. A group
/// with no session is in the registry and therefore still appears.
fn group_reads(tree: &GroupTree) -> Vec<GroupRead> {
    tree.get_all_groups()
        .iter()
        .map(|group| GroupRead {
            name: group.name.clone(),
            path: group.path.clone(),
            children: group.children.iter().map(|c| c.name.clone()).collect(),
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
        if !crate::cli::runtime_read::dto::valid_session_project_path(&path)
            || known.contains(&path)
            || !seen.insert(path.clone())
        {
            continue;
        }
        projects.push(ProjectRead {
            // A synthesized row is never merged, so it needs no canonical_key
            // call; the path is as good a key as any for a row nothing keys on.
            merge_key: path.clone(),
            name: path.rsplit('/').next().unwrap_or_default().to_string(),
            path,
            scope: ProjectScope::Profile,
            default_base_branch: None,
            registered: false,
        });
    }
}

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
    profiles: Vec<ProfileHello>,
    status_freshness: StatusFreshness,
}

#[derive(Serialize)]
struct SnapshotData {
    namespace: String,
    cursor: Cursor,
    health: SnapshotHealth,
    default_profile: Option<String>,
    /// Resolved default name, including a stale configuration, unless resolution would create it.
    resolved_default_profile: Option<String>,
    profiles: Vec<ProfileRead>,
    sessions: Vec<SessionRead>,
    global_projects: Vec<ProjectRead>,
    status_freshness: StatusFreshness,
}

/// Profile name and cached-data readiness; full inventory follows in Snapshot.
#[derive(Serialize, Clone)]
struct ProfileHello {
    name: String,
    listed: bool,
    aliases: Vec<String>,
    health: ProfileHealth,
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

    #[cfg(any(target_os = "linux", test, debug_assertions))]
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
    listed: bool,
    aliases: Vec<String>,
    groups: Vec<GroupRead>,
    projects: Vec<ProjectRead>,
    health: ProfileHealth,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::runtime_read::dto::{
        parse_hello, parse_snapshot, validate_cross_message, validate_hello, validate_snapshot,
    };
    use crate::session::{Group, Status, WorkspaceInfo, WorkspaceRepo as StoredRepo, WorktreeInfo};

    #[test]
    #[serial_test::serial]
    fn emitted_nested_rows_satisfy_the_published_wire_schemas() {
        use crate::cli::runtime_read::pack::{validate_data, HELLO_SCHEMA, SNAPSHOT_SCHEMA};
        let _home = TempHome::new();
        let manager = tempfile::tempdir().unwrap();
        std::fs::create_dir(manager.path().join("systemd")).unwrap();
        std::fs::write(manager.path().join("systemd/private"), "").unwrap();
        let _env = crate::session::test_support::EnvGuard::unset(&["DBUS_SESSION_BUS_ADDRESS"])
            .and_set("XDG_RUNTIME_DIR", manager.path());
        crate::session::config::update_config(|config| config.systemd.enabled = true).unwrap();
        let mut session = named("a", "main");
        session.group_path = "team/sub".into();
        session.tool = "custom\tagent".into();
        let mut repository = repo("beta\t", "/srv/beta\t");
        repository.branch = "feature\u{202e}review".into();
        session.worktree_info = Some(WorktreeInfo {
            branch: "feature\u{202e}review".into(),
            main_repo_path: "/repo/".into(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: Some("base\u{2066}review".into()),
        });
        session.workspace_info = Some(WorkspaceInfo {
            branch: "feature".into(),
            workspace_dir: "/repo/.aoe/workspace".into(),
            created_at: Utc::now(),
            cleanup_on_delete: true,
            repos: vec![repository],
        });
        let mut empty_path = named("empty-path", "main");
        empty_path.project_path.clear();
        let rows: Vec<_> = [session, empty_path]
            .iter()
            .map(SessionRead::from_instance)
            .collect();
        for owner in [Owner::remote(), Owner::local(0)] {
            let sampled = build_snapshot(
                &RuntimeState::new(),
                rows.clone(),
                owner,
                Utc::now(),
                &crate::server::reload::RuntimeReadCache::accepted_inventory(),
            );
            for (schema, bytes) in [
                (HELLO_SCHEMA, hello_frame(&sampled)),
                (SNAPSHOT_SCHEMA, snapshot_frame(&sampled)),
            ] {
                let frame: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                validate_data(schema, &frame["data"]).unwrap();
            }
        }
    }

    /// Preserve workspace repo order for local CLI parity.
    #[test]
    fn the_producer_keeps_workspace_repos_in_stored_order() {
        let mut instance = Instance::new("s1", "/srv/repo");
        instance.source_profile = "main".into();
        instance.workspace_info = Some(WorkspaceInfo {
            branch: "main".into(),
            workspace_dir: "/srv/repo/.aoe/workspace".into(),
            created_at: Utc::now(),
            cleanup_on_delete: true,
            repos: vec![repo("beta", "/srv/beta"), repo("alpha", "/srv/alpha")],
        });
        let emitted = SessionRead::from_instance(&instance);
        assert_eq!(
            emitted
                .workspace_repos
                .iter()
                .map(|repo| (repo.name.as_str(), repo.source_path.as_str()))
                .collect::<Vec<_>>(),
            vec![("beta", "/srv/beta"), ("alpha", "/srv/alpha")],
            "the stored order must reach the wire unchanged"
        );
        assert_eq!(
            emitted
                .workspace_repos
                .iter()
                .map(|repo| (repo.name.as_str(), repo.source_path.as_str()))
                .collect::<Vec<_>>(),
            instance
                .workspace_info
                .as_ref()
                .expect("workspace_info")
                .repos
                .iter()
                .map(|repo| (repo.name.as_str(), repo.source_path.as_str()))
                .collect::<Vec<_>>(),
            "the emitted order is the store's own, row for row"
        );

        // The client half, a repeated (name, source_path) pair refused and any
        // order accepted, is pinned in the client's own
        // `workspace_repos_are_accepted_in_any_order_and_refused_when_repeated`.
    }

    fn repo(name: &str, source_path: &str) -> StoredRepo {
        StoredRepo {
            name: name.into(),
            source_path: source_path.into(),
            branch: "main".into(),
            worktree_path: format!("{source_path}/.worktrees/main"),
            main_repo_path: source_path.into(),
            managed_by_aoe: true,
            branch_preexisting: false,
            base_branch: None,
            base_branch_override: None,
        }
    }

    async fn cached_snapshot(state: &Arc<AppState>) -> crate::cli::runtime_read::dto::SnapshotData {
        let instances = state.instances.read().await;
        let cache = state.runtime_read_cache.read().unwrap();
        let mut rows = Vec::with_capacity(instances.len() + cache.alias_only_instances.len());
        rows.extend(
            instances
                .iter()
                .chain(cache.alias_only_instances.iter())
                .map(SessionRead::from_instance),
        );
        let sampled = build_snapshot(
            &RuntimeState::new(),
            rows,
            Owner::remote(),
            Utc::now(),
            &cache,
        );
        let hello = parse_hello(&hello_frame(&sampled)).unwrap();
        let snapshot = parse_snapshot(&snapshot_frame(&sampled)).unwrap();
        validate_hello(&hello).unwrap();
        validate_snapshot(&snapshot).unwrap();
        validate_cross_message(&hello, &snapshot, None).unwrap();
        snapshot
    }

    async fn native_session_ids(state: &Arc<AppState>) -> Vec<String> {
        let response = super::super::api::list_sessions(
            axum::extract::State(state.clone()),
            axum::extract::Query(crate::daemon::ListSessionsQuery { state: None }),
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let envelope: crate::daemon::SessionsEnvelope = serde_json::from_slice(&body).unwrap();
        envelope.sessions.into_iter().map(|row| row.id).collect()
    }

    fn cached_data_healthy(
        snapshot: &crate::cli::runtime_read::dto::SnapshotData,
        profile: &str,
    ) -> bool {
        let profile = snapshot
            .profiles
            .iter()
            .find(|row| row.name == profile)
            .unwrap();
        crate::cli::runtime_read::dto::profile_component_healthy(&profile.health.profile_data)
    }

    #[test]
    #[serial_test::serial]
    fn stored_rows_outside_the_contract_degrade_only_their_component() {
        use crate::cli::runtime_read::dto::{component_healthy, profile_component_healthy};

        let home = TempHome::new();
        Storage::new_unwatched("healthy").unwrap();
        let storage = Storage::new_unwatched("broken").unwrap();
        let directory = home.app_dir().join("profiles/broken");
        std::fs::write(
            directory.join("projects.json"),
            br#"[{"name":"kept","path":"/srv/kept","scope":"profile"}]"#,
        )
        .unwrap();
        let healthy = named("healthy-row", "healthy");
        for case in [
            "workspace-trailing",
            "workspace-dot",
            "workspace-parent",
            "workspace-duplicate",
            "relative-project",
            "tilde-project",
            "relative-worktree",
            "control-id",
            "bidi-id",
            "bidi-parent",
            "bidi-agent-id",
            "duplicate-id",
            "deep-parent-chain",
            "group-duplicate",
            "group-name",
            "global-project-duplicate",
        ] {
            let invalid_groups = matches!(case, "group-duplicate" | "group-name");
            let invalid_global = case == "global-project-duplicate";
            let invalid_sessions = !invalid_groups && !invalid_global;
            let mut groups = vec![Group::new("kept", "kept")];
            match case {
                "group-duplicate" => groups.push(Group::new("kept", "kept")),
                "group-name" => groups.push(Group::new("wrong", "session/child")),
                _ => {}
            }
            let stored_groups = serde_json::to_vec(&groups).unwrap();
            std::fs::write(directory.join("groups.json"), &stored_groups).unwrap();
            let loaded_groups = storage.load_groups_readonly().unwrap();
            assert_eq!(
                loaded_groups
                    .iter()
                    .map(|group| (&group.name, &group.path))
                    .collect::<Vec<_>>(),
                groups
                    .iter()
                    .map(|group| (&group.name, &group.path))
                    .collect::<Vec<_>>(),
                "{case}: the store must accept the regression input"
            );
            let global_projects: &[u8] = if invalid_global {
                br#"[{"name":"global","path":"/srv/global"},{"name":"global","path":"/srv/global"}]"#
            } else {
                br#"[{"name":"global","path":"/srv/global"}]"#
            };
            std::fs::write(home.app_dir().join("projects.json"), global_projects).unwrap();
            assert_eq!(
                crate::session::projects::load_global().unwrap().len(),
                if invalid_global { 2 } else { 1 },
                "{case}: the global store must accept the regression input"
            );
            let mut bad = named("bad-row", "broken");
            let mut rows = vec![named("otherwise-valid", "broken")];
            if !invalid_sessions {
                bad.group_path = "session/child".into();
                rows[0].group_path = "session/child".into();
            }
            match case {
                "workspace-trailing"
                | "workspace-dot"
                | "workspace-parent"
                | "workspace-duplicate" => {
                    let source = match case {
                        "workspace-trailing" => "/src/a/",
                        "workspace-dot" => "/src/./a",
                        "workspace-parent" => "/src/a/../b",
                        _ => "/src/a",
                    };
                    let repository = repo("a", source);
                    bad.workspace_info = Some(WorkspaceInfo {
                        branch: "main".into(),
                        workspace_dir: "/workspace".into(),
                        created_at: Utc::now(),
                        cleanup_on_delete: true,
                        repos: if case == "workspace-duplicate" {
                            vec![repository.clone(), repository]
                        } else {
                            vec![repository]
                        },
                    });
                }
                "relative-project" => bad.project_path = "relative/repo".into(),
                "tilde-project" => bad.project_path = "~/repo".into(),
                "relative-worktree" => {
                    bad.worktree_info = Some(WorktreeInfo {
                        branch: "main".into(),
                        main_repo_path: "relative/repo".into(),
                        managed_by_aoe: true,
                        created_at: Utc::now(),
                        base_branch: None,
                    });
                }
                "control-id" => bad.id = "bad\nid".into(),
                "bidi-id" => bad.id = "bad\u{202e}id".into(),
                "bidi-parent" => bad.parent_session_id = Some("bad\u{202e}parent".into()),
                "bidi-agent-id" => bad.agent_session_id = Some("bad\u{202e}agent".into()),
                "duplicate-id" => rows.push(bad.clone()),
                "deep-parent-chain" => {
                    for index in 0..65 {
                        let mut row = named(&format!("chain-{index}"), "broken");
                        row.parent_session_id =
                            (index < 64).then(|| format!("chain-{}", index + 1));
                        rows.push(row);
                    }
                }
                "group-duplicate" | "group-name" => {}
                "global-project-duplicate" => bad.project_path = "/srv/global".into(),
                _ => unreachable!(),
            }
            rows.push(bad);
            let stored = serde_json::to_vec(&rows).unwrap();
            std::fs::write(directory.join("sessions.json"), &stored).unwrap();
            let loaded = storage.load_instances_readonly().unwrap();
            assert_eq!(
                loaded.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
                rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
                "{case}: the store must accept the regression input"
            );
            let instances = std::iter::once(&healthy).chain(loaded.iter());
            let sampled = build_snapshot(
                &RuntimeState::new(),
                instances.map(SessionRead::from_instance).collect(),
                Owner::remote(),
                Utc::now(),
                &crate::server::reload::RuntimeReadCache::accepted_inventory(),
            );
            let hello = parse_hello(&hello_frame(&sampled)).unwrap();
            let snapshot = parse_snapshot(&snapshot_frame(&sampled)).unwrap();
            validate_hello(&hello).unwrap();
            validate_snapshot(&snapshot).unwrap();
            validate_cross_message(&hello, &snapshot, None).unwrap();
            let expected_ids: &[&str] = if invalid_sessions {
                &["healthy-row"]
            } else {
                &["healthy-row", "otherwise-valid", "bad-row"]
            };
            assert_eq!(
                snapshot
                    .sessions
                    .iter()
                    .map(|row| row.id.as_str())
                    .collect::<Vec<_>>(),
                expected_ids,
                "{case}"
            );
            assert!(cached_data_healthy(&snapshot, "healthy"), "{case}");
            assert_eq!(
                cached_data_healthy(&snapshot, "broken"),
                !invalid_sessions,
                "{case}"
            );
            let healthy_profile = snapshot
                .profiles
                .iter()
                .find(|row| row.name == "healthy")
                .unwrap();
            assert!(
                profile_component_healthy(&healthy_profile.health.profile_enumeration)
                    && profile_component_healthy(&healthy_profile.health.metadata),
                "{case}"
            );
            assert!(
                component_healthy(&snapshot.health.global_enumeration),
                "{case}"
            );
            assert_eq!(
                component_healthy(&snapshot.health.global_metadata),
                !invalid_global,
                "{case}"
            );
            let broken = snapshot
                .profiles
                .iter()
                .find(|row| row.name == "broken")
                .unwrap();
            assert!(profile_component_healthy(&broken.health.metadata), "{case}");
            assert_eq!(
                profile_component_healthy(&broken.health.profile_enumeration),
                !invalid_groups,
                "{case}"
            );
            assert_eq!(
                broken.groups.iter().any(|group| group.path == "kept"),
                !invalid_groups,
                "{case}"
            );
            if !invalid_sessions {
                for path in ["session", "session/child"] {
                    assert!(
                        broken.groups.iter().any(|group| group.path == path),
                        "{case}"
                    );
                }
            }
            if invalid_global {
                assert!(snapshot.global_projects.is_empty(), "{case}");
                assert!(
                    broken
                        .projects
                        .iter()
                        .any(|project| { project.path == "/srv/global" && !project.registered }),
                    "{case}"
                );
            } else {
                assert_eq!(snapshot.global_projects[0].path, "/srv/global", "{case}");
            }
            assert!(
                broken
                    .projects
                    .iter()
                    .any(|project| project.path == "/srv/kept"),
                "{case}"
            );
            assert_eq!(
                std::fs::read(directory.join("sessions.json")).unwrap(),
                stored,
                "{case}"
            );
            assert_eq!(
                std::fs::read(directory.join("groups.json")).unwrap(),
                stored_groups,
                "{case}"
            );
            assert_eq!(
                std::fs::read(home.app_dir().join("projects.json")).unwrap(),
                global_projects,
                "{case}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_profile_created_between_native_and_read_scans_has_rows_without_partial_native_health(
    ) -> anyhow::Result<()> {
        use super::super::reload::{
            load_all_profiles, load_read_projection, reload_state_instances_from_disk,
        };
        use super::super::state::StatusSource;
        use std::sync::atomic::Ordering;
        let _home = TempHome::new();
        let main = Storage::new_unwatched("main")?;
        crate::session::config::update_config(|config| config.default_profile = "main".into())?;
        let state =
            super::super::test_support::build_test_app_state_configured(Vec::new(), |state| {
                state.profile = std::sync::RwLock::new("main".into())
            });
        let epoch = state.mutation_epoch.load(Ordering::SeqCst);
        let first = load_all_profiles(&state.file_watch)?;
        assert!(first.instances.is_empty());
        let main_identity = first
            .inventory
            .iter()
            .find(|profile| profile.name == "main")
            .unwrap()
            .identity;
        main.update(|rows, _| {
            rows.push(named("later-main-row", "main"));
            Ok(())
        })?;
        let fresh = Storage::new_unwatched("fresh")?;
        fresh.update(|rows, _| {
            let mut row = named("between-scans-row", "fresh");
            row.title = "second-scan profile B".into();
            rows.push(row);
            Ok(())
        })?;
        let (loaded, metadata) = load_read_projection(&state.file_watch, Ok(first));
        assert!(
            loaded.instances.is_empty(),
            "CLI additions cannot enter the earlier native snapshot"
        );
        assert_eq!(
            loaded
                .cache
                .inventory
                .iter()
                .find(|profile| profile.name == "main")
                .unwrap()
                .identity,
            main_identity
        );
        reload_state_instances_from_disk(
            &state,
            loaded,
            Vec::new(),
            StatusSource::DiskOnly,
            epoch,
            metadata,
            Default::default(),
        )
        .await;
        let snapshot = cached_snapshot(&state).await;
        assert!(cached_data_healthy(&snapshot, "main"));
        assert!(cached_data_healthy(&snapshot, "fresh"));
        assert_eq!(
            snapshot
                .sessions
                .iter()
                .map(|row| (row.id.as_str(), row.profile.as_str(), row.title.as_str()))
                .collect::<Vec<_>>(),
            [("between-scans-row", "fresh", "second-scan profile B")]
        );
        let native = state.runtime.publish(&state).await?;
        assert_eq!(
            native.value.contents.health,
            crate::daemon::RuntimeHealth::Healthy
        );
        assert!(native.value.contents.sessions.is_empty());
        assert_eq!(
            native
                .value
                .contents
                .profiles
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>(),
            ["main"]
        );
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_name_retargeted_between_scans_cannot_recertify_its_cached_rows() -> anyhow::Result<()>
    {
        use super::super::reload::{
            load_all_profiles, load_read_projection, reload_state_instances_from_disk,
        };
        use super::super::state::StatusSource;
        use std::sync::atomic::Ordering;
        let home = TempHome::new();
        Storage::new_unwatched("main")?.update(|rows, _| {
            rows.push(named("old-main", "main"));
            Ok(())
        })?;
        crate::session::config::update_config(|config| config.default_profile = "main".into())?;
        let state =
            super::super::test_support::build_test_app_state_configured(Vec::new(), |state| {
                state.profile = std::sync::RwLock::new("main".into())
            });
        let epoch = state.mutation_epoch.load(Ordering::SeqCst);
        let first = load_all_profiles(&state.file_watch)?;
        let identity = first
            .inventory
            .iter()
            .find(|profile| profile.name == "main")
            .unwrap()
            .identity;
        let retired = tempfile::tempdir()?;
        std::fs::rename(
            home.app_dir().join("profiles/main"),
            retired.path().join("main"),
        )?;
        let replacement = Storage::new_unwatched("main")?;
        replacement.update(|rows, _| {
            rows.push(named("replacement-main", "main"));
            Ok(())
        })?;
        let bytes = std::fs::read(replacement.sessions_path())?;
        let (loaded, metadata) = load_read_projection(&state.file_watch, Ok(first));
        assert_eq!(
            loaded
                .cache
                .inventory
                .iter()
                .find(|profile| profile.name == "main")
                .unwrap()
                .identity,
            identity
        );
        reload_state_instances_from_disk(
            &state,
            loaded,
            Vec::new(),
            StatusSource::DiskOnly,
            epoch,
            metadata,
            Default::default(),
        )
        .await;
        let snapshot = cached_snapshot(&state).await;
        assert!(!cached_data_healthy(&snapshot, "main"));
        assert!(snapshot.sessions.iter().any(|row| row.id == "old-main"));
        assert!(!snapshot
            .sessions
            .iter()
            .any(|row| row.id == "replacement-main"));
        assert_eq!(std::fs::read(replacement.sessions_path())?, bytes);
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn new_listed_profiles_read_their_store_without_replacing_live_rows() {
        let home = TempHome::new();
        let mut overlay = named("overlay", "main");
        overlay.title = "live overlay".into();
        let state = super::super::test_support::build_test_app_state(Vec::new());
        Storage::new_unwatched("main").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("main", home.app_dir().join("profiles/retarget")).unwrap();
        super::super::test_support::accept_runtime_read_cache_for_test(&state).await;
        *state.instances.write().await = vec![overlay];
        let accepted_names: Vec<_> = state
            .runtime_read_cache
            .read()
            .unwrap()
            .inventory
            .iter()
            .map(|profile| profile.name.clone())
            .collect();
        crate::session::create_profile("empty").unwrap();
        let fresh = Storage::new_unwatched("fresh").unwrap();
        fresh
            .update(|rows, _| {
                *rows = vec![named("fresh-row", "fresh")];
                Ok(())
            })
            .unwrap();
        let mut fresh_live = named("fresh-row", "fresh");
        fresh_live.title = "live fresh title".into();
        state.instances.write().await.push(fresh_live);
        #[cfg(unix)]
        {
            std::fs::remove_file(home.app_dir().join("profiles/retarget")).unwrap();
            Storage::new_unwatched("retarget")
                .unwrap()
                .update(|rows, _| {
                    *rows = vec![named("retargeted-row", "retarget")];
                    Ok(())
                })
                .unwrap();
        }
        Storage::new_unwatched("corrupt").unwrap();
        let corrupt = home.app_dir().join("profiles/corrupt/sessions.json");
        std::fs::write(&corrupt, b"not json").unwrap();
        let before = std::fs::read(fresh.sessions_path()).unwrap();

        for _ in 0..2 {
            let snapshot = cached_snapshot(&state).await;
            assert!(cached_data_healthy(&snapshot, "empty"));
            assert!(cached_data_healthy(&snapshot, "fresh"));
            assert!(!cached_data_healthy(&snapshot, "corrupt"));
            #[cfg(unix)]
            assert!(!cached_data_healthy(&snapshot, "retarget"));
            let rows: Vec<_> = snapshot
                .sessions
                .iter()
                .map(|row| (row.profile.as_str(), row.id.as_str()))
                .collect();
            assert_eq!(rows, [("main", "overlay"), ("fresh", "fresh-row")]);
            assert_eq!(
                snapshot
                    .sessions
                    .iter()
                    .find(|row| row.profile == "fresh")
                    .unwrap()
                    .title,
                "live fresh title"
            );
            let live = state.instances.read().await;
            assert_eq!(
                live.iter()
                    .map(|row| (row.id.as_str(), row.title.as_str()))
                    .collect::<Vec<_>>(),
                [
                    ("overlay", "live overlay"),
                    ("fresh-row", "live fresh title")
                ]
            );
            assert_eq!(
                state
                    .runtime_read_cache
                    .read()
                    .unwrap()
                    .inventory
                    .iter()
                    .map(|profile| profile.name.clone())
                    .collect::<Vec<_>>(),
                accepted_names
            );
            assert_eq!(std::fs::read(fresh.sessions_path()).unwrap(), before);
            assert_eq!(std::fs::read(&corrupt).unwrap(), b"not json");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn session_load_failures_recover_only_with_an_accepted_reload() {
        use crate::server::{
            reload::reload_state_instances_from_disk, state::StatusSource, test_support,
        };
        use std::sync::atomic::Ordering;
        let home = TempHome::new();
        for mode in [StatusSource::DiskOnly, StatusSource::TmuxApplied] {
            for (profile, id) in [("main", "bad"), ("safe", "good")] {
                Storage::new_unwatched(profile)
                    .unwrap()
                    .update(|rows, _| {
                        *rows = vec![named(id, profile)];
                        Ok(())
                    })
                    .unwrap();
            }
            let state = test_support::build_test_app_state(Vec::new());
            async {
                let (loaded, metadata) =
                    crate::server::reload::load_profiles_for_reload(&state.file_watch);
                reload_state_instances_from_disk(
                    &state,
                    loaded,
                    vec![],
                    mode,
                    0,
                    metadata,
                    Default::default(),
                )
                .await
            }
            .await;
            let path = home.app_dir().join("profiles/main/sessions.json");
            let original = std::fs::read(&path).unwrap();
            std::fs::write(&path, "not json").unwrap();
            let (failed, failed_metadata) =
                crate::server::reload::load_profiles_for_reload(&state.file_watch);
            state.mutation_epoch.store(1, Ordering::SeqCst);
            reload_state_instances_from_disk(
                &state,
                failed,
                vec![],
                mode,
                0,
                failed_metadata,
                Default::default(),
            )
            .await;
            let snapshot = cached_snapshot(&state).await;
            assert!(
                cached_data_healthy(&snapshot, "main"),
                "a stale failed load cannot degrade current rows"
            );
            assert!(snapshot.sessions.iter().any(|row| row.id == "bad"));
            async {
                let (loaded, metadata) =
                    crate::server::reload::load_profiles_for_reload(&state.file_watch);
                reload_state_instances_from_disk(
                    &state,
                    loaded,
                    vec![],
                    mode,
                    1,
                    metadata,
                    Default::default(),
                )
                .await
            }
            .await;
            let snapshot = cached_snapshot(&state).await;
            assert!(!cached_data_healthy(&snapshot, "main"));
            assert!(!snapshot.sessions.iter().any(|row| row.id == "bad"));
            assert!(cached_data_healthy(&snapshot, "safe"));
            assert!(snapshot.sessions.iter().any(|row| row.id == "good"));
            std::fs::write(&path, &original).unwrap();
            assert!(
                !cached_data_healthy(&cached_snapshot(&state).await, "main"),
                "disk repair is not cache recovery"
            );
            let (recovered, recovered_metadata) =
                crate::server::reload::load_profiles_for_reload(&state.file_watch);
            state.mutation_epoch.store(2, Ordering::SeqCst);
            reload_state_instances_from_disk(
                &state,
                recovered,
                vec![],
                mode,
                1,
                recovered_metadata,
                Default::default(),
            )
            .await;
            assert!(
                !cached_data_healthy(&cached_snapshot(&state).await, "main"),
                "a stale healthy load cannot clear a newer failure"
            );
            async {
                let (loaded, metadata) =
                    crate::server::reload::load_profiles_for_reload(&state.file_watch);
                reload_state_instances_from_disk(
                    &state,
                    loaded,
                    vec![],
                    mode,
                    2,
                    metadata,
                    Default::default(),
                )
                .await
            }
            .await;
            let snapshot = cached_snapshot(&state).await;
            assert!(cached_data_healthy(&snapshot, "main"));
            assert!(snapshot.sessions.iter().any(|row| row.id == "bad"));
            std::fs::write(&path, "[]").unwrap();
            async {
                let (loaded, metadata) =
                    crate::server::reload::load_profiles_for_reload(&state.file_watch);
                reload_state_instances_from_disk(
                    &state,
                    loaded,
                    vec![],
                    mode,
                    2,
                    metadata,
                    Default::default(),
                )
                .await
            }
            .await;
            let snapshot = cached_snapshot(&state).await;
            assert!(cached_data_healthy(&snapshot, "main"));
            assert!(!snapshot.sessions.iter().any(|row| row.profile == "main"));
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_blocked_alias_degrades_only_its_own_metadata_in_the_snapshot() {
        use crate::cli::runtime_read::dto::profile_component_healthy;
        use crate::server::alias_io::{held::Hold, in_flight_for_test, wait_idle_for_test};
        let home = TempHome::new();
        Storage::new_unwatched("main").unwrap();
        let external = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(external.path(), home.app_dir().join("profiles/ext")).unwrap();
        let state = super::super::test_support::build_test_app_state(Vec::new());
        super::super::test_support::accept_runtime_read_cache_for_test(&state).await;
        let metadata_healthy = |snapshot: &crate::cli::runtime_read::dto::SnapshotData, name| {
            let profile = snapshot
                .profiles
                .iter()
                .find(|row| row.name == name)
                .unwrap();
            profile_component_healthy(&profile.health.metadata)
        };
        let healthy = cached_snapshot(&state).await;
        assert!(metadata_healthy(&healthy, "ext"));

        let hold = Hold::install("ext", "projects");
        for _ in 0..2 {
            let blocked = cached_snapshot(&state).await;
            assert!(metadata_healthy(&blocked, "main"));
            assert!(!metadata_healthy(&blocked, "ext"));
        }
        hold.wait_entered();
        assert_eq!(in_flight_for_test("ext"), 1, "no second worker starts");

        hold.release();
        wait_idle_for_test("ext");
        assert!(metadata_healthy(&cached_snapshot(&state).await, "ext"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn session_enumeration_failure_does_not_block_recovered_inventory() {
        use crate::server::{
            reload::{load_profiles_for_reload, reload_state_instances_from_disk},
            state::StatusSource,
            test_support,
        };
        let home = TempHome::new();
        let profiles = home.app_dir().join("profiles");
        std::fs::create_dir_all(home.app_dir()).unwrap();
        std::fs::write(&profiles, "not a directory").unwrap();
        let state = test_support::build_test_app_state(Vec::new());
        let reload = |(loaded, metadata)| {
            reload_state_instances_from_disk(
                &state,
                loaded,
                vec![],
                StatusSource::DiskOnly,
                0,
                metadata,
                Default::default(),
            )
        };
        reload(load_profiles_for_reload(&state.file_watch)).await;
        let snapshot = cached_snapshot(&state).await;
        assert!(!crate::cli::runtime_read::dto::component_healthy(
            &snapshot.health.global_enumeration
        ));
        std::fs::remove_file(&profiles).unwrap();
        Storage::new_unwatched("main")
            .unwrap()
            .update(|_, _| Ok(()))
            .unwrap();
        let snapshot = cached_snapshot(&state).await;
        assert!(!cached_data_healthy(&snapshot, "main"));
        assert!(crate::cli::runtime_read::dto::component_healthy(
            &snapshot.health.global_enumeration
        ));
        assert!(crate::cli::runtime_read::dto::profile_component_healthy(
            &snapshot.profiles[0].health.metadata
        ));
        reload(load_profiles_for_reload(&state.file_watch)).await;
        assert!(cached_data_healthy(&cached_snapshot(&state).await, "main"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn failed_enumeration_keeps_rows_until_a_successful_empty_inventory() {
        use crate::daemon::{ReloadFailureCode, RuntimeHealth};
        use crate::server::{
            reload::{
                load_all_profiles, load_profiles_for_reload, load_read_projection,
                reload_state_instances_from_disk,
            },
            state::StatusSource,
            test_support,
        };
        use std::sync::atomic::Ordering;

        for mode in [StatusSource::DiskOnly, StatusSource::TmuxApplied] {
            let home = TempHome::new();
            Storage::new_unwatched("main")
                .unwrap()
                .update(|rows, _| {
                    *rows = vec![named("kept", "main")];
                    Ok(())
                })
                .unwrap();
            let state = test_support::build_test_app_state(Vec::new());
            let reload = |(loaded, metadata), read_epoch| {
                reload_state_instances_from_disk(
                    &state,
                    loaded,
                    vec![],
                    mode,
                    read_epoch,
                    metadata,
                    Default::default(),
                )
            };
            reload(load_profiles_for_reload(&state.file_watch), 0).await;
            state.instances.write().await[0].last_error = Some("daemon-only".into());
            let accepted_metadata = state.canonical_metadata.read().await.clone();

            let profiles = home.app_dir().join("profiles");
            let stash = home.app_dir().join("profiles-stash");
            std::fs::rename(&profiles, &stash).unwrap();
            std::fs::write(&profiles, "not a directory").unwrap();
            let failed = load_profiles_for_reload(&state.file_watch);
            assert!(failed.0.cache.health.enumeration_failed);
            state.mutation_epoch.store(1, Ordering::SeqCst);
            reload(failed, 0).await;
            assert!(
                !state
                    .runtime_read_cache
                    .read()
                    .unwrap()
                    .health
                    .enumeration_failed
            );
            assert_eq!(*state.canonical_health.read().await, RuntimeHealth::Healthy);

            reload(load_profiles_for_reload(&state.file_watch), 1).await;
            assert!(
                state
                    .runtime_read_cache
                    .read()
                    .unwrap()
                    .health
                    .enumeration_failed
            );
            let degraded = RuntimeHealth::Degraded {
                code: ReloadFailureCode::ProfileEnumeration,
                profiles: Vec::new(),
            };
            assert_eq!(*state.canonical_health.read().await, degraded);
            assert_eq!(*state.canonical_metadata.read().await, accepted_metadata);
            let snapshot = cached_snapshot(&state).await;
            assert!(!crate::cli::runtime_read::dto::component_healthy(
                &snapshot.health.global_enumeration
            ));
            assert!(snapshot.sessions.iter().any(|row| row.id == "kept"));
            assert_eq!(
                state.instances.read().await[0].last_error.as_deref(),
                Some("daemon-only")
            );

            std::fs::remove_file(&profiles).unwrap();
            std::fs::rename(&stash, &profiles).unwrap();
            let canonical = load_all_profiles(&state.file_watch);
            assert!(canonical.is_ok());
            std::fs::rename(&profiles, &stash).unwrap();
            std::fs::write(&profiles, "not a directory").unwrap();
            let partial = load_read_projection(&state.file_watch, canonical);
            assert!(partial.0.cache.health.enumeration_failed);
            assert!(partial.1.is_ok());
            reload(partial, 1).await;
            assert_eq!(*state.canonical_health.read().await, degraded);
            assert_eq!(state.instances.read().await.len(), 1);

            std::fs::remove_file(&profiles).unwrap();
            std::fs::rename(&stash, &profiles).unwrap();
            reload(load_profiles_for_reload(&state.file_watch), 1).await;
            assert!(
                !state
                    .runtime_read_cache
                    .read()
                    .unwrap()
                    .health
                    .enumeration_failed
            );
            assert_eq!(*state.canonical_health.read().await, RuntimeHealth::Healthy);
            assert_eq!(state.instances.read().await.len(), 1);

            std::fs::remove_dir_all(&profiles).unwrap();
            reload(load_profiles_for_reload(&state.file_watch), 1).await;
            assert!(state.instances.read().await.is_empty());
            assert!(cached_snapshot(&state).await.sessions.is_empty());
        }
    }

    /// Project and group registry errors degrade only their own components.
    #[test]
    #[serial_test::serial]
    fn one_unreadable_profile_registry_degrades_only_that_profile() {
        let home = TempHome::new();
        let profiles = home.app_dir().join("profiles");
        std::fs::create_dir_all(profiles.join("main")).expect("main profile");
        std::fs::create_dir_all(profiles.join("broken").join("projects.json"))
            .expect("a directory where the registry belongs: a read that cannot succeed");
        std::fs::create_dir_all(profiles.join("broken-groups").join("groups.json")).unwrap();
        let instances = [instance("a", "main"), instance("b", "broken")];
        let sampled = build_snapshot(
            &RuntimeState::new(),
            instances.iter().map(SessionRead::from_instance).collect(),
            Owner::remote(),
            Utc::now(),
            &crate::server::reload::RuntimeReadCache::accepted_inventory(),
        );

        let hello = parse_hello(&hello_frame(&sampled)).expect("the client decodes the Hello");
        validate_hello(&hello).expect("a degraded profile is not a protocol violation");
        let snapshot =
            parse_snapshot(&snapshot_frame(&sampled)).expect("the client decodes the Snapshot");
        validate_snapshot(&snapshot).expect("the Snapshot is still usable");
        validate_cross_message(&hello, &snapshot, None).expect("Hello and Snapshot agree");

        assert!(
            matches!(
                hello.health,
                crate::cli::runtime_read::dto::AggregateHealth::Degraded { .. }
            ),
            "the roll-up reports the degradation: {:?}",
            hello.health
        );
        let broken = snapshot
            .profiles
            .iter()
            .find(|profile| profile.name == "broken")
            .expect("the profile is still published");
        assert_eq!(
            serde_json::to_value(broken.health.metadata).expect("encodes"),
            serde_json::json!({"kind": "degraded", "code": "metadata"}),
            "the bad profile says so, and the good one is untouched"
        );
        assert!(crate::cli::runtime_read::dto::profile_component_healthy(
            &broken.health.profile_enumeration
        ));
        assert!(cached_data_healthy(&snapshot, "broken"));
        let groups = snapshot
            .profiles
            .iter()
            .find(|row| row.name == "broken-groups")
            .unwrap();
        assert!(!crate::cli::runtime_read::dto::profile_component_healthy(
            &groups.health.profile_enumeration
        ));
        assert!(crate::cli::runtime_read::dto::profile_component_healthy(
            &groups.health.metadata
        ));
        assert!(cached_data_healthy(&snapshot, "broken-groups"));
        let main = snapshot
            .profiles
            .iter()
            .find(|profile| profile.name == "main")
            .expect("the healthy profile is published");
        assert_eq!(
            serde_json::to_value(main.health.metadata).expect("encodes"),
            serde_json::json!({"kind": "healthy"})
        );
    }

    /// Read-only projection must not create quarantine sidecars.
    #[test]
    #[serial_test::serial]
    fn a_served_read_leaves_the_profile_directory_untouched() {
        let home = TempHome::new();
        let profile_dir = home.app_dir().join("profiles").join("main");
        std::fs::create_dir_all(&profile_dir).expect("main profile");
        std::fs::write(profile_dir.join("sessions.json"), "[]").expect("an empty session list");
        let groups = serde_json::json!([
            Group::new("alpha", "work/alpha"),
            { "name": "corrupt-no-path" },
            Group::new("beta", "work/beta"),
        ]);
        std::fs::write(
            profile_dir.join("groups.json"),
            serde_json::to_vec(&groups).expect("groups encode"),
        )
        .expect("a registry with one row that cannot deserialise");
        let before = directory_contents(&profile_dir);

        let sampled = build_snapshot(
            &RuntimeState::new(),
            Vec::new(),
            Owner::remote(),
            Utc::now(),
            &crate::server::reload::RuntimeReadCache::accepted_inventory(),
        );

        let published: Vec<&str> = sampled.data.profiles[0]
            .groups
            .iter()
            .map(|group| group.path.as_str())
            .collect();
        assert_eq!(
            published,
            ["work/alpha", "work/beta"],
            "the readable rows still reach the snapshot, so this is not fixed by not reading"
        );
        assert_eq!(
            before,
            directory_contents(&profile_dir),
            "the served read must not write beside the store it reads"
        );
    }

    /// Every regular file under `dir` as a sorted `(name, bytes)` list, so a
    /// created file and a rewritten one both show up as a difference.
    fn directory_contents(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
        let mut contents: Vec<(String, Vec<u8>)> = std::fs::read_dir(dir)
            .expect("the profile directory")
            .map(|entry| {
                let entry = entry.expect("a directory entry");
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).expect("a readable file"),
                )
            })
            .collect();
        contents.sort();
        contents
    }

    /// A missing configured default must not silently select another profile.
    #[test]
    #[serial_test::serial]
    fn a_deleted_configured_default_publishes_no_default() {
        let home = TempHome::new();
        let app_dir = home.app_dir();
        std::fs::create_dir_all(app_dir.join("profiles").join("zeta")).expect("zeta profile");
        std::fs::write(app_dir.join("config.toml"), "default_profile = \"gone\"\n")
            .expect("config");

        let sampled = build_snapshot(
            &RuntimeState::new(),
            [instance("a", "zeta")]
                .iter()
                .map(SessionRead::from_instance)
                .collect(),
            Owner::remote(),
            Utc::now(),
            &crate::server::reload::RuntimeReadCache::accepted_inventory(),
        );
        assert_eq!(sampled.data.default_profile, None);
        assert_eq!(sampled.data.profiles.len(), 1);

        // The local half refuses the same state, which is what makes publishing
        // no default the matching answer rather than a second policy.
        let refused =
            crate::session::resolve_existing_profile("").expect_err("the local path refuses");
        assert!(
            refused.to_string().contains("does not exist"),
            "unexpected refusal: {refused}"
        );
    }

    /// Owned XDG fixture with process-global environment exclusion.
    struct TempHome {
        _dir: tempfile::TempDir,
        _env: crate::server::test_support::RuntimeEnvGuard,
    }

    impl TempHome {
        fn new() -> Self {
            let mut env = crate::server::test_support::RuntimeEnvGuard::read_lock();
            let dir = tempfile::tempdir().expect("temp home");
            env.bind(dir.path());
            Self {
                _dir: dir,
                _env: env,
            }
        }

        /// The app dir the seeded profiles and config live in.
        fn app_dir(&self) -> std::path::PathBuf {
            self._dir.path().join(crate::session::APP_DIR_NAME_XDG)
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

    /// The emitted frames satisfy the real client decoder and cross-message invariants.
    #[test]
    #[serial_test::serial]
    fn emitted_frames_satisfy_the_client_wire_contract() {
        let _home = TempHome::new();
        let instances = [instance("a", "main"), instance("b", "main")];
        let sampled = build_snapshot(
            &RuntimeState::new(),
            instances.iter().map(SessionRead::from_instance).collect(),
            Owner::remote(),
            Utc::now(),
            &crate::server::reload::RuntimeReadCache::accepted_inventory(),
        );

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
            [instance("a", "main")]
                .iter()
                .map(SessionRead::from_instance)
                .collect(),
            Owner::remote(),
            Utc::now(),
            &crate::server::reload::RuntimeReadCache::accepted_inventory(),
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
        assert!(observed_at.ends_with('Z'));
        assert!(
            chrono::DateTime::parse_from_rfc3339(observed_at).is_ok(),
            "{observed_at}"
        );
    }

    /// The cursor is the independent counter: it is the observed revision plus
    /// one, and each committed sample advances both by exactly one.
    #[test]
    #[serial_test::serial]
    fn each_sample_advances_revision_and_cursor_together() {
        let _home = TempHome::new();
        let runtime = RuntimeState::new();
        let first = build_snapshot(
            &runtime,
            Vec::new(),
            Owner::remote(),
            Utc::now(),
            &crate::server::reload::RuntimeReadCache::accepted_inventory(),
        );
        let second = build_snapshot(
            &runtime,
            Vec::new(),
            Owner::remote(),
            Utc::now(),
            &crate::server::reload::RuntimeReadCache::accepted_inventory(),
        );

        assert_eq!(observed_revision(&first), 1);
        assert_eq!(observed_revision(&second), 2);
        assert_eq!(first.data.cursor.revision, 2);
        assert_eq!(second.data.cursor.revision, 3);
        assert_eq!(first.data.cursor.epoch, second.data.cursor.epoch);
    }

    /// Counter exhaustion is unavailable freshness, not a malformed snapshot.
    #[test]
    #[serial_test::serial]
    fn a_latched_sampler_still_produces_a_snapshot_the_client_accepts() {
        let _home = TempHome::new();
        let runtime = RuntimeState::new();
        runtime.sampler.lock().unwrap().successes = u64::MAX;
        let sampled = build_snapshot(
            &runtime,
            Vec::new(),
            Owner::remote(),
            Utc::now(),
            &crate::server::reload::RuntimeReadCache::accepted_inventory(),
        );

        assert!(matches!(
            sampled.data.status_freshness,
            StatusFreshness::Unavailable { .. }
        ));
        assert!(runtime.sampler.lock().unwrap().latched);

        let frame = serde_json::to_string(&SnapshotFrame {
            kind: "snapshot",
            data: &sampled.data,
        })
        .expect("the snapshot encodes");
        let decoded = crate::cli::runtime_read::dto::parse_snapshot(frame.as_bytes())
            .expect("the client accepts a latched sample");
        assert!(
            crate::cli::runtime_read::dto::validate_snapshot(&decoded).is_ok(),
            "the latched cursor is one the client's validator accepts"
        );
        assert!(
            !crate::cli::runtime_read::dto::freshness_observed(&decoded.status_freshness),
            "and the renderer reads it as the freshness it cannot report"
        );

        let (again, cursor) = publish_freshness(&runtime, Utc::now());
        assert!(matches!(again, StatusFreshness::Unavailable { .. }));
        assert_eq!(cursor, LATCHED_CURSOR);
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
        assert!(
            !projects[0].registered,
            "a synthesized row is not a registry row"
        );
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

    #[test]
    #[serial_test::serial]
    fn persisted_group_keys_and_scoped_orphans_survive_projection() {
        let _home = TempHome::new();
        for key in [
            "team/",
            "work//task",
            "/task",
            "/",
            "work///task",
            "work/.",
            "work/..",
            "work/\nleaf",
        ] {
            let mut orphan = named("orphan", "main");
            orphan.parent_session_id = Some("deleted".into());
            let mut trailing = named("trailing", "main");
            trailing.project_path = "/repo/".into();
            let mut grouped = named("grouped", "main");
            grouped.group_path = key.into();
            let instances = [named("a", "main"), orphan, trailing, grouped];
            let sampled = build_snapshot(
                &RuntimeState::new(),
                instances.iter().map(SessionRead::from_instance).collect(),
                Owner::remote(),
                Utc::now(),
                &crate::server::reload::RuntimeReadCache::accepted_inventory(),
            );
            let snapshot = parse_snapshot(&snapshot_frame(&sampled)).unwrap();
            validate_snapshot(&snapshot).unwrap();
            let grouped = snapshot
                .sessions
                .iter()
                .find(|row| row.id == "grouped")
                .unwrap();
            assert_eq!(grouped.group_path, key);
            let groups = &snapshot
                .profiles
                .iter()
                .find(|profile| profile.name == "main")
                .unwrap()
                .groups;
            for expected in GroupTree::new_with_group_paths([key], &[]).get_all_groups() {
                assert!(
                    groups
                        .iter()
                        .any(|group| group.path == expected.path && group.name == expected.name),
                    "missing group {:?}",
                    expected.path
                );
            }
            let orphan = snapshot
                .sessions
                .iter()
                .find(|row| row.id == "orphan")
                .unwrap();
            assert_eq!(orphan.parent_session_id.as_deref(), Some("deleted"));
            let trailing = snapshot
                .sessions
                .iter()
                .find(|row| row.id == "trailing")
                .unwrap();
            assert_eq!(trailing.project_path, "/repo/");
        }
    }

    #[test]
    fn cycle_entries_preserve_tree_entries_and_resolved_suffixes() {
        let cases: &[(&[(usize, usize)], &[usize])] = &[
            (&[], &[]),
            (&[(0, 1), (1, 2)], &[]),
            (&[(2, 1), (1, 0)], &[]),
            (&[(0, 5), (1, 5), (5, 6)], &[]),
            (&[(0, 9), (9, 7), (7, 8), (8, 9), (2, 8)], &[9]),
            (&[(0, 1), (1, 0), (3, 4), (4, 3)], &[0, 3]),
            (&[(2, 2), (usize::MAX, usize::MAX)], &[2, usize::MAX]),
        ];
        for &(edges, cuts) in cases {
            let expected_cuts: HashSet<_> = cuts.iter().copied().collect();
            let expected_parents: HashMap<_, _> = edges
                .iter()
                .copied()
                .filter(|(row, _)| !expected_cuts.contains(row))
                .collect();
            for reversed in [false, true] {
                let mut edges = edges.to_vec();
                if reversed {
                    edges.reverse();
                }
                let mut parents = edges.into_iter().collect();
                assert_eq!(cycle_entries(&mut parents), expected_cuts);
                assert_eq!(parents, expected_parents);
            }
        }
        let mut chain: HashMap<_, _> = (0..4096).map(|row| (row, row + 1)).collect();
        let unchanged = chain.clone();
        assert!(cycle_entries(&mut chain).is_empty());
        assert_eq!(chain, unchanged);
    }

    #[test]
    fn profile_local_orphans_survive_while_cycles_are_severed() {
        let mut cross = row("a", Some("b"), "/repo");
        cross.profile = "other".into();
        let b = row("b", None, "/repo");
        let orphan = row("c", Some("nowhere"), "/repo");
        let mut cycle = vec![row("x", Some("y"), "/repo"), row("y", Some("x"), "/repo")];
        cycle.push(row("z", Some("x"), "/repo"));
        let mut rows = vec![cross, b, orphan];
        rows.append(&mut cycle);

        reconcile_legacy_rows(&mut rows);

        let parents: Vec<(&str, Option<&str>)> = rows
            .iter()
            .map(|row| (row.id.as_str(), row.parent_session_id.as_deref()))
            .collect();
        assert_eq!(
            parents,
            vec![
                ("a", Some("b")),
                ("b", None),
                ("c", Some("nowhere")),
                ("x", None),
                ("y", Some("x")),
                ("z", Some("x")),
            ]
        );
    }

    /// The wire keeps the fraction a stored timestamp carries: a session
    /// created mid-second comes back spelled the same way on the far side,
    /// which is what the local `--json` output prints.
    #[test]
    #[serial_test::serial]
    fn a_fractional_timestamp_survives_the_wire_intact() {
        let _home = TempHome::new();
        let mut inst = named("fractional", "main");
        inst.created_at = "2026-01-02T03:04:05.123456789Z".parse().expect("timestamp");
        inst.pinned_at = Some("2026-01-02T03:04:06.5Z".parse().expect("timestamp"));
        let sampled = build_snapshot(
            &RuntimeState::new(),
            [inst].iter().map(SessionRead::from_instance).collect(),
            Owner::remote(),
            Utc::now(),
            &crate::server::reload::RuntimeReadCache::accepted_inventory(),
        );

        let row = &sampled.data.sessions[0];
        assert_eq!(row.created_at, "2026-01-02T03:04:05.123456789Z");
        // `AutoSi` writes the smallest of 0, 3, 6 or 9 digits that keeps the
        // instant, so `.5` comes back as `.500`: the same spelling the local
        // `DateTime<Utc>` serializer produces.
        assert_eq!(row.pinned_at.as_deref(), Some("2026-01-02T03:04:06.500Z"));

        // And the client's own validator accepts the spelling it is handed.
        let snapshot =
            parse_snapshot(&snapshot_frame(&sampled)).expect("client accepts the Snapshot");
        validate_snapshot(&snapshot).expect("the snapshot is projectable");
    }

    #[test]
    #[serial_test::serial]
    fn recording_an_empty_store_does_not_create_app_directories() {
        for owner in [RecordedOwner::Local { uid: 0 }, RecordedOwner::Remote] {
            let home = TempHome::new();
            let frames = record_exchange(
                &[],
                owner,
                &RecordingPins {
                    runtime_epoch: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    prebind_instance_id: "bbbbbbbb-cccc-dddd-eeee-ffffffffffff".into(),
                    runtime_instance_id: "cccccccc-dddd-eeee-ffff-000000000000".into(),
                    observed_at: "2026-01-01T00:00:00Z".parse().unwrap(),
                },
            );
            let snapshot = parse_snapshot(&frames.snapshot).unwrap();
            validate_snapshot(&snapshot).unwrap();
            assert!(snapshot.profiles.is_empty());
            assert_eq!(snapshot.default_profile, None);
            assert!(!home.app_dir().exists(), "{owner:?}");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn physical_alias_stores_are_cached_once_and_reload_acceptance_controls_readiness() {
        use super::super::reload::reload_state_instances_from_disk;
        use super::super::state::StatusSource;
        use std::sync::atomic::Ordering;
        let home = TempHome::new();
        let canonical = Storage::new_unwatched("main").unwrap();
        canonical
            .update(|rows, _| {
                *rows = vec![named("local", "main")];
                Ok(())
            })
            .unwrap();
        let external = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let root = home.app_dir().join("profiles");
        std::os::unix::fs::symlink(root.join("main"), root.join("inside")).unwrap();
        for name in ["remote-a", "remote-b"] {
            std::os::unix::fs::symlink(external.path(), root.join(name)).unwrap();
        }
        std::os::unix::fs::symlink(empty.path(), root.join("empty")).unwrap();
        std::os::unix::fs::symlink(external.path().join("absent"), root.join("broken")).unwrap();
        std::fs::write(external.path().join("file"), b"x").unwrap();
        std::os::unix::fs::symlink(external.path().join("file"), root.join("not-directory"))
            .unwrap();
        let remote = Storage::open_reference("remote-a").unwrap();
        remote
            .update(|rows, _| {
                *rows = vec![named("outside", "remote-a")];
                Ok(())
            })
            .unwrap();
        std::fs::write(
            home.app_dir().join("config.toml"),
            "default_profile = \"remote-b\"\n",
        )
        .unwrap();
        let state = super::super::test_support::build_test_app_state(Vec::new());
        let (loaded, loaded_metadata) =
            crate::server::reload::load_profiles_for_reload(&state.file_watch);
        assert_eq!(
            loaded
                .instances
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["local"]
        );
        assert_eq!(
            loaded
                .cache
                .alias_only_instances
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["outside"]
        );
        reload_state_instances_from_disk(
            &state,
            loaded,
            vec![],
            StatusSource::DiskOnly,
            0,
            loaded_metadata,
            Default::default(),
        )
        .await;
        let snapshot = cached_snapshot(&state).await;
        assert_eq!(snapshot.default_profile.as_deref(), Some("remote-b"));
        assert_eq!(
            snapshot
                .profiles
                .iter()
                .map(|profile| (profile.name.as_str(), profile.listed))
                .collect::<Vec<_>>(),
            [("empty", false), ("main", true), ("remote-a", false)]
        );
        assert_eq!(snapshot.profiles[1].aliases, ["inside"]);
        assert_eq!(snapshot.profiles[2].aliases, ["remote-b"]);
        assert!(cached_data_healthy(&snapshot, "empty"));
        assert_eq!(
            state
                .instances
                .read()
                .await
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["local"]
        );
        assert_eq!(native_session_ids(&state).await, ["local"]);
        std::fs::write(external.path().join("sessions.json"), b"[]").unwrap();
        assert_eq!(
            cached_snapshot(&state)
                .await
                .sessions
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["local", "outside"]
        );
        std::fs::remove_file(root.join("remote-a")).unwrap();
        std::fs::write(
            external.path().join("groups.json"),
            br#"[{"name":"current","path":"current"}]"#,
        )
        .unwrap();
        let renamed = cached_snapshot(&state).await;
        assert_eq!(
            renamed
                .sessions
                .iter()
                .find(|row| row.id == "outside")
                .unwrap()
                .profile,
            "remote-b"
        );
        let profile = renamed
            .profiles
            .iter()
            .find(|profile| profile.name == "remote-b")
            .unwrap();
        assert_eq!(profile.groups[0].path, "current");
        assert!(cached_data_healthy(&renamed, "remote-b"));
        let replacement = tempfile::tempdir().unwrap();
        std::fs::write(
            replacement.path().join("sessions.json"),
            serde_json::to_vec(&vec![named("replacement", "remote-b")]).unwrap(),
        )
        .unwrap();
        std::fs::remove_file(root.join("remote-b")).unwrap();
        std::os::unix::fs::symlink(replacement.path(), root.join("remote-b")).unwrap();
        let retargeted = cached_snapshot(&state).await;
        assert!(!cached_data_healthy(&retargeted, "remote-b"));
        assert!(retargeted.sessions.iter().any(|row| row.id == "outside"));
        let (loaded, loaded_metadata) =
            crate::server::reload::load_profiles_for_reload(&state.file_watch);
        state.mutation_epoch.store(1, Ordering::SeqCst);
        reload_state_instances_from_disk(
            &state,
            loaded,
            vec![],
            StatusSource::DiskOnly,
            0,
            loaded_metadata,
            Default::default(),
        )
        .await;
        assert!(!cached_data_healthy(
            &cached_snapshot(&state).await,
            "remote-b"
        ));
        async {
            let (loaded, metadata) =
                crate::server::reload::load_profiles_for_reload(&state.file_watch);
            reload_state_instances_from_disk(
                &state,
                loaded,
                vec![],
                StatusSource::DiskOnly,
                1,
                metadata,
                Default::default(),
            )
            .await
        }
        .await;
        let accepted = cached_snapshot(&state).await;
        assert!(cached_data_healthy(&accepted, "remote-b"));
        assert_eq!(
            accepted
                .sessions
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["local", "replacement"]
        );
        std::fs::write(replacement.path().join("sessions.json"), b"not json").unwrap();
        async {
            let (loaded, metadata) =
                crate::server::reload::load_profiles_for_reload(&state.file_watch);
            reload_state_instances_from_disk(
                &state,
                loaded,
                vec![],
                StatusSource::DiskOnly,
                1,
                metadata,
                Default::default(),
            )
            .await
        }
        .await;
        assert!(!cached_data_healthy(
            &cached_snapshot(&state).await,
            "remote-b"
        ));
        std::fs::write(replacement.path().join("sessions.json"), b"[]").unwrap();
        assert!(!cached_data_healthy(
            &cached_snapshot(&state).await,
            "remote-b"
        ));
        async {
            let (loaded, metadata) =
                crate::server::reload::load_profiles_for_reload(&state.file_watch);
            reload_state_instances_from_disk(
                &state,
                loaded,
                vec![],
                StatusSource::DiskOnly,
                1,
                metadata,
                Default::default(),
            )
            .await
        }
        .await;
        assert!(cached_data_healthy(
            &cached_snapshot(&state).await,
            "remote-b"
        ));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cached_canonical_rows_follow_a_renamed_directory_with_the_old_name_as_alias() {
        let home = TempHome::new();
        Storage::new_unwatched("main")
            .unwrap()
            .update(|rows, _| {
                *rows = vec![named("cached", "main")];
                Ok(())
            })
            .unwrap();
        std::fs::write(
            home.app_dir().join("config.toml"),
            "default_profile = \"main\"\n",
        )
        .unwrap();
        let state = super::super::test_support::build_test_app_state(Vec::new());
        super::super::test_support::accept_runtime_read_cache_for_test(&state).await;
        let root = home.app_dir().join("profiles");
        std::fs::rename(root.join("main"), root.join("zeta")).unwrap();
        std::os::unix::fs::symlink(root.join("zeta"), root.join("main")).unwrap();
        std::fs::write(root.join("zeta/sessions.json"), b"[]").unwrap();
        let snapshot = cached_snapshot(&state).await;
        assert_eq!(
            snapshot
                .profiles
                .iter()
                .map(|profile| profile.name.as_str())
                .collect::<Vec<_>>(),
            ["zeta"]
        );
        assert!(snapshot.profiles[0].listed);
        assert_eq!(snapshot.profiles[0].aliases, ["main"]);
        assert_eq!(snapshot.sessions[0].id, "cached");
        assert_eq!(snapshot.sessions[0].profile, "zeta");
        assert!(cached_data_healthy(&snapshot, "zeta"));
        assert_eq!(snapshot.default_profile.as_deref(), Some("main"));
        assert_eq!(state.instances.read().await[0].source_profile, "main");
        super::super::test_support::accept_runtime_read_cache_for_test(&state).await;
        assert!(cached_snapshot(&state).await.sessions.is_empty());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn earlier_alias_representatives_preserve_empty_readiness_and_failed_load_latches() {
        use super::super::reload::reload_state_instances_from_disk;
        use super::super::state::StatusSource;
        for unreadable in [false, true] {
            let home = TempHome::new();
            let outside = tempfile::tempdir().unwrap();
            let root = home.app_dir().join("profiles");
            std::fs::create_dir_all(&root).unwrap();
            if unreadable {
                std::fs::write(outside.path().join("sessions.json"), b"not JSON").unwrap();
            }
            std::os::unix::fs::symlink(outside.path(), root.join("outside-z")).unwrap();
            let state = super::super::test_support::build_test_app_state(Vec::new());
            super::super::test_support::accept_runtime_read_cache_for_test(&state).await;
            std::os::unix::fs::symlink(outside.path(), root.join("outside-a")).unwrap();
            let renamed = cached_snapshot(&state).await;
            assert_eq!(
                renamed
                    .profiles
                    .iter()
                    .map(|profile| profile.name.as_str())
                    .collect::<Vec<_>>(),
                ["outside-a"]
            );
            assert_eq!(renamed.profiles[0].aliases, ["outside-z"]);
            assert_eq!(cached_data_healthy(&renamed, "outside-a"), !unreadable);
            assert!(renamed.sessions.is_empty());
            std::fs::write(outside.path().join("sessions.json"), b"[]").unwrap();
            assert_eq!(
                cached_data_healthy(&cached_snapshot(&state).await, "outside-a"),
                !unreadable
            );
            state
                .mutation_epoch
                .store(1, std::sync::atomic::Ordering::SeqCst);
            async {
                let (loaded, metadata) =
                    crate::server::reload::load_profiles_for_reload(&state.file_watch);
                reload_state_instances_from_disk(
                    &state,
                    loaded,
                    vec![],
                    StatusSource::DiskOnly,
                    0,
                    metadata,
                    Default::default(),
                )
                .await
            }
            .await;
            assert_eq!(
                cached_data_healthy(&cached_snapshot(&state).await, "outside-a"),
                !unreadable
            );
            super::super::test_support::accept_runtime_read_cache_for_test(&state).await;
            assert!(cached_data_healthy(
                &cached_snapshot(&state).await,
                "outside-a"
            ));
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn raw_lexical_aliases_and_legacy_directories_keep_read_inventory_semantics() {
        let home = TempHome::new();
        Storage::new_unwatched("main").unwrap();
        let root = home.app_dir().join("profiles");
        for name in ["tab\tname", "all\n", "\u{0085}name", "\u{202e}name"] {
            std::os::unix::fs::symlink(root.join("main"), root.join(name)).unwrap();
        }
        for name in ["ALL", "legacy\\name"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let state = super::super::test_support::build_test_app_state(Vec::new());
        super::super::test_support::accept_runtime_read_cache_for_test(&state).await;
        let client = cached_snapshot(&state).await;
        validate_snapshot(&client).unwrap();
        let main = client
            .profiles
            .iter()
            .find(|profile| profile.name == "main")
            .unwrap();
        assert_eq!(
            main.aliases,
            ["all\n", "tab\tname", "\u{0085}name", "\u{202e}name"]
        );
        for name in ["ALL", "legacy\\name"] {
            let profile = client
                .profiles
                .iter()
                .find(|profile| profile.name == name)
                .unwrap();
            assert!(profile.listed);
            assert!(!crate::cli::runtime_read::dto::profile_component_healthy(
                &profile.health.profile_enumeration
            ));
            assert!(crate::cli::runtime_read::dto::profile_component_healthy(
                &profile.health.metadata
            ));
            assert!(crate::cli::runtime_read::dto::profile_component_healthy(
                &profile.health.profile_data
            ));
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn default_alias_without_listed_profiles_and_parse_fallback_use_existing_config_rules() {
        let home = TempHome::new();
        let outside = tempfile::tempdir().unwrap();
        let root = home.app_dir().join("profiles");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("outside")).unwrap();
        std::fs::write(
            home.app_dir().join("config.toml"),
            "default_profile = \"outside\"\n",
        )
        .unwrap();
        let state = super::super::test_support::build_test_app_state(Vec::new());
        let not_accepted = cached_snapshot(&state).await;
        assert_eq!(not_accepted.default_profile.as_deref(), Some("outside"));
        assert!(!cached_data_healthy(&not_accepted, "outside"));
        super::super::test_support::accept_runtime_read_cache_for_test(&state).await;
        assert!(cached_data_healthy(
            &cached_snapshot(&state).await,
            "outside"
        ));
        Storage::new_unwatched("main").unwrap();
        std::fs::write(home.app_dir().join("state.toml"), "[").unwrap();
        let fallback = cached_snapshot(&state).await;
        assert_eq!(fallback.default_profile.as_deref(), Some("main"));
        assert_eq!(fallback.resolved_default_profile.as_deref(), Some("main"));
    }

    async fn socket_pair() -> (ReadSocket, tokio_tungstenite::WebSocketStream<UnixStream>) {
        let (server, peer) = UnixStream::pair().unwrap();
        let server = tokio_tungstenite::WebSocketStream::from_raw_socket(
            server,
            tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        let peer = tokio_tungstenite::WebSocketStream::from_raw_socket(
            peer,
            tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        (ReadSocket::Unix(server), peer)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn completed_responses_release_capacity_before_peer_close() {
        async fn response(
            peer: &mut tokio_tungstenite::WebSocketStream<UnixStream>,
        ) -> serde_json::Value {
            let mut snapshot = serde_json::Value::Null;
            for expected in ["hello", "snapshot"] {
                let frame = peer.next().await.unwrap().unwrap();
                let value: serde_json::Value =
                    serde_json::from_str(frame.to_text().unwrap()).unwrap();
                assert_eq!(value["kind"], expected);
                snapshot = value;
            }
            snapshot
        }

        let _home = TempHome::new();
        let state =
            super::super::test_support::build_test_app_state_configured(Vec::new(), |state| {
                state.runtime_read_semaphore = Arc::new(tokio::sync::Semaphore::new(1));
            });
        let (first_socket, mut first_peer) = socket_pair().await;
        let first = tokio::spawn(run_read(
            first_socket,
            state.clone(),
            Owner::remote(),
            false,
        ));
        tokio::time::timeout(Duration::from_secs(5), response(&mut first_peer))
            .await
            .unwrap();

        let (second_socket, mut second_peer) = socket_pair().await;
        let second = tokio::spawn(run_read(second_socket, state, Owner::remote(), false));
        let second_response =
            tokio::time::timeout(Duration::from_secs(5), response(&mut second_peer)).await;
        drop(first_peer);
        drop(second_peer);
        tokio::time::timeout(Duration::from_secs(5), async {
            first.await.unwrap();
            second.await.unwrap();
        })
        .await
        .unwrap();
        assert!(
            second_response.is_ok(),
            "an idle close must not block the next read"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cancellation_retains_admission_until_a_blocked_sample_finishes() {
        let _home = TempHome::new();
        let state =
            super::super::test_support::build_test_app_state_configured(Vec::new(), |state| {
                state.runtime_read_semaphore = Arc::new(tokio::sync::Semaphore::new(1));
            });
        let semaphore = state.runtime_read_semaphore.clone();
        let (socket, peer) = socket_pair().await;
        {
            let flight = RUNTIME.flight.lock().unwrap();
            let mut read = Box::pin(run_read(socket, state, Owner::remote(), false));
            let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
            assert!(std::future::Future::poll(read.as_mut(), &mut context).is_pending());
            assert_eq!(semaphore.available_permits(), 0);
            drop(read);
            assert_eq!(semaphore.available_permits(), 0);
            drop(flight);
        }
        let permit = tokio::time::timeout(Duration::from_secs(5), semaphore.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
        drop(peer);
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn retargeting_a_retired_cached_name_to_a_canonical_store_keeps_other_reads_valid() {
        let home = TempHome::new();
        Storage::new_unwatched("main")
            .unwrap()
            .update(|rows, _| {
                *rows = vec![named("canonical", "main")];
                Ok(())
            })
            .unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = home.app_dir().join("profiles");
        std::os::unix::fs::symlink(outside.path(), root.join("outside")).unwrap();
        Storage::open_reference("outside")
            .unwrap()
            .update(|rows, _| {
                *rows = vec![named("retired", "outside")];
                Ok(())
            })
            .unwrap();
        let state = super::super::test_support::build_test_app_state(Vec::new());
        super::super::test_support::accept_runtime_read_cache_for_test(&state).await;
        std::fs::remove_file(root.join("outside")).unwrap();
        std::os::unix::fs::symlink(root.join("main"), root.join("outside")).unwrap();
        let snapshot = cached_snapshot(&state).await;
        assert_eq!(native_session_ids(&state).await, ["canonical"]);
        assert!(cached_data_healthy(&snapshot, "main"));
        assert!(!cached_data_healthy(&snapshot, "outside"));
        assert_eq!(
            snapshot
                .sessions
                .iter()
                .map(|row| (row.id.as_str(), row.profile.as_str()))
                .collect::<Vec<_>>(),
            [("canonical", "main"), ("retired", "outside")]
        );
        assert!(snapshot
            .profiles
            .iter()
            .find(|profile| profile.name == "main")
            .unwrap()
            .aliases
            .is_empty());
        state
            .mutation_epoch
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let (loaded, metadata) = super::super::reload::load_profiles_for_reload(&state.file_watch);
        super::super::reload::reload_state_instances_from_disk(
            &state,
            loaded,
            vec![],
            super::super::state::StatusSource::DiskOnly,
            0,
            metadata,
            Default::default(),
        )
        .await;
        let rejected = cached_snapshot(&state).await;
        assert_eq!(
            rejected
                .sessions
                .iter()
                .map(|row| (row.id.as_str(), row.profile.as_str()))
                .collect::<Vec<_>>(),
            [("canonical", "main"), ("retired", "outside")]
        );
        assert!(!cached_data_healthy(&rejected, "outside"));
        super::super::test_support::accept_runtime_read_cache_for_test(&state).await;
        let accepted = cached_snapshot(&state).await;
        assert_eq!(
            accepted
                .sessions
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["canonical"]
        );
        assert_eq!(accepted.profiles[0].aliases, ["outside"]);
        assert!(cached_data_healthy(&accepted, "main"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn retargeted_alias_only_copies_reserve_old_keys_without_mixing_physical_stores() {
        use super::super::reload::reload_state_instances_from_disk;
        use super::super::state::StatusSource;
        for surviving_alias in [true, false] {
            let home = TempHome::new();
            Storage::new_unwatched("main")
                .unwrap()
                .update(|rows, _| {
                    *rows = vec![named("canonical", "main")];
                    Ok(())
                })
                .unwrap();
            let a = tempfile::tempdir().unwrap();
            let b = tempfile::tempdir().unwrap();
            let root = home.app_dir().join("profiles");
            for (name, directory, title, status) in [
                ("outside-a", a.path(), "store-a", Status::Waiting),
                ("outside-z", b.path(), "store-b", Status::Running),
            ] {
                std::os::unix::fs::symlink(directory, root.join(name)).unwrap();
                Storage::open_reference(name)
                    .unwrap()
                    .update(|rows, _| {
                        let mut row = named("copy", name);
                        row.title = title.into();
                        row.command = format!("printf {title}");
                        row.status = status;
                        *rows = vec![row];
                        Ok(())
                    })
                    .unwrap();
            }
            let state = super::super::test_support::build_test_app_state(Vec::new());
            super::super::test_support::accept_runtime_read_cache_for_test(&state).await;
            std::fs::remove_file(root.join("outside-a")).unwrap();
            std::os::unix::fs::symlink(b.path(), root.join("outside-a")).unwrap();
            assert!(!cached_data_healthy(
                &cached_snapshot(&state).await,
                "outside-a"
            ));
            std::fs::remove_file(root.join("outside-a")).unwrap();
            std::os::unix::fs::symlink(a.path(), root.join("outside-a")).unwrap();
            let restored = cached_snapshot(&state).await;
            assert!(cached_data_healthy(&restored, "outside-a"));
            assert_eq!(
                restored
                    .sessions
                    .iter()
                    .find(|row| row.profile == "outside-a")
                    .unwrap()
                    .title,
                "store-a"
            );
            std::fs::remove_file(root.join("outside-a")).unwrap();
            std::os::unix::fs::symlink(b.path(), root.join("outside-a")).unwrap();
            if !surviving_alias {
                std::fs::remove_file(root.join("outside-z")).unwrap();
            }
            for rejected in [false, true] {
                if rejected {
                    state
                        .mutation_epoch
                        .store(1, std::sync::atomic::Ordering::SeqCst);
                    async {
                        let (loaded, metadata) =
                            crate::server::reload::load_profiles_for_reload(&state.file_watch);
                        reload_state_instances_from_disk(
                            &state,
                            loaded,
                            vec![],
                            StatusSource::DiskOnly,
                            0,
                            metadata,
                            Default::default(),
                        )
                        .await
                    }
                    .await;
                }
                let snapshot = cached_snapshot(&state).await;
                assert_eq!(
                    snapshot
                        .sessions
                        .iter()
                        .map(|row| (row.id.as_str(), row.profile.as_str(), row.title.as_str()))
                        .collect::<Vec<_>>(),
                    [
                        ("canonical", "main", "Session canonical"),
                        ("copy", "outside-a", "store-a"),
                        ("copy", "outside-z", "store-b")
                    ]
                );
                assert!(cached_data_healthy(&snapshot, "main"));
                assert!(!cached_data_healthy(&snapshot, "outside-a"));
                assert_eq!(cached_data_healthy(&snapshot, "outside-z"), surviving_alias);
                let b_row = snapshot
                    .sessions
                    .iter()
                    .find(|row| row.profile == "outside-z")
                    .unwrap();
                assert_eq!(b_row.command, "printf store-b");
                assert_eq!(
                    b_row.status,
                    crate::cli::runtime_read::dto::WireStatus::Running
                );
                assert_eq!(native_session_ids(&state).await, ["canonical"]);
            }
            super::super::test_support::accept_runtime_read_cache_for_test(&state).await;
            let accepted = cached_snapshot(&state).await;
            assert_eq!(
                accepted
                    .profiles
                    .iter()
                    .map(|profile| profile.name.as_str())
                    .collect::<Vec<_>>(),
                ["main", "outside-a"]
            );
            let b_row = accepted
                .sessions
                .iter()
                .find(|row| row.profile == "outside-a")
                .unwrap();
            assert_eq!(b_row.title, "store-b");
            assert_eq!(b_row.command, "printf store-b");
            assert!(cached_data_healthy(&accepted, "outside-a"));
            assert_eq!(
                accepted
                    .sessions
                    .iter()
                    .map(|row| row.id.as_str())
                    .collect::<Vec<_>>(),
                ["canonical", "copy"]
            );
        }
    }
}
