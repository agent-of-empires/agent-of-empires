//! Background read of every enabled remote daemon's session list, so the home
//! view can list remote sessions inline with local ones.
//!
//! Remote rows are display and navigation only: they never enter the local
//! instance map, so no local lifecycle action, poller, or storage write can
//! reach a session this machine does not own.

use std::collections::HashMap;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use reqwest::StatusCode;

use crate::daemon::{DaemonClientError, SessionResponse};
use crate::session::{Instance, Item, RemoteShelf, SandboxInfo, Status, WorktreeInfo};
use crate::tui::worker::Worker;

/// One remote's contribution to the sidebar.
#[derive(Debug, Clone)]
pub(crate) struct RemoteSnapshot {
    pub name: String,
    /// The remote's sessions sorted by title, or why they could not be read.
    /// `None` until the first read after it was configured finishes.
    pub sessions: Option<Result<Vec<SessionResponse>, String>>,
    /// What creating a session there needs: profiles, agents, home directory.
    pub meta: Option<RemoteMeta>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct RemoteProfile {
    pub name: String,
    #[serde(default)]
    pub is_default: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct RemoteAgent {
    pub name: String,
    #[serde(default)]
    pub installed: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct RemoteMeta {
    pub home: Option<String>,
    pub profiles: Vec<RemoteProfile>,
    pub agents: Vec<RemoteAgent>,
    /// The remote's `/api/docker/status`; an unread status counts as absent.
    pub container_runtime_available: bool,
}

/// Profiles and agents change rarely; re-read them this often, not every poll.
const META_TTL: std::time::Duration = std::time::Duration::from_secs(300);

impl RemoteSnapshot {
    /// Change detector: the rows as read. The sidebar rebuilds only when this
    /// moves, so an idle remote costs no redraws.
    fn fingerprint(&self) -> SnapshotPrint {
        (self.name.clone(), self.sessions.clone())
    }
}

type SnapshotPrint = (String, Option<Result<Vec<SessionResponse>, String>>);
pub(crate) type RemoteFingerprint = Vec<SnapshotPrint>;

/// A render-only `Instance` for the info panel. Never enters the instance map:
/// it exists so the remote row reuses the local preview renderer verbatim.
pub(crate) fn remote_display_instance(remote: &str, row: &SessionResponse) -> Instance {
    let time = |raw: &str| {
        chrono::DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|t| t.with_timezone(&chrono::Utc))
    };
    let mut inst = Instance::new(&row.title, &row.project_path);
    inst.id = row.id.clone();
    inst.tool = row.tool.clone();
    inst.view = row.view;
    inst.created_at = time(&row.created_at).unwrap_or(inst.created_at);
    inst.last_accessed_at = row.last_accessed_at.as_deref().and_then(time);
    inst.idle_entered_at = row.idle_entered_at.as_deref().and_then(time);
    inst.idle_dormant_since = row.idle_dormant_since.as_deref().and_then(time);
    inst.archived_at = row.archived_at.as_deref().and_then(time);
    inst.trashed_at = row.trashed_at.as_deref().and_then(time);
    inst.snoozed_until = row.snoozed_until.as_deref().and_then(time);
    inst.favorited_at = row.favorited_at.as_deref().and_then(time);
    inst.pinned_at = row.pinned_at.as_deref().and_then(time);
    inst.unread = row.unread;
    inst.source_profile = if row.profile.is_empty() {
        remote.to_string()
    } else {
        format!("{}@{remote}", row.profile)
    };
    inst.status = Status::from_api_str(&row.status).unwrap_or(Status::Unknown);
    inst.last_error = row.last_error.clone();
    inst.worktree_info = match (&row.branch, &row.main_repo_path) {
        (Some(branch), Some(main_repo_path)) => Some(WorktreeInfo {
            branch: branch.clone(),
            main_repo_path: main_repo_path.clone(),
            managed_by_aoe: row.has_managed_worktree,
            created_at: chrono::Utc::now(),
            base_branch: row
                .base_branch_override
                .clone()
                .or_else(|| row.base_branch.clone()),
        }),
        _ => None,
    };
    // The wire row says whether a sandbox exists, not which container.
    inst.sandbox_info = row.is_sandboxed.then(|| SandboxInfo {
        enabled: true,
        container_id: None,
        image: String::new(),
        container_name: format!("container on {remote}"),
        extra_env: None,
        custom_instruction: None,
        container_workdir: None,
        before_start_env: Vec::new(),
    });
    inst
}

/// Display instances for every row read from `snapshots`.
pub(crate) fn display_instances(snapshots: &[RemoteSnapshot]) -> RemoteInstances {
    snapshots
        .iter()
        .filter_map(|snapshot| {
            let rows = snapshot.sessions.as_ref()?.as_ref().ok()?;
            let instances = rows
                .iter()
                .map(|row| (row.id.clone(), remote_display_instance(&snapshot.name, row)))
                .collect();
            Some((snapshot.name.clone(), instances))
        })
        .collect()
}

/// Placeholder snapshots for configured remotes nothing has been read from yet,
/// so their headers show at once instead of the list reshaping seconds later.
pub(crate) fn pending(names: impl IntoIterator<Item = String>) -> Vec<RemoteSnapshot> {
    names
        .into_iter()
        .map(|name| RemoteSnapshot {
            name,
            sessions: None,
            meta: None,
        })
        .collect()
}

/// A remote row's place in the sidebar. `/api/sessions` with no `state`
/// returns every session, so the split happens here.
pub(crate) fn shelf_of(inst: &Instance) -> RemoteShelf {
    if inst.is_trashed() {
        RemoteShelf::Trashed
    } else if inst.is_archived() {
        RemoteShelf::Archived
    } else {
        RemoteShelf::Live
    }
}

/// Remote-side collapse state, keyed by remote and which of its lists.
pub(crate) type CollapsedRemotes = std::collections::HashSet<(String, RemoteShelf)>;

pub(crate) fn fingerprint(snapshots: &[RemoteSnapshot]) -> RemoteFingerprint {
    snapshots.iter().map(RemoteSnapshot::fingerprint).collect()
}

/// Live sidebar rows for the snapshots: a header per remote, then its live
/// sessions. A remote that answered with nothing live still gets a header, so
/// a reachable but idle daemon reads differently from an unconfigured one.
pub(crate) fn remote_items(
    snapshots: &[RemoteSnapshot],
    instances: &RemoteInstances,
    collapsed: &CollapsedRemotes,
    sort_order: crate::session::config::SortOrder,
) -> Vec<Item> {
    let mut items = Vec::new();
    for snapshot in snapshots {
        let is_collapsed = collapsed.contains(&(snapshot.name.clone(), RemoteShelf::Live));
        let header = |session_count, connecting, error| Item::RemoteGroup {
            name: snapshot.name.clone(),
            depth: 0,
            session_count,
            collapsed: is_collapsed,
            connecting,
            error,
            shelf: RemoteShelf::Live,
        };
        match &snapshot.sessions {
            None => items.push(header(0, true, None)),
            Some(Ok(_)) => {
                let mut live = shelved(instances, &snapshot.name, RemoteShelf::Live);
                crate::session::sort_sessions(&mut live, sort_order);
                items.push(header(live.len(), false, None));
                if !is_collapsed {
                    items.extend(session_rows(&snapshot.name, &live, 1));
                }
            }
            Some(Err(error)) => items.push(header(
                0,
                false,
                Some(error.lines().next().unwrap_or_default().to_string()),
            )),
        }
    }
    items
}

/// Display instances of remote rows, by remote name then session id.
pub(crate) type RemoteInstances =
    std::collections::HashMap<String, std::collections::HashMap<String, Instance>>;

fn shelved<'a>(
    instances: &'a RemoteInstances,
    remote: &str,
    shelf: RemoteShelf,
) -> Vec<&'a Instance> {
    let mut rows: Vec<_> = instances.get(remote).map_or_else(Vec::new, |rows| {
        rows.values()
            .filter(|inst| shelf_of(inst) == shelf)
            .collect()
    });
    // A stable base order, so equal sort keys never shuffle between rebuilds.
    rows.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then(a.id.cmp(&b.id))
    });
    rows
}

fn session_rows(remote: &str, rows: &[&Instance], depth: usize) -> Vec<Item> {
    rows.iter()
        .map(|inst| Item::RemoteSession {
            remote: remote.to_string(),
            id: inst.id.clone(),
            depth,
        })
        .collect()
}

/// Rows for one shelf (`Archived` or `Trashed`): a depth-1 sub-header per
/// remote that has any, then those sessions most recent first, as the local
/// shelf orders them. Returns the rows and the session total, which the
/// caller adds to the local section header's count.
pub(crate) fn remote_shelf_items(
    snapshots: &[RemoteSnapshot],
    instances: &RemoteInstances,
    shelf: RemoteShelf,
    collapsed: &CollapsedRemotes,
) -> (Vec<Item>, usize) {
    let mut items = Vec::new();
    let mut total = 0;
    for snapshot in snapshots {
        let mut rows = shelved(instances, &snapshot.name, shelf);
        if rows.is_empty() {
            continue;
        }
        rows.sort_by_key(|inst| {
            std::cmp::Reverse(match shelf {
                RemoteShelf::Trashed => inst.trashed_at,
                _ => inst.archived_at,
            })
        });
        total += rows.len();
        let is_collapsed = collapsed.contains(&(snapshot.name.clone(), shelf));
        items.push(Item::RemoteGroup {
            name: snapshot.name.clone(),
            depth: 1,
            session_count: rows.len(),
            collapsed: is_collapsed,
            connecting: false,
            error: None,
            shelf,
        });
        if !is_collapsed {
            items.extend(session_rows(&snapshot.name, &rows, 2));
        }
    }
    (items, total)
}

/// A remote the TUI can reach, by the name its rows carry.
#[derive(Debug, Clone)]
pub(crate) struct RemoteEntry {
    pub name: String,
    pub endpoint: crate::acp::client::discovery::DaemonEndpoint,
}

/// Every enabled remote, and why the registry could not be read if it could
/// not. The one resolver for the feed, preview, live-send and create.
#[derive(Debug, Default)]
pub(crate) struct EnabledRemotes {
    pub entries: Vec<RemoteEntry>,
    pub registry_error: Option<String>,
}

impl EnabledRemotes {
    pub(crate) fn get(&self, name: &str) -> Option<&RemoteEntry> {
        self.entries.iter().find(|entry| entry.name == name)
    }
}

/// The endpoint of the enabled remote named `name`.
pub(crate) fn remote_endpoint(name: &str) -> Option<crate::acp::client::discovery::DaemonEndpoint> {
    enabled_remotes()
        .entries
        .into_iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.endpoint)
}

/// Enabled registry entries, then the `AOE_DAEMON_URL` daemon as a temporary
/// remote that is never saved.
pub(crate) fn enabled_remotes() -> EnabledRemotes {
    let mut remotes = match crate::daemon::remotes::load() {
        Ok(registry) => EnabledRemotes {
            entries: registry
                .enabled()
                .map(|remote| RemoteEntry {
                    name: remote.name.clone(),
                    endpoint: remote.endpoint(),
                })
                .collect(),
            registry_error: None,
        },
        Err(e) => EnabledRemotes {
            entries: Vec::new(),
            registry_error: Some(format!("{e:#}")),
        },
    };
    if let Some(endpoint) = crate::acp::client::discovery::discover_env() {
        add_env_remote(&mut remotes.entries, endpoint);
    }
    remotes
}

/// Name the env daemon after its host (and port), skip it when a registered
/// remote already points at the same URL, and suffix ` (env)` when only the
/// name collides, so a registered remote keeps its name and rows.
fn add_env_remote(
    entries: &mut Vec<RemoteEntry>,
    endpoint: crate::acp::client::discovery::DaemonEndpoint,
) {
    let url = endpoint.base_url.trim_end_matches('/');
    if entries
        .iter()
        .any(|entry| entry.endpoint.base_url.trim_end_matches('/') == url)
    {
        return;
    }
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            let host = parsed.host_str()?.to_string();
            Some(match parsed.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .unwrap_or_else(|| url.to_string());
    let name = if entries.iter().any(|entry| entry.name == host) {
        format!("{host} (env)")
    } else {
        host
    };
    entries.push(RemoteEntry { name, endpoint });
}

/// Header name for an unreadable registry, listed where its remotes were.
pub(crate) const REGISTRY_ERROR_ROW: &str = "remotes.toml";

async fn fetch_meta(client: &crate::daemon::DaemonClient) -> Option<RemoteMeta> {
    #[derive(serde::Deserialize)]
    struct Home {
        path: String,
    }
    #[derive(serde::Deserialize)]
    struct DockerStatus {
        available: bool,
    }
    let (home, profiles, agents, docker) = tokio::join!(
        client.get_api::<Home>(&["filesystem", "home"], &[]),
        client.get_api::<Vec<RemoteProfile>>(&["profiles"], &[]),
        client.get_api::<Vec<RemoteAgent>>(&["agents"], &[]),
        client.get_api::<DockerStatus>(&["docker", "status"], &[]),
    );
    Some(RemoteMeta {
        home: home.ok().map(|h| h.path),
        profiles: profiles.ok()?,
        agents: agents.unwrap_or_default(),
        container_runtime_available: docker.is_ok_and(|status| status.available),
    })
}

/// Lockout wait when the daemon names none: its own lockout length.
const DEFAULT_LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// A remote the feed stopped asking after it refused this machine. Every
/// refused request is another failed attempt against its lockout, so polling
/// on would keep the IP locked and block pairing again from here.
#[derive(Debug)]
struct Hold {
    credentials: u64,
    /// When to ask again; `None` waits for the entry's credentials to change.
    until: Option<Instant>,
    /// Shown for a credential refusal; a lockout renders its time left.
    refused: Option<String>,
}

impl Hold {
    fn after(error: &DaemonClientError, url: &str, credentials: u64, now: Instant) -> Option<Self> {
        match error {
            DaemonClientError::RateLimited { retry_after_secs } => Some(Self {
                credentials,
                until: Some(now + retry_after_secs.map_or(DEFAULT_LOCKOUT, Duration::from_secs)),
                refused: None,
            }),
            DaemonClientError::Status {
                status: StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN,
                code: None,
                ..
            } => Some(Self {
                credentials,
                until: None,
                refused: Some(format!(
                    "not authorized; pair again with `aoe remote add {url}`"
                )),
            }),
            _ => None,
        }
    }

    /// The header error while this still holds the remote, else `None`.
    fn error(&self, credentials: u64, now: Instant) -> Option<String> {
        if credentials != self.credentials {
            return None;
        }
        match (self.until, &self.refused) {
            (None, Some(refused)) => Some(refused.clone()),
            (Some(until), _) if now < until => Some(crate::daemon::lockout_message(Some(
                until.duration_since(now).as_secs().max(1),
            ))),
            _ => None,
        }
    }
}

type Holds = HashMap<String, Hold>;

async fn fetch_one(
    remote: RemoteEntry,
    cached: Option<RemoteMeta>,
) -> (RemoteSnapshot, Option<Hold>) {
    let name = remote.name;
    let credentials = remote.endpoint.credential_fingerprint();
    let refused = |e: &DaemonClientError| {
        Hold::after(e, &remote.endpoint.base_url, credentials, Instant::now())
    };
    let client = match remote.endpoint.daemon_client() {
        Ok(client) => client,
        Err(e) => {
            let snapshot = RemoteSnapshot {
                name,
                sessions: Some(Err(e.summary())),
                meta: cached,
            };
            return (snapshot, None);
        }
    };
    let (sessions, hold) = match client.list_sessions(None).await {
        Ok(envelope) => {
            let mut rows = envelope.sessions;
            rows.sort_by_key(|row| row.title.to_lowercase());
            (Ok(rows), None)
        }
        Err(e) => {
            let hold = refused(&e);
            let message = hold
                .as_ref()
                .and_then(|hold| hold.error(credentials, Instant::now()))
                .unwrap_or_else(|| e.summary());
            (Err(message), hold)
        }
    };
    let meta = match cached {
        Some(meta) => Some(meta),
        None if sessions.is_ok() => fetch_meta(&client).await,
        None => None,
    };
    let snapshot = RemoteSnapshot {
        name,
        sessions: Some(sessions),
        meta,
    };
    (snapshot, hold)
}

type MetaCache = HashMap<String, (Instant, RemoteMeta)>;

/// One finished read of every enabled remote.
#[derive(Debug)]
pub(crate) struct FeedRead {
    pub snapshots: Vec<RemoteSnapshot>,
    pub registry_error: Option<String>,
}

/// Every enabled remote, concurrently, so one dead daemon cannot stall the
/// rest behind its request timeout.
async fn fetch_all(cache: &mut MetaCache, holds: &mut Holds) -> FeedRead {
    let remotes = enabled_remotes();
    let now = Instant::now();
    let fresh = |cache: &MetaCache, name: &str| {
        cache
            .get(name)
            .filter(|(at, _)| at.elapsed() < META_TTL)
            .map(|(_, meta)| meta.clone())
    };
    holds.retain(|name, _| remotes.get(name).is_some());
    // `None` marks a remote being read; held ones answer without a request.
    let mut slots = Vec::new();
    let mut requests = Vec::new();
    for remote in remotes.entries {
        let credentials = remote.endpoint.credential_fingerprint();
        if let Some(error) = holds
            .get(&remote.name)
            .and_then(|hold| hold.error(credentials, now))
        {
            slots.push(Some(RemoteSnapshot {
                meta: fresh(cache, &remote.name),
                name: remote.name,
                sessions: Some(Err(error)),
            }));
            continue;
        }
        holds.remove(&remote.name);
        let cached = fresh(cache, &remote.name);
        requests.push((cached.is_none(), fetch_one(remote, cached)));
        slots.push(None);
    }
    let refetched: Vec<bool> = requests.iter().map(|(refetch, _)| *refetch).collect();
    let mut fetched = futures_util::future::join_all(requests.into_iter().map(|(_, f)| f))
        .await
        .into_iter()
        .zip(refetched);
    let mut snapshots = Vec::with_capacity(slots.len());
    for slot in slots {
        let snapshot = match slot {
            Some(held) => held,
            None => {
                let Some(((snapshot, hold), refetch)) = fetched.next() else {
                    continue;
                };
                if let (true, Some(meta)) = (refetch, &snapshot.meta) {
                    cache.insert(snapshot.name.clone(), (Instant::now(), meta.clone()));
                }
                if let Some(hold) = hold {
                    holds.insert(snapshot.name.clone(), hold);
                }
                snapshot
            }
        };
        snapshots.push(snapshot);
    }
    FeedRead {
        snapshots,
        registry_error: remotes.registry_error,
    }
}

/// The snapshots to show after `read`. An unreadable registry becomes one
/// error header, and the remotes listed before it stay rather than vanish.
pub(crate) fn merge_read(previous: &[RemoteSnapshot], read: FeedRead) -> Vec<RemoteSnapshot> {
    let Some(error) = read.registry_error else {
        return read.snapshots;
    };
    let mut snapshots = vec![RemoteSnapshot {
        name: REGISTRY_ERROR_ROW.to_string(),
        sessions: Some(Err(error)),
        meta: None,
    }];
    snapshots.extend(read.snapshots);
    for kept in previous {
        if !snapshots.iter().any(|s| s.name == kept.name) {
            snapshots.push(kept.clone());
        }
    }
    snapshots
}

/// Worker thread that reads every enabled remote's session list on request.
pub struct RemoteFeed {
    worker: Worker<(), FeedRead>,
}

impl RemoteFeed {
    pub fn new() -> Self {
        // One current-thread runtime for the worker's lifetime, as in
        // `SessionFeed`: the TUI's own runtime is not reachable from here.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        if let Err(e) = &runtime {
            tracing::warn!(target: "tui.remote_feed", "runtime build failed; remotes stay hidden: {e}");
        }
        Self {
            worker: {
                let mut cache = MetaCache::new();
                let mut holds = Holds::new();
                Worker::spawn("aoe-remote-feed", move |()| match runtime.as_ref() {
                    Ok(rt) => rt.block_on(fetch_all(&mut cache, &mut holds)),
                    Err(e) => FeedRead {
                        snapshots: Vec::new(),
                        registry_error: Some(format!("no runtime: {e}")),
                    },
                })
            },
        }
    }

    pub fn request_refresh(&self) {
        self.worker.request(());
    }

    pub(crate) fn try_recv(&self) -> Result<FeedRead, TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for RemoteFeed {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, title: &str) -> SessionResponse {
        serde_json::from_value(serde_json::json!({"id": id, "title": title})).unwrap()
    }

    fn snap(name: &str, sessions: Option<Result<Vec<SessionResponse>, String>>) -> RemoteSnapshot {
        RemoteSnapshot {
            name: name.into(),
            sessions,
            meta: None,
        }
    }

    #[test]
    fn the_info_panel_instance_carries_the_remote_rows_details() {
        let row: SessionResponse = serde_json::from_value(serde_json::json!({
            "id": "r1",
            "title": "refactor",
            "project_path": "/Users/me/scm/app-wt",
            "tool": "codex",
            "status": "Running",
            "profile": "work",
            "branch": "feature/x",
            "main_repo_path": "/Users/me/scm/app",
            "base_branch": "main",
            "is_sandboxed": true,
        }))
        .unwrap();

        let inst = remote_display_instance("mini", &row);
        assert_eq!(inst.id, "r1");
        assert_eq!(inst.tool, "codex");
        assert_eq!(inst.source_profile, "work@mini");
        assert_eq!(inst.status, Status::Running);
        let wt = inst.worktree_info.expect("worktree shown");
        assert_eq!(wt.branch, "feature/x");
        assert_eq!(wt.main_repo_path, "/Users/me/scm/app");
        assert_eq!(wt.base_branch.as_deref(), Some("main"));
        assert!(inst.sandbox_info.is_some_and(|s| s.enabled));
    }

    #[test]
    fn a_refusing_remote_is_left_alone_until_its_wait_or_its_credentials_change() {
        let now = Instant::now();
        let url = "http://100.89.98.53:63827";
        let status = |status| DaemonClientError::Status {
            status,
            code: None,
            body: String::new(),
            truncated: false,
        };

        let refused = Hold::after(&status(StatusCode::UNAUTHORIZED), url, 1, now).unwrap();
        let error = refused.error(1, now + Duration::from_secs(3600)).unwrap();
        assert!(error.contains(&format!("aoe remote add {url}")), "{error}");
        assert_eq!(refused.error(2, now), None, "re-paired: poll again");

        let locked = DaemonClientError::RateLimited {
            retry_after_secs: Some(725),
        };
        let locked = Hold::after(&locked, url, 1, now).unwrap();
        let error = locked.error(1, now + Duration::from_secs(60)).unwrap();
        assert!(error.contains("locked out for 12m"), "{error}");
        assert_eq!(locked.error(1, now + Duration::from_secs(725)), None);

        let unnamed = DaemonClientError::RateLimited {
            retry_after_secs: None,
        };
        let unnamed = Hold::after(&unnamed, url, 1, now).unwrap();
        assert!(unnamed.error(1, now + DEFAULT_LOCKOUT / 2).is_some());

        for passing in [
            DaemonClientError::Transport,
            status(StatusCode::INTERNAL_SERVER_ERROR),
            DaemonClientError::Status {
                status: StatusCode::FORBIDDEN,
                code: Some(crate::daemon::ApiErrorCode::ReadOnly),
                body: String::new(),
                truncated: false,
            },
        ] {
            assert!(Hold::after(&passing, url, 1, now).is_none(), "{passing:?}");
        }
    }

    #[test]
    fn a_row_without_worktree_or_sandbox_shows_neither() {
        let row: SessionResponse =
            serde_json::from_value(serde_json::json!({"id": "r1", "title": "t"})).unwrap();
        let inst = remote_display_instance("mini", &row);
        assert!(inst.worktree_info.is_none());
        assert!(inst.sandbox_info.is_none());
        assert_eq!(inst.source_profile, "mini");
    }

    fn items(snaps: &[RemoteSnapshot], collapsed: &CollapsedRemotes) -> Vec<Item> {
        remote_items(
            snaps,
            &display_instances(snaps),
            collapsed,
            crate::session::config::SortOrder::AZ,
        )
    }

    fn shelf(
        snaps: &[RemoteSnapshot],
        shelf: RemoteShelf,
        collapsed: &CollapsedRemotes,
    ) -> (Vec<Item>, usize) {
        remote_shelf_items(snaps, &display_instances(snaps), shelf, collapsed)
    }

    #[test]
    fn a_reachable_remote_lists_a_header_then_its_sessions_one_level_in() {
        let items = items(
            &[snap(
                "mini",
                Some(Ok(vec![row("a", "alpha"), row("b", "beta")])),
            )],
            &CollapsedRemotes::new(),
        );
        assert!(matches!(
            &items[0],
            Item::RemoteGroup { name, session_count: 2, error: None, connecting: false, depth: 0, collapsed: false, shelf: RemoteShelf::Live } if name == "mini"
        ));
        let ids: Vec<_> = items[1..]
            .iter()
            .map(|i| match i {
                Item::RemoteSession {
                    remote,
                    id,
                    depth: 1,
                } if remote == "mini" => id.as_str(),
                other => panic!("unexpected row {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["a", "b"]);
    }

    #[test]
    fn remote_rows_follow_the_sidebar_sort_order() {
        use crate::session::config::SortOrder;
        let snaps = [snap(
            "mini",
            Some(Ok(vec![row("a", "alpha"), row("b", "beta")])),
        )];
        for (order, expected) in [(SortOrder::AZ, ["a", "b"]), (SortOrder::ZA, ["b", "a"])] {
            let items = remote_items(
                &snaps,
                &display_instances(&snaps),
                &CollapsedRemotes::new(),
                order,
            );
            let ids: Vec<_> = items[1..]
                .iter()
                .map(|i| match i {
                    Item::RemoteSession { id, .. } => id.as_str(),
                    other => panic!("unexpected {other:?}"),
                })
                .collect();
            assert_eq!(ids, expected, "{order:?}");
        }
    }

    #[test]
    fn a_collapsed_remote_keeps_its_header_and_count_but_hides_rows() {
        let collapsed: CollapsedRemotes = [("mini".to_string(), RemoteShelf::Live)].into();
        let items = items(
            &[snap("mini", Some(Ok(vec![row("a", "alpha")])))],
            &collapsed,
        );
        assert_eq!(items.len(), 1);
        assert!(matches!(
            &items[0],
            Item::RemoteGroup {
                session_count: 1,
                collapsed: true,
                ..
            }
        ));
    }

    #[test]
    fn an_unread_remote_is_connecting_and_an_unreachable_one_carries_its_first_error_line() {
        let items = items(
            &[
                snap("new", None),
                snap(
                    "down",
                    Some(Err("connection refused\ncaused by: tcp".into())),
                ),
            ],
            &CollapsedRemotes::new(),
        );
        assert!(matches!(
            &items[0],
            Item::RemoteGroup {
                connecting: true,
                error: None,
                ..
            }
        ));
        assert!(matches!(
            &items[1],
            Item::RemoteGroup { connecting: false, error: Some(e), .. } if e == "connection refused"
        ));
    }

    #[test]
    fn an_unreadable_registry_shows_one_error_row_and_keeps_listed_remotes() {
        let previous = [
            snap("mini", Some(Ok(vec![row("a", "alpha")]))),
            snap(REGISTRY_ERROR_ROW, Some(Err("old".into()))),
        ];
        let merged = merge_read(
            &previous,
            FeedRead {
                snapshots: Vec::new(),
                registry_error: Some("remotes registry file is group/world accessible".into()),
            },
        );
        let names: Vec<_> = merged.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, [REGISTRY_ERROR_ROW, "mini"]);
        assert!(matches!(&merged[0].sessions, Some(Err(e)) if e.contains("accessible")));

        let healthy = merge_read(
            &merged,
            FeedRead {
                snapshots: vec![snap("mini", None)],
                registry_error: None,
            },
        );
        assert_eq!(healthy.len(), 1, "a readable registry replaces the list");
    }

    #[test]
    fn fingerprint_moves_with_any_row_change() {
        let mut a = row("a", "alpha");
        let before = fingerprint(&[snap("m", Some(Ok(vec![a.clone()])))]);
        assert_eq!(before, fingerprint(&[snap("m", Some(Ok(vec![a.clone()])))]));
        a.unread = true;
        assert_ne!(before, fingerprint(&[snap("m", Some(Ok(vec![a])))]));
        assert_ne!(
            fingerprint(&[snap("m", None)]),
            fingerprint(&[snap("m", Some(Ok(vec![])))])
        );
    }

    fn shelved(id: &str, archived: Option<&str>, trashed: Option<&str>) -> SessionResponse {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "title": id,
            "archived_at": archived,
            "trashed_at": trashed,
        }))
        .unwrap()
    }

    #[test]
    fn the_live_section_leaves_archived_and_trashed_rows_to_the_shelf() {
        let rows = vec![
            row("live", "live"),
            shelved("old", Some("2026-01-01T00:00:00Z"), None),
            shelved("gone", None, Some("2026-02-01T00:00:00Z")),
            // Trash wins over archive, as for local rows.
            shelved(
                "both",
                Some("2026-01-01T00:00:00Z"),
                Some("2026-03-01T00:00:00Z"),
            ),
        ];
        let snaps = [snap("mini", Some(Ok(rows)))];
        let collapsed = CollapsedRemotes::new();

        let live = items(&snaps, &collapsed);
        assert!(matches!(
            &live[0],
            Item::RemoteGroup {
                session_count: 1,
                ..
            }
        ));
        assert_eq!(live.len(), 2);

        let (archived, archived_total) = shelf(&snaps, RemoteShelf::Archived, &collapsed);
        assert_eq!(archived_total, 1);
        assert!(matches!(
            &archived[0],
            Item::RemoteGroup {
                depth: 1,
                shelf: RemoteShelf::Archived,
                ..
            }
        ));
        assert!(matches!(&archived[1], Item::RemoteSession { id, depth: 2, .. } if id == "old"));

        let (trashed, trashed_total) = shelf(&snaps, RemoteShelf::Trashed, &collapsed);
        assert_eq!(trashed_total, 2);
        let ids: Vec<_> = trashed[1..]
            .iter()
            .map(|i| match i {
                Item::RemoteSession { id, .. } => id.as_str(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(ids, ["both", "gone"], "most recently trashed first");
    }

    #[test]
    fn a_remote_with_nothing_shelved_adds_no_shelf_rows() {
        let snaps = [snap("mini", Some(Ok(vec![row("live", "live")])))];
        let (items, total) = shelf(&snaps, RemoteShelf::Trashed, &CollapsedRemotes::new());
        assert!(items.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn the_env_daemon_joins_the_registry_without_shadowing_it() {
        use crate::acp::client::discovery::{DaemonEndpoint, Source};
        let env = |url: &str| DaemonEndpoint::new(url.into(), Some("t".into()), Source::Env);
        let registered = |name: &str, url: &str| RemoteEntry {
            name: name.into(),
            endpoint: DaemonEndpoint::new(url.into(), None, Source::Remote),
        };

        for (existing, url, expected) in [
            (
                vec![],
                "https://box.tailnet.ts.net/",
                Some("box.tailnet.ts.net"),
            ),
            (vec![], "http://127.0.0.1:8080", Some("127.0.0.1:8080")),
            (
                vec![registered("mini", "https://box.tailnet.ts.net")],
                "https://box.tailnet.ts.net/",
                None,
            ),
            (
                vec![registered("box.tailnet.ts.net", "https://other.example")],
                "https://box.tailnet.ts.net",
                Some("box.tailnet.ts.net (env)"),
            ),
        ] {
            let mut entries = existing;
            let before = entries.len();
            add_env_remote(&mut entries, env(url));
            let added = (entries.len() > before).then(|| entries.last().unwrap().name.as_str());
            assert_eq!(added, expected, "{url}");
            if let Some(entry) = entries.get(before) {
                assert_eq!(entry.endpoint.source, Source::Env);
            }
        }
    }

    #[test]
    fn profiles_and_agents_decode_from_the_daemon_shapes() {
        let profiles: Vec<RemoteProfile> = serde_json::from_value(serde_json::json!([
            {"name": "main", "is_default": true, "description": "day job"},
            {"name": "side"}
        ]))
        .unwrap();
        assert!(profiles[0].is_default && !profiles[1].is_default);
        let agents: Vec<RemoteAgent> = serde_json::from_value(serde_json::json!([
            {"kind": "builtin", "name": "codex", "installed": true, "acp_capable": true},
            {"kind": "builtin", "name": "gemini", "installed": false}
        ]))
        .unwrap();
        assert_eq!(
            agents
                .iter()
                .filter(|a| a.installed)
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>(),
            ["codex"]
        );
    }
}
