//! Read-only runtime read endpoint: `GET /api/runtime/ws`.
//!
//! One connection carries exactly two application frames, a Hello handshake
//! and a Snapshot, and the server then sends a normal close. The handler only
//! reads session, profile, group and project state; it never mutates it and
//! never consults a client-local filesystem.
//!
//! Authentication for the HTTP route is the router's existing credential gate, so
//! a caller without a valid credential never reaches the upgrade. The same two
//! frames are also served over the daemon's own UNIX socket, where the peer is
//! the same uid the daemon runs as, so that transport declares the local owner.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use futures_util::{SinkExt, StreamExt};

use serde::Serialize;
use tokio_tungstenite::tungstenite;

use super::AppState;
use crate::session::{GroupTree, Instance, Storage};

/// Wire generation 4 adds physical profile aliases and listed membership.
pub(crate) const PROTOCOL_VERSION: u16 = 4;
/// A stalled reader must not hold a connection slot, or a full disk rescan's
/// worth of work, open indefinitely. Both transports spend this one budget,
/// each for the whole connection from accept to close rather than per stage,
/// and it matches the client's single read budget, so a peer that
/// authenticates and then says nothing is bounded identically either way.
pub(crate) const CONNECTION_BUDGET: Duration = Duration::from_secs(15);
/// The message ceiling both transports hand tungstenite, and the ceiling this
/// module refuses to send past. tungstenite's message limit is larger than the
/// frame limit it was already accepting here, so the frame ceiling already
/// matched `APPLICATION_LIMIT` on both routes while the WS route was accepting
/// more than the UDS route did. One constant, so the two routes cannot drift
/// apart again.
pub(crate) const MESSAGE_LIMIT: usize = crate::cli::runtime_read::APPLICATION_LIMIT;
/// The trusted namespace this build publishes for itself, and the name the
/// UDS marker files carry.
pub(crate) const NAMESPACE: &str = if cfg!(debug_assertions) {
    "debug:agent-of-empires-dev"
} else {
    "release:agent-of-empires"
};

/// How many runtime-read connections are admitted at once. The sample itself
/// is single-flight, so this does not raise throughput; it bounds how many
/// connections can be alive holding a slot and a cloned row set while they
/// wait for that one sample.
pub(crate) const RUNTIME_READ_CONCURRENCY: usize = 8;

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
    // The whole read is one budget, as on the local socket: the two frames and
    // the close. The response that admits the upgrade is written by the
    // extractor, so the bound has to wrap the upgraded task itself: a peer
    // that authenticates, upgrades and then stops reading must not hold this
    // task, its connection slot and its sample for the life of the daemon.
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
    let Ok(admitted) = state.runtime_read_semaphore.clone().acquire_owned().await else {
        return;
    };
    let runtime = &RUNTIME;
    let (instances, cache, observed_at) = {
        let instances = state.instances.read().await;
        let cache = state
            .runtime_read_cache
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rows = Vec::with_capacity(instances.len() + cache.alias_only_instances.len());
        rows.extend(instances.iter().cloned());
        rows.extend(cache.alias_only_instances.iter().cloned());
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
        let sample = build_snapshot(runtime, &instances, owner, observed_at, &cache);
        (sample, admitted)
    })
    .await;

    let (snapshot, _admitted) = match sampled {
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
    for frame in frames {
        match frame {
            Ok(encoded) => {
                // Measured on what is about to go out, because that is the
                // `String` the client's `max_message_size` counts. A frame
                // past it is refused by the client after this daemon has done
                // the work to build it, and the only honest alternative to
                // sending it whole is a partial listing.
                if encoded.len() > MESSAGE_LIMIT {
                    tracing::warn!(
                        target: "runtime.ws",
                        bytes = encoded.len(),
                        "runtime frame is over the client's message ceiling; closing without sending"
                    );
                    socket.close().await;
                    return;
                }
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
// Contract Pack recording
// ---------------------------------------------------------------------------

/// A recorded exchange, with everything the recorder had to pin spelled out.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub struct RecordedExchange {
    /// The Hello frame, as the producer's own serialiser wrote it.
    pub hello: Vec<u8>,
    /// The Snapshot frame, likewise.
    pub snapshot: Vec<u8>,
}

/// Which transport the recorded exchange pretends to have arrived over. The
/// local one declares the uid the peer would have; the remote one declares no
/// uid at all, which is the whole difference between the two Hello frames.
#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
#[derive(Debug, Clone, Copy)]
pub enum RecordedOwner {
    Local { uid: u32 },
    Remote,
}

/// Everything a recording pins so the same store always yields the same bytes.
///
/// A transcript is a frozen artefact, so the two things the daemon mints per
/// process or per instant, the three identity UUIDs and the freshness clock,
/// are the only inputs a recorder may not take from the environment.
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
        instances,
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
/// that count plus one. `latched` is the overflow latch: once set no counter
/// moves again and the public projection stays unavailable.
#[derive(Default)]
struct Sampler {
    successes: u64,
    latched: bool,
}

struct RuntimeState {
    identity: RuntimeIdentity,
    sampler: Mutex<Sampler>,
    /// The single sample in flight. A `std` mutex because the guard belongs to
    /// the blocking sample rather than to the connection awaiting it, so a
    /// cancelled read cannot release it while its work still runs.
    flight: Mutex<()>,
    /// The clock a read observes at. A seam and nothing more: `None` is the
    /// wall clock, and a recording harness pins an instant so a transcript
    /// is reproducible. The daemon itself never pins one.
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

/// The identities every Hello this process emits carries. The UDS publisher
/// writes the same three values into its marker files, so a client can prove
/// the daemon it reached is the one that published the socket.
pub(crate) fn identity() -> &'static RuntimeIdentity {
    &RUNTIME.identity
}

/// The cursor a latched sampler publishes. Both counters are saturated, so
/// there is no next value and the cursor freezes at the last one it could
/// name. It cannot freeze at zero, which is the value the client refuses,
/// because that would turn a freshness the daemon cannot report into a
/// snapshot the client rejects outright, and `freshness_unavailable` into
/// `schema_invalid`.
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
            observed_at: crate::cli::list::display_timestamp(observed_at),
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
    groups: Result<Vec<crate::session::Group>, ()>,
}

fn build_snapshot(
    runtime: &RuntimeState,
    instances: &[Instance],
    owner: Owner,
    observed_at: DateTime<Utc>,
    cache: &super::reload::RuntimeReadCache,
) -> Sampled {
    // `local_owner` mirrors the declared owner so the two can never disagree.
    let local_owner = owner.is_local();
    let identity = &runtime.identity;
    let (freshness, cursor_revision) = publish_freshness(runtime, observed_at);

    // Rows are projected before the disk reads so the session projection itself
    // never runs behind a filesystem call.
    let mut sessions: Vec<SessionRead> = instances.iter().map(SessionRead::from_instance).collect();

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
            let projects = if accepted {
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
            } else {
                Err(())
            };
            let groups = if accepted {
                Storage::open_unwatched(name)
                    .and_then(|storage| storage.load_groups_readonly())
                    .map_err(|_| ())
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
    let global_metadata_healthy = global_projects.is_some();
    let mut global_projects = global_projects.unwrap_or_default();
    drop_unusable_projects(&mut global_projects);

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
    reconcile_legacy_rows(&mut sessions, &listed_names);

    let mut profile_reads = Vec::with_capacity(inventory.len());
    let mut profile_health = BTreeMap::new();
    for (name, entry) in disk {
        let projects_healthy = entry.projects.is_ok();
        let groups_healthy = entry.groups.is_ok();
        let scoped: Vec<&SessionRead> = sessions.iter().filter(|row| row.profile == name).collect();
        let mut projects = entry.projects.unwrap_or_default();
        drop_unusable_projects(&mut projects);
        // Referential integrity: every session's project path is a member of its
        // own profile's project list.
        add_session_projects(&mut projects, &scoped, &global_projects);
        // Registry order, not a canonical sort: that is the order a local
        // `aoe project list` prints, and the wire carries the presentation.
        let mut identities: HashSet<(String, String)> = HashSet::new();
        projects.retain(|project| identities.insert((project.name.clone(), project.path.clone())));
        let stored_groups = entry.groups.unwrap_or_default();
        let mut tree = GroupTree::new_with_group_paths(
            scoped.iter().map(|row| row.group_path.as_str()),
            &stored_groups,
        );
        drop_unusable_groups(&mut tree);
        let groups = group_reads(&tree);
        let health = ProfileHealth {
            profile_enumeration: component(groups_healthy, HealthCode::ProfileEnumeration),
            metadata: component(projects_healthy, HealthCode::Metadata),
            profile_data: component(
                !cache.health.enumeration_failed
                    && inventory[&name].as_ref().is_some_and(|profile| {
                        cache.inventory.iter().any(|accepted| {
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

/// Drop a project row the client could not admit.
///
/// The client's contract requires an absolute path, because a relative one is
/// not a path any session row can reference. A hand-edited `projects.json` is
/// the one way a row gets one, and refusing a whole snapshot over a single row
/// would fail every read command on every profile: so the row goes, in the
/// same spirit as [`reconcile_legacy_rows`], and the client's own
/// `valid_stored_project_path` stays fail-closed.
fn drop_unusable_projects(projects: &mut Vec<ProjectRead>) {
    projects
        .retain(|project| crate::cli::runtime_read::dto::valid_stored_project_path(&project.path));
}

/// Drop a group the client could not admit, and everything under it.
///
/// The tree is pruned before it is projected: a row dropped only from the
/// output would stay in its parent's `children`, and the client refuses a
/// snapshot over a child whose path is not in the profile's own set just as
/// hard as it refuses the row itself.
fn drop_unusable_groups(tree: &mut GroupTree) {
    for group in tree.get_all_groups() {
        if !crate::cli::runtime_read::dto::valid_group_path(&group.path) {
            tree.delete_group(&group.path);
        }
    }
}

/// Preserve scoped orphans, sever cycles and invalid group paths.
fn reconcile_legacy_rows(sessions: &mut [SessionRead], listed: &HashSet<&str>) {
    let index: HashMap<(&str, &str), usize> = sessions
        .iter()
        .enumerate()
        .map(|(position, row)| ((row.profile.as_str(), row.id.as_str()), position))
        .collect();
    let listed_ids: HashSet<&str> = sessions
        .iter()
        .filter(|row| listed.contains(row.profile.as_str()))
        .map(|row| row.id.as_str())
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
    let mut severed = cycle_entries(&mut parents);
    for (position, row) in sessions.iter().enumerate() {
        if listed.contains(row.profile.as_str())
            && row.parent_session_id.as_deref().is_some_and(|parent| {
                !index.contains_key(&(row.profile.as_str(), parent)) && listed_ids.contains(parent)
            })
        {
            severed.insert(position);
        }
    }
    for (position, row) in sessions.iter_mut().enumerate() {
        if severed.contains(&position) {
            row.parent_session_id = None;
        }
        if !row.group_path.is_empty()
            && !crate::cli::runtime_read::dto::valid_group_path(&row.group_path)
        {
            row.group_path.clear();
        }
    }
}

/// Sever one edge of each scoped cycle in stored row order.
fn cycle_entries(parents: &mut HashMap<usize, usize>) -> HashSet<usize> {
    let mut entries = HashSet::new();
    for start in parents.keys().copied().collect::<BTreeSet<_>>() {
        let mut walked = HashSet::new();
        let mut cursor = Some(start);
        while let Some(row) = cursor {
            if !walked.insert(row) {
                // Subsequent walks see the severed edge.
                entries.insert(row);
                parents.remove(&row);
                break;
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
        if !crate::cli::runtime_read::dto::valid_stored_project_path(&path)
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
    profiles: Vec<ProfileHello>,
    status_freshness: StatusFreshness,
}

#[derive(Serialize)]
struct SnapshotData {
    namespace: String,
    cursor: Cursor,
    health: SnapshotHealth,
    default_profile: Option<String>,
    /// The name the daemon resolved as the default, published whatever
    /// `default_profile` says and absent only when resolving would have
    /// bootstrapped a profile, so a client can name the profile a stale
    /// default points at.
    resolved_default_profile: Option<String>,
    profiles: Vec<ProfileRead>,
    sessions: Vec<SessionRead>,
    global_projects: Vec<ProjectRead>,
    status_freshness: StatusFreshness,
}

/// What the Hello says about one profile: its name, and whether this daemon
/// could read it at all. The inventory is not repeated, because the Snapshot
/// that follows carries every group and project of every profile and the
/// client validates each of them there.
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
    /// Opaque equality metadata: the project's identity as the *daemon's*
    /// filesystem resolves it. The client merges on this and never on a path,
    /// because a path it resolved would be a path on the wrong machine.
    merge_key: String,
    scope: ProjectScope,
    default_base_branch: Option<String>,
    /// True for a row the registry holds, false for one synthesized so a
    /// session's project path resolves. A `aoe project list` prints only the
    /// registered ones.
    registered: bool,
}

#[derive(Debug, Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProjectScope {
    Global,
    Profile,
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
    has_worktree_info: bool,
    has_managed_worktree: bool,
    worktree: Option<WorktreeRead>,
    workspace_repos: Vec<WorkspaceRepo>,
}

impl SessionRead {
    fn from_instance(inst: &Instance) -> Self {
        // Stored order, the way the local projection emits it: sorting here
        // would make `aoe list --json` order the array by the transport. The
        // client's rule is identity, which the store already holds.
        let workspace_repos: Vec<WorkspaceRepo> = inst
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
            created_at: crate::cli::list::display_timestamp(inst.created_at),
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
            has_worktree_info: inst.worktree_info.is_some(),
            has_managed_worktree: inst
                .worktree_info
                .as_ref()
                .is_some_and(|worktree| worktree.managed_by_aoe),
            worktree: inst.worktree_info.as_ref().map(|worktree| WorktreeRead {
                branch: worktree.branch.clone(),
                main_repo_path: worktree.main_repo_path.clone(),
                managed_by_aoe: worktree.managed_by_aoe,
                base_branch: worktree.base_branch.clone(),
            }),
            workspace_repos,
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

fn timestamp(value: Option<DateTime<Utc>>) -> Option<String> {
    value.map(crate::cli::list::display_timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::runtime_read::dto::{
        parse_hello, parse_snapshot, validate_cross_message, validate_hello, validate_snapshot,
    };
    use crate::session::{Group, Status, WorkspaceInfo, WorkspaceRepo as StoredRepo, WorktreeInfo};

    /// One row's `required` list from the published document beside the
    /// fixtures rather than from anything this module knows. `definition` of
    /// `None` is the frame's own list, which is the one the top-level row
    /// answers to.
    #[cfg(debug_assertions)]
    fn schema_required(document: &str, definition: Option<&str>) -> BTreeSet<String> {
        let parsed = published_schema(document);
        let node = match definition {
            Some(definition) => &parsed["$defs"][definition],
            None => &parsed,
        };
        node["required"]
            .as_array()
            .expect("every wire row requires its fields")
            .iter()
            .map(|name| name.as_str().expect("a field name").to_string())
            .collect()
    }

    /// Every field a `$defs` entry declares, required or not. A row that is a
    /// discriminated union requires only its discriminant.
    #[cfg(debug_assertions)]
    fn schema_properties(document: &str, definition: Option<&str>) -> BTreeSet<String> {
        let parsed = published_schema(document);
        let node = match definition {
            Some(definition) => &parsed["$defs"][definition],
            None => &parsed,
        };
        node["properties"]
            .as_object()
            .expect("every wire row declares its fields")
            .keys()
            .map(|name| name.to_string())
            .collect()
    }

    /// Every `$defs` entry that names an object row the frame reaches.
    ///
    /// Found by following `$ref`s out of the document rather than by matching
    /// a name suffix. A suffix filter silently drops every object row whose
    /// name does not end in `_read` or `_repo`, and the dropped ones are not
    /// incidental: the cursor, the freshness row and the health rows are
    /// required on every frame, so a field added inside one of them was emitted
    /// by the producer, absent from the schema, and let past by a gate whose
    /// own comment says that is the shape it exists to catch.
    #[cfg(debug_assertions)]
    fn row_definitions(document: &str) -> BTreeSet<String> {
        use crate::cli::runtime_read::pack::{HELLO_SCHEMA, SNAPSHOT_SCHEMA};
        let published = published_schema(document);
        let defs = published["$defs"]
            .as_object()
            .expect("$defs is an object")
            .clone();
        let mut reached: BTreeSet<String> = BTreeSet::new();
        let mut pending = vec![published.clone()];
        while let Some(node) = pending.pop() {
            let Some(reference) = node.get("$ref").and_then(|reference| reference.as_str()) else {
                match node {
                    // `$defs` is the dictionary rows are pulled out of, so
                    // walking into it would reach every definition the schema
                    // defines rather than every one this frame uses.
                    serde_json::Value::Object(fields) => pending.extend(
                        fields
                            .into_iter()
                            .filter(|(key, _)| key != "$defs")
                            .map(|(_, value)| value),
                    ),
                    serde_json::Value::Array(items) => pending.extend(items),
                    _ => {}
                }
                continue;
            };
            let name = reference
                .rsplit('/')
                .next()
                .expect("a $ref names a definition")
                .to_string();
            if !reached.insert(name.clone()) {
                continue;
            }
            pending.push(
                defs.get(&name)
                    .unwrap_or_else(|| panic!("{document} references an undefined {name}"))
                    .clone(),
            );
        }
        // A row is a shape. A scalar format and an enum have no fields of their
        // own to hold the producer to, and their parents require them by name.
        let rows: BTreeSet<String> = reached
            .into_iter()
            .filter(|name| defs[name]["type"] == "object")
            .collect();
        assert_eq!(
            document == SNAPSHOT_SCHEMA,
            rows.contains("session_read"),
            "the documents are not the ones this gate reads"
        );
        assert_eq!(
            document == HELLO_SCHEMA,
            rows.contains("profile_hello"),
            "the documents are not the ones this gate reads"
        );
        rows
    }

    #[cfg(debug_assertions)]
    fn published_schema(document: &str) -> serde_json::Value {
        let text =
            std::fs::read_to_string(crate::cli::runtime_read::pack::pack_root().join(document))
                .expect("the published schema");
        serde_json::from_str(&text).expect("it is JSON")
    }

    /// Every field the producer serialises is one the published schema
    /// requires, in both frames and in every row either one names.
    ///
    /// The pack holds recorded transcripts to the schemas, which proves the
    /// two agree with each other and says nothing about either against the
    /// producer. A field the producer grew and the schema and every recorded
    /// frame left out is the shape that passes: the one this gate is for, and
    /// the one no version number would catch. Checking three of the rows left
    /// the rest unchecked, and a field added inside a worktree or a workspace
    /// repo passed while the same field added to a session row did not.
    ///
    /// The last assertion is what makes this a gate rather than a list: a
    /// definition the schemas gain and this table does not account for is a
    /// row nothing holds the producer to, so adding one fails here until the
    /// sample grows a row of that shape.
    #[test]
    #[serial_test::serial]
    // The published schemas it reads live behind the pack module's debug gate,
    // so this gate is where a release test build would otherwise name a module
    // that is not compiled.
    #[cfg(debug_assertions)]
    fn every_field_the_producer_emits_is_required_by_the_published_schema() {
        use crate::cli::runtime_read::pack::{HELLO_SCHEMA, SNAPSHOT_SCHEMA};
        let _home = TempHome::new();
        let mut session = named("a", "main");
        session.group_path = "team/sub".into();
        // A row the gate cannot see is a row nothing holds the producer to,
        // so the sample carries one of every shape the two schemas name.
        session.worktree_info = Some(WorktreeInfo {
            branch: "feature".into(),
            main_repo_path: "/repo".into(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: Some("main".into()),
        });
        session.workspace_info = Some(WorkspaceInfo {
            branch: "feature".into(),
            workspace_dir: "/repo/.aoe/workspace".into(),
            created_at: Utc::now(),
            cleanup_on_delete: true,
            repos: vec![repo("beta", "/srv/beta")],
        });
        let sampled = build_snapshot(
            &RuntimeState::new(),
            &[session],
            Owner::remote(),
            Utc::now(),
            &crate::server::reload::RuntimeReadCache::accepted_inventory(),
        );
        let hello: serde_json::Value =
            serde_json::from_slice(&hello_frame(&sampled)).expect("the Hello is JSON");
        let snapshot: serde_json::Value =
            serde_json::from_slice(&snapshot_frame(&sampled)).expect("the Snapshot is JSON");
        let hello = &hello["data"];
        let snapshot = &snapshot["data"];
        let profile = &snapshot["profiles"][0];
        let row = &snapshot["sessions"][0];
        let emitted: [(&str, Option<&str>, &serde_json::Value); 19] = [
            (HELLO_SCHEMA, None, hello),
            (HELLO_SCHEMA, Some("owner"), &hello["owner"]),
            (HELLO_SCHEMA, Some("profile_hello"), &hello["profiles"][0]),
            (SNAPSHOT_SCHEMA, None, snapshot),
            (SNAPSHOT_SCHEMA, Some("session_read"), row),
            (SNAPSHOT_SCHEMA, Some("profile_read"), profile),
            (
                SNAPSHOT_SCHEMA,
                Some("project_read"),
                &profile["projects"][0],
            ),
            (SNAPSHOT_SCHEMA, Some("group_read"), &profile["groups"][0]),
            (SNAPSHOT_SCHEMA, Some("worktree_read"), &row["worktree"]),
            (
                SNAPSHOT_SCHEMA,
                Some("workspace_repo"),
                &row["workspace_repos"][0],
            ),
            // The rows the frame reaches that no `_read` or `_repo` name marks:
            // the cursor and the freshness row are required on every frame, and
            // the health shapes carry the components the answer rests on.
            (SNAPSHOT_SCHEMA, Some("cursor"), &snapshot["cursor"]),
            (
                SNAPSHOT_SCHEMA,
                Some("snapshot_health"),
                &snapshot["health"],
            ),
            (
                SNAPSHOT_SCHEMA,
                Some("component_health"),
                &snapshot["health"]["global_enumeration"],
            ),
            (SNAPSHOT_SCHEMA, Some("profile_health"), &profile["health"]),
            (
                SNAPSHOT_SCHEMA,
                Some("status_freshness"),
                &snapshot["status_freshness"],
            ),
            (HELLO_SCHEMA, Some("aggregate_health"), &hello["health"]),
            (
                HELLO_SCHEMA,
                Some("profile_health"),
                &hello["profiles"][0]["health"],
            ),
            (
                HELLO_SCHEMA,
                Some("component_health"),
                &hello["profiles"][0]["health"]["profile_enumeration"],
            ),
            (
                HELLO_SCHEMA,
                Some("status_freshness"),
                &hello["status_freshness"],
            ),
        ];
        let mut checked: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        for (document, definition, value) in emitted {
            let label = definition.unwrap_or("the frame itself");
            let object = value
                .as_object()
                .unwrap_or_else(|| panic!("{label} is an object"));
            let required = schema_required(document, definition);
            let declared = schema_properties(document, definition);
            // Both directions, and neither is the other's. A field the producer
            // grew that the schema left out is the shape this gate exists for,
            // and `additionalProperties: false` agrees with it. A field the
            // schema demands that the producer does not send would reach a
            // client as `schema_invalid` instead.
            //
            // The test is against `properties` rather than `required` because a
            // health row is a discriminated union: `kind` alone is required and
            // `code` belongs to the degraded arm only, so demanding that every
            // emitted field be required rejects the shape rather than a
            // regression. Every row in the pack but those has no optional
            // field at all, so on those the two tests are the same test.
            let unknown: Vec<&String> = object
                .keys()
                .filter(|key| !declared.contains(*key))
                .collect();
            assert!(
                unknown.is_empty(),
                "{label} emits fields the published schema does not declare: {unknown:?}"
            );
            let unsent: Vec<&String> = required
                .iter()
                .filter(|key| !object.contains_key(*key))
                .collect();
            assert!(
                unsent.is_empty(),
                "{label} leaves fields the published schema requires unsent: {unsent:?}"
            );
            if let Some(definition) = definition {
                checked
                    .entry(document)
                    .or_default()
                    .insert(definition.to_string());
            }
        }
        for document in [HELLO_SCHEMA, SNAPSHOT_SCHEMA] {
            assert_eq!(
                row_definitions(document),
                checked.get(document).cloned().unwrap_or_default(),
                "{document} names a row this gate does not check"
            );
        }
    }

    /// The wire carries a session's repos in the order the workspace stored
    /// them, because that is the order the local projection emits them in and
    /// `aoe list --json` prints the array. Sorting here would make the array's
    /// order depend on whether a daemon is publishing.
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
        rows.extend(instances.iter().cloned());
        rows.extend(cache.alias_only_instances.iter().cloned());
        let sampled = build_snapshot(
            &RuntimeState::new(),
            &rows,
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

    #[tokio::test]
    #[serial_test::serial]
    async fn session_load_failures_recover_only_with_an_accepted_reload() {
        use crate::server::{
            reload::{load_all_instances, reload_state_instances_from_disk},
            state::StatusSource,
            test_support,
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
            reload_state_instances_from_disk(
                &state,
                load_all_instances(&state.file_watch),
                vec![],
                mode,
                0,
            )
            .await;
            let path = home.app_dir().join("profiles/main/sessions.json");
            let original = std::fs::read(&path).unwrap();
            std::fs::write(&path, "not json").unwrap();
            let failed = load_all_instances(&state.file_watch);
            state.mutation_epoch.store(1, Ordering::SeqCst);
            reload_state_instances_from_disk(&state, failed, vec![], mode, 0).await;
            let snapshot = cached_snapshot(&state).await;
            assert!(
                cached_data_healthy(&snapshot, "main"),
                "a stale failed load cannot degrade current rows"
            );
            assert!(snapshot.sessions.iter().any(|row| row.id == "bad"));
            reload_state_instances_from_disk(
                &state,
                load_all_instances(&state.file_watch),
                vec![],
                mode,
                1,
            )
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
            let recovered = load_all_instances(&state.file_watch);
            state.mutation_epoch.store(2, Ordering::SeqCst);
            reload_state_instances_from_disk(&state, recovered, vec![], mode, 1).await;
            assert!(
                !cached_data_healthy(&cached_snapshot(&state).await, "main"),
                "a stale healthy load cannot clear a newer failure"
            );
            reload_state_instances_from_disk(
                &state,
                load_all_instances(&state.file_watch),
                vec![],
                mode,
                2,
            )
            .await;
            let snapshot = cached_snapshot(&state).await;
            assert!(cached_data_healthy(&snapshot, "main"));
            assert!(snapshot.sessions.iter().any(|row| row.id == "bad"));
            std::fs::write(&path, "[]").unwrap();
            reload_state_instances_from_disk(
                &state,
                load_all_instances(&state.file_watch),
                vec![],
                mode,
                2,
            )
            .await;
            let snapshot = cached_snapshot(&state).await;
            assert!(cached_data_healthy(&snapshot, "main"));
            assert!(!snapshot.sessions.iter().any(|row| row.profile == "main"));
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn session_enumeration_failure_does_not_block_recovered_inventory() {
        use crate::server::{
            reload::{load_all_instances, reload_state_instances_from_disk},
            state::StatusSource,
            test_support,
        };
        let home = TempHome::new();
        let profiles = home.app_dir().join("profiles");
        std::fs::create_dir_all(home.app_dir()).unwrap();
        std::fs::write(&profiles, "not a directory").unwrap();
        let state = test_support::build_test_app_state(Vec::new());
        let failed = load_all_instances(&state.file_watch);
        let failed_health = failed.cache.health.clone();
        reload_state_instances_from_disk(&state, failed, vec![], StatusSource::DiskOnly, 0).await;
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
        reload_state_instances_from_disk(
            &state,
            load_all_instances(&state.file_watch),
            vec![],
            StatusSource::DiskOnly,
            0,
        )
        .await;
        assert!(cached_data_healthy(&cached_snapshot(&state).await, "main"));
        std::fs::remove_dir_all(profiles.join("main")).unwrap();
        let failed = crate::server::reload::LoadedInstances {
            instances: vec![],
            cache: super::super::reload::RuntimeReadCache {
                health: failed_health,
                ..Default::default()
            },
        };
        reload_state_instances_from_disk(&state, failed, vec![], StatusSource::DiskOnly, 0).await;
        let snapshot = cached_snapshot(&state).await;
        assert!(crate::cli::runtime_read::dto::component_healthy(
            &snapshot.health.global_enumeration
        ));
        assert!(snapshot.profiles.is_empty());
        assert!(snapshot.sessions.is_empty());
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
        let instances = vec![instance("a", "main"), instance("b", "broken")];
        let sampled = build_snapshot(
            &RuntimeState::new(),
            &instances,
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

    /// A read must not write. The projection path may read a registry with a
    /// row that cannot deserialise, but it must not materialise a quarantine
    /// sidecar beside a store it only reads.
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
            &[],
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

    /// Deleting the configured default leaves the config naming a profile that
    /// is gone. The local path refuses (`resolve_existing_profile`), so the
    /// served path publishes no default at all rather than marking some other
    /// profile `(default)` and serving its sessions.
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
            &[instance("a", "zeta")],
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

    /// Points the app dir at an empty temporary XDG base for the duration of one
    /// test, so a snapshot is assembled from fixtures rather than the developer's
    /// real profiles and project registries. The environment guard holds the
    /// process-wide lock, and drops before the directory it points at.
    struct TempHome {
        _env: crate::server::test_support::RuntimeEnvGuard,
        _dir: tempfile::TempDir,
    }

    impl TempHome {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("temp home");
            let env = crate::server::test_support::RuntimeEnvGuard::set(dir.path());
            Self {
                _env: env,
                _dir: dir,
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

    /// The two frames the server emits, decoded by the client's real decoders and
    /// validators: any field-name, member-order or invariant drift fails here.
    #[test]
    #[serial_test::serial]
    fn emitted_frames_satisfy_the_client_wire_contract() {
        let _home = TempHome::new();
        let instances = vec![instance("a", "main"), instance("b", "main")];
        let sampled = build_snapshot(
            &RuntimeState::new(),
            &instances,
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
            &[instance("a", "main")],
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
            &[],
            Owner::remote(),
            Utc::now(),
            &crate::server::reload::RuntimeReadCache::accepted_inventory(),
        );
        let second = build_snapshot(
            &runtime,
            &[],
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

    /// Once the counter would wrap, the projection latches to unavailable and no
    /// counter moves again. What matters is the consequence on the client: the
    /// sample it produces has to survive the real decoder, or the read is
    /// refused as `schema_invalid` and `freshness_unavailable`, which is in
    /// the Contract Pack's code set and in `EMITTABLE_CODES`, can never be
    /// emitted at all.
    #[test]
    #[serial_test::serial]
    fn a_latched_sampler_still_produces_a_snapshot_the_client_accepts() {
        let _home = TempHome::new();
        let runtime = RuntimeState::new();
        runtime.sampler.lock().unwrap().successes = u64::MAX;
        let sampled = build_snapshot(
            &runtime,
            &[],
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

    /// A stored row the read cannot fix is kept as it stands: a trailing
    /// separator is the spelling the store holds and stays it, so the row is
    /// still the identifier a `session show` can be given, and an orphan parent
    /// is left pointing at a row that is not there: the state `rm --purge`
    /// leaves behind, and the one the local path prints. The client then
    /// accepts the snapshot unchanged.
    #[test]
    #[serial_test::serial]
    fn a_legacy_row_is_reconciled_and_the_client_still_accepts_the_snapshot() {
        let _home = TempHome::new();
        let mut orphan = named("orphan", "main");
        orphan.parent_session_id = Some("deleted".into());
        let mut trailing = named("trailing", "main");
        trailing.project_path = "/repo/".into();
        let mut grouped = named("grouped", "main");
        grouped.group_path = "team/".into();
        let instances = vec![named("a", "main"), orphan, trailing, grouped];

        let sampled = build_snapshot(
            &RuntimeState::new(),
            &instances,
            Owner::remote(),
            Utc::now(),
            &crate::server::reload::RuntimeReadCache::accepted_inventory(),
        );

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
                ("orphan", Some("deleted"), "/repo"),
                ("trailing", None, "/repo/"),
                ("grouped", None, "/repo"),
            ]
        );
    }

    /// A parent that names a row of another profile, and a cycle, are the two
    /// relations the client refuses. Both are severed at the row that closes
    /// them, and both choices are the same on every run. A parent that names no
    /// row at all is neither: it is persisted state, and it is kept.
    #[test]
    fn cross_profile_parents_and_cycles_are_severed_at_the_closing_row() {
        let mut cross = row("a", Some("b"), "/repo");
        cross.profile = "other".into();
        let b = row("b", None, "/repo");
        let orphan = row("c", Some("nowhere"), "/repo");
        let mut cycle = vec![row("x", Some("y"), "/repo"), row("y", Some("x"), "/repo")];
        cycle.push(row("z", Some("x"), "/repo"));
        let mut rows = vec![cross, b, orphan];
        rows.append(&mut cycle);

        reconcile_legacy_rows(&mut rows, &HashSet::from(["main", "other"]));

        let parents: Vec<(&str, Option<&str>)> = rows
            .iter()
            .map(|row| (row.id.as_str(), row.parent_session_id.as_deref()))
            .collect();
        assert_eq!(
            parents,
            vec![
                ("a", None),
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
            &[inst],
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
    fn runtime_ws_requires_one_bearer_header() {
        let mut headers = HeaderMap::new();
        assert!(!has_bearer_header(&headers));
        headers.append(header::AUTHORIZATION, "Bearer token".parse().unwrap());
        assert!(has_bearer_header(&headers));
        headers.append(header::AUTHORIZATION, "Bearer second".parse().unwrap());
        assert!(!has_bearer_header(&headers));
    }
    #[test]
    #[serial_test::serial]
    fn recording_an_empty_store_does_not_create_app_directories() {
        let home = TempHome::new();
        let frames = record_exchange(
            &[],
            RecordedOwner::Remote,
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
        assert!(!home.app_dir().exists());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn physical_alias_stores_are_cached_once_and_reload_acceptance_controls_readiness() {
        use super::super::reload::{load_all_instances, reload_state_instances_from_disk};
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
        let remote = Storage::open_unwatched("remote-a").unwrap();
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
        let loaded = load_all_instances(&state.file_watch);
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
        reload_state_instances_from_disk(&state, loaded, vec![], StatusSource::DiskOnly, 0).await;
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
        let rest = super::super::api::list_sessions(
            axum::extract::State(state.clone()),
            axum::extract::Query(crate::daemon::ListSessionsQuery { state: None }),
        )
        .await;
        assert_eq!(
            rest.0
                .sessions
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["local"]
        );
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
        let loaded = load_all_instances(&state.file_watch);
        state.mutation_epoch.store(1, Ordering::SeqCst);
        reload_state_instances_from_disk(&state, loaded, vec![], StatusSource::DiskOnly, 0).await;
        assert!(!cached_data_healthy(
            &cached_snapshot(&state).await,
            "remote-b"
        ));
        reload_state_instances_from_disk(
            &state,
            load_all_instances(&state.file_watch),
            vec![],
            StatusSource::DiskOnly,
            1,
        )
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
        reload_state_instances_from_disk(
            &state,
            load_all_instances(&state.file_watch),
            vec![],
            StatusSource::DiskOnly,
            1,
        )
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
        reload_state_instances_from_disk(
            &state,
            load_all_instances(&state.file_watch),
            vec![],
            StatusSource::DiskOnly,
            1,
        )
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
        use super::super::reload::{load_all_instances, reload_state_instances_from_disk};
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
            reload_state_instances_from_disk(
                &state,
                load_all_instances(&state.file_watch),
                vec![],
                StatusSource::DiskOnly,
                0,
            )
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
    async fn admission_is_held_until_the_reading_peer_closes() {
        let _home = TempHome::new();
        let mut state = super::super::test_support::build_test_app_state(Vec::new());
        Arc::get_mut(&mut state).unwrap().runtime_read_semaphore =
            Arc::new(tokio::sync::Semaphore::new(1));
        let semaphore = state.runtime_read_semaphore.clone();
        let (socket, mut peer) = socket_pair().await;
        let served = tokio::spawn(run_read(socket, state, Owner::remote(), false));
        for expected in ["hello", "snapshot"] {
            let frame = tokio::time::timeout(Duration::from_secs(5), peer.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let value: serde_json::Value = serde_json::from_str(frame.to_text().unwrap()).unwrap();
            assert_eq!(value["kind"], expected);
        }
        assert_eq!(semaphore.available_permits(), 0);
        peer.close(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), served)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cancellation_retains_admission_until_a_blocked_sample_finishes() {
        let _home = TempHome::new();
        let mut state = super::super::test_support::build_test_app_state(Vec::new());
        Arc::get_mut(&mut state).unwrap().runtime_read_semaphore =
            Arc::new(tokio::sync::Semaphore::new(1));
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
        Storage::open_unwatched("outside")
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
        let rest = super::super::api::list_sessions(
            axum::extract::State(state.clone()),
            axum::extract::Query(crate::daemon::ListSessionsQuery { state: None }),
        )
        .await;
        assert_eq!(
            rest.0
                .sessions
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["canonical"]
        );
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
        super::super::reload::reload_state_instances_from_disk(
            &state,
            super::super::reload::load_all_instances(&state.file_watch),
            vec![],
            super::super::state::StatusSource::DiskOnly,
            0,
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
        use super::super::reload::{load_all_instances, reload_state_instances_from_disk};
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
                Storage::open_unwatched(name)
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
            if !surviving_alias {
                std::fs::remove_file(root.join("outside-z")).unwrap();
            }
            for rejected in [false, true] {
                if rejected {
                    state
                        .mutation_epoch
                        .store(1, std::sync::atomic::Ordering::SeqCst);
                    reload_state_instances_from_disk(
                        &state,
                        load_all_instances(&state.file_watch),
                        vec![],
                        StatusSource::DiskOnly,
                        0,
                    )
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
                let rest = super::super::api::list_sessions(
                    axum::extract::State(state.clone()),
                    axum::extract::Query(crate::daemon::ListSessionsQuery { state: None }),
                )
                .await;
                assert_eq!(
                    rest.0
                        .sessions
                        .iter()
                        .map(|row| row.id.as_str())
                        .collect::<Vec<_>>(),
                    ["canonical"]
                );
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
