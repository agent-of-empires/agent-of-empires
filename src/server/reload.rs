//! Reloading session rows from disk and merging them onto what the daemon
//! already holds in memory.

use crate::file_watch::FileWatchService;
use crate::session::Handoff;
use crate::session::Instance;
use crate::session::Status;
use crate::session::Storage;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ProfileIdentity {
    device: u64,
    inode: u64,
}

impl ProfileIdentity {
    pub(crate) fn verify_storage(self, storage: &Storage) -> anyhow::Result<()> {
        let (device, inode) = storage.bound_profile_identity()?;
        anyhow::ensure!(
            self == Self { device, inode },
            "Profile identity changed during read"
        );
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct SelectableProfile {
    pub name: String,
    pub listed: bool,
    pub aliases: Vec<String>,
    pub identity: ProfileIdentity,
}

/// Physical stores, with local enumeration kept separate from selectable aliases.
pub(crate) fn selectable_profiles() -> anyhow::Result<Vec<SelectableProfile>> {
    let root = crate::session::get_profile_dir_path("__inventory__")?
        .parent()
        .expect("profile parent")
        .to_path_buf();
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut directories = Vec::new();
    let mut aliases = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let kind = entry.file_type()?;
        if !kind.is_dir() && !kind.is_symlink() {
            continue;
        }
        if kind.is_symlink() && !crate::session::valid_profile_name(&name) {
            continue;
        }
        let (device, inode) = if kind.is_symlink() {
            let path = entry.path();
            let sampled = super::alias_io::sample(&name, "identity", move || {
                std::fs::metadata(path)
                    .ok()
                    .filter(std::fs::Metadata::is_dir)
                    .map(|metadata| crate::process::metadata_identity(&metadata))
            });
            match sampled.value.and_then(|identity| *identity) {
                Some(identity) => identity,
                None => continue,
            }
        } else {
            match std::fs::metadata(entry.path()) {
                Ok(metadata) if metadata.is_dir() => crate::process::metadata_identity(&metadata),
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let identity = ProfileIdentity { device, inode };
        if kind.is_dir() {
            directories.push(SelectableProfile {
                name,
                listed: true,
                aliases: Vec::new(),
                identity,
            });
        } else {
            aliases.push((name, identity));
        }
    }
    directories.sort_by(|left, right| left.name.cmp(&right.name));
    aliases.sort_by(|left, right| left.0.cmp(&right.0));
    for (name, identity) in aliases {
        if let Some(profile) = directories
            .iter_mut()
            .find(|profile| profile.identity == identity)
        {
            profile.aliases.push(name);
        } else {
            directories.push(SelectableProfile {
                name,
                listed: false,
                aliases: Vec::new(),
                identity,
            });
        }
    }
    directories.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(directories)
}

#[derive(Clone, Default)]
pub(crate) struct SessionLoadHealth {
    pub enumeration_failed: bool,
    pub unreadable_profiles: std::collections::HashSet<String>,
}

#[derive(Default)]
pub(crate) struct LoadedInstances {
    pub instances: Vec<Instance>,
    pub cache: RuntimeReadCache,
}

#[cfg(any(test, debug_assertions))]
impl From<Vec<Instance>> for LoadedInstances {
    fn from(instances: Vec<Instance>) -> Self {
        Self {
            instances,
            cache: RuntimeReadCache::default(),
        }
    }
}

pub(super) fn load_all_instances(file_watch: &Arc<FileWatchService>) -> LoadedInstances {
    load_read_instances(file_watch, None)
}

fn read_inventory_instances(
    name: &str,
    identity: ProfileIdentity,
    file_watch: Arc<FileWatchService>,
) -> anyhow::Result<Vec<Instance>> {
    let storage = Storage::open_inventory_reference(name, file_watch)?;
    identity.verify_storage(&storage)?;
    let instances = storage.load_instances_readonly()?;
    storage.verify_bound_profile()?;
    Ok(instances)
}

fn load_read_instances(
    file_watch: &Arc<FileWatchService>,
    canonical: Option<LoadedInstances>,
) -> LoadedInstances {
    let reuse_canonical = canonical.is_some();
    let mut loaded = canonical.unwrap_or_default();
    let inventory = match selectable_profiles() {
        Ok(profiles) => profiles,
        Err(error) => {
            tracing::warn!(target: "server.file_watch", %error, "profile enumeration failed");
            loaded.cache.health.enumeration_failed = true;
            return loaded;
        }
    };
    for mut profile in inventory {
        if let Some(index) = loaded
            .cache
            .inventory
            .iter()
            .position(|accepted| accepted.identity == profile.identity)
        {
            for name in std::iter::once(profile.name).chain(profile.aliases) {
                if !loaded
                    .cache
                    .inventory
                    .iter()
                    .any(|accepted| accepted.name == name)
                    && !loaded.cache.inventory[index].aliases.contains(&name)
                {
                    loaded.cache.inventory[index].aliases.push(name);
                }
            }
            continue;
        }
        if loaded
            .cache
            .inventory
            .iter()
            .any(|accepted| accepted.name == profile.name)
        {
            continue;
        }
        profile.aliases.retain(|name| {
            !loaded
                .cache
                .inventory
                .iter()
                .any(|accepted| &accepted.name == name)
        });
        let (rows, stale) = if profile.listed {
            (
                read_inventory_instances(&profile.name, profile.identity, file_watch.clone())
                    .map_err(|error| error.to_string()),
                false,
            )
        } else {
            let name = profile.name.clone();
            let identity = profile.identity;
            let file_watch = file_watch.clone();
            let sampled = super::alias_io::sample(&profile.name, "sessions", move || {
                read_inventory_instances(&name, identity, file_watch)
                    .map(|instances| (identity, instances))
                    .map_err(|error| error.to_string())
            });
            let rows = match sampled.value.as_deref() {
                Some(Ok((identity, instances))) if *identity == profile.identity => {
                    Ok(instances.clone())
                }
                Some(Ok(_)) => Err("Profile identity changed during read".to_owned()),
                Some(Err(error)) => Err(error.clone()),
                None => Err("alias read pending".to_owned()),
            };
            (rows, !sampled.fresh)
        };
        match rows {
            Ok(mut instances) => {
                for instance in &mut instances {
                    instance.source_profile.clone_from(&profile.name);
                }
                if profile.listed && !reuse_canonical {
                    loaded.instances.extend(instances);
                } else {
                    loaded.cache.alias_only_instances.extend(instances);
                }
                if stale {
                    loaded
                        .cache
                        .health
                        .unreadable_profiles
                        .insert(profile.name.clone());
                }
            }
            Err(error) => {
                tracing::warn!(target: "server.file_watch", profile = %profile.name, %error, "session load failed");
                loaded
                    .cache
                    .health
                    .unreadable_profiles
                    .insert(profile.name.clone());
            }
        }
        loaded.cache.inventory.push(profile);
    }
    loaded
        .cache
        .inventory
        .sort_by(|left, right| left.name.cmp(&right.name));
    loaded
}

pub(super) fn load_profiles_for_reload(
    file_watch: &Arc<FileWatchService>,
) -> (LoadedInstances, Result<CanonicalMetadata, ReloadFailure>) {
    load_read_projection(file_watch, load_all_profiles(file_watch))
}

pub(super) fn load_read_projection(
    file_watch: &Arc<FileWatchService>,
    canonical: Result<LoadedProfiles, ReloadFailure>,
) -> (LoadedInstances, Result<CanonicalMetadata, ReloadFailure>) {
    match canonical {
        Ok(LoadedProfiles {
            instances,
            inventory,
            metadata,
        }) => (
            load_read_instances(
                file_watch,
                Some(LoadedInstances {
                    instances,
                    cache: RuntimeReadCache {
                        inventory,
                        ..Default::default()
                    },
                }),
            ),
            Ok(metadata),
        ),
        Err(error) => (load_all_instances(file_watch), Err(error)),
    }
}

#[derive(Default)]
pub(crate) struct RuntimeReadCache {
    pub inventory: Vec<SelectableProfile>,
    /// CLI-only rows, including listed profiles discovered after the native snapshot.
    pub alias_only_instances: Vec<Instance>,
    pub health: SessionLoadHealth,
}

impl RuntimeReadCache {
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn accepted_inventory() -> Self {
        match selectable_profiles() {
            Ok(inventory) => Self {
                inventory,
                ..Self::default()
            },
            Err(_) => Self {
                health: SessionLoadHealth {
                    enumeration_failed: true,
                    ..Default::default()
                },
                ..Self::default()
            },
        }
    }
}
use super::push::{publish, publish_status_change};

use super::state::{AppState, StatusSource};
use super::structured_repair::{
    persist_structured_row_repairs, repair_structured_rows_from_live_workers,
    LiveStructuredWorkerRecord,
};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CanonicalMetadata {
    pub default_profile: String,
    pub profiles: Vec<crate::daemon::ProfileSnapshot>,
    pub workspace_ordering: Vec<String>,
    pub global_projects: Vec<crate::daemon::ProjectResponse>,
    pub status_hooks: std::collections::HashMap<String, crate::status_hooks::StatusHookConfig>,
    pub auxiliary_tools: std::collections::HashMap<String, Vec<String>>,
    pub raw_tool_names: std::collections::HashMap<String, Vec<String>>,
}

pub(crate) struct LoadedProfiles {
    pub instances: Vec<Instance>,
    pub metadata: CanonicalMetadata,
    pub inventory: Vec<SelectableProfile>,
}

#[derive(Debug, thiserror::Error)]
#[error("canonical profile load failed")]
pub(crate) struct ReloadFailure {
    pub health: crate::daemon::RuntimeHealth,
    #[source]
    pub(super) source: anyhow::Error,
}
impl ReloadFailure {
    pub(crate) fn owner_diagnostics(&self) -> Vec<crate::daemon::OwnerLoadDiagnostic> {
        self.source.downcast_ref::<crate::session::LoadSource>().map(|source| {
            vec![crate::daemon::OwnerLoadDiagnostic {
                path: crate::session::safe_load_label(&source.0.to_string_lossy()),
                message: "Repair the unreadable or invalid file; a fresh runtime snapshot restores access.".to_owned(),
            }]
        }).unwrap_or_default()
    }
}

pub(crate) fn load_all_profiles(
    file_watch: &Arc<FileWatchService>,
) -> Result<LoadedProfiles, ReloadFailure> {
    use crate::daemon::{ProfileSnapshot, ProjectResponse, ReloadFailureCode, RuntimeHealth};
    let profiles = crate::session::list_profiles().map_err(|source| ReloadFailure {
        health: RuntimeHealth::Degraded {
            code: ReloadFailureCode::ProfileEnumeration,
            profiles: Vec::new(),
        },
        source,
    })?;
    let (default_profile, workspace_ordering, global_projects) = (|| {
        let mut default_profile = crate::session::config::Config::load()?.default_profile;
        anyhow::ensure!(!profiles.is_empty(), "Profile catalogue is empty");
        if default_profile.is_empty() {
            profiles[0].clone_into(&mut default_profile);
        } else {
            anyhow::ensure!(
                profiles.contains(&default_profile),
                "Configured default profile is unavailable"
            );
        }
        let ordering = crate::session::load_workspace_ordering()?.order;
        let projects = crate::session::projects::load_global()?
            .into_iter()
            .map(ProjectResponse::from)
            .collect();
        Ok::<_, anyhow::Error>((default_profile, ordering, projects))
    })()
    .map_err(|source| ReloadFailure {
        health: RuntimeHealth::Degraded {
            code: ReloadFailureCode::Metadata,
            profiles: Vec::new(),
        },
        source,
    })?;
    let mut all = Vec::new();
    let mut profile_metadata = Vec::with_capacity(profiles.len());
    let mut inventory = Vec::with_capacity(profiles.len());
    let mut status_hooks = std::collections::HashMap::new();
    let mut auxiliary_tools = std::collections::HashMap::new();
    let mut raw_tool_names = std::collections::HashMap::new();
    for profile in profiles {
        let (mut instances, groups, details, identity) = (|| {
            let storage = Storage::open(&profile, file_watch.clone()).map_err(|error| {
                match crate::session::get_profile_dir_path(&profile) {
                    Ok(directory) => error.context(crate::session::LoadSource(directory)),
                    Err(_) => error,
                }
            })?;
            let (instances, groups) = storage.load_complete_with_groups()?;
            let (device, inode) = storage.bound_profile_identity()?;
            let details = load_profile_details(&profile)?;
            Ok::<_, anyhow::Error>((
                instances,
                groups,
                details,
                ProfileIdentity { device, inode },
            ))
        })()
        .map_err(|source| ReloadFailure {
            health: RuntimeHealth::Degraded {
                code: ReloadFailureCode::ProfileData,
                profiles: vec![profile.clone()],
            },
            source,
        })?;
        for inst in &mut instances {
            inst.source_profile = profile.clone();
        }
        all.extend(instances);
        status_hooks.insert(profile.clone(), details.status_hooks);
        auxiliary_tools.insert(profile.clone(), details.auxiliary_tools);
        raw_tool_names.insert(profile.clone(), details.raw_tool_names);
        inventory.push(SelectableProfile {
            name: profile.clone(),
            listed: true,
            aliases: Vec::new(),
            identity,
        });
        profile_metadata.push(ProfileSnapshot {
            name: profile,
            description: details.description,
            groups,
            projects: details.projects,
        });
    }
    Ok(LoadedProfiles {
        instances: all,
        inventory,
        metadata: CanonicalMetadata {
            default_profile,
            profiles: profile_metadata,
            workspace_ordering,
            global_projects,
            status_hooks,
            auxiliary_tools,
            raw_tool_names,
        },
    })
}

pub(super) struct ProfileDetails {
    pub description: Option<String>,
    pub projects: Vec<crate::daemon::ProjectResponse>,
    pub status_hooks: crate::status_hooks::StatusHookConfig,
    pub auxiliary_tools: Vec<String>,
    pub raw_tool_names: Vec<String>,
}

pub(super) fn load_profile_details(profile: &str) -> anyhow::Result<ProfileDetails> {
    let description = crate::session::load_profile_config(profile)?.description;
    let projects = crate::session::projects::load_profile(profile)?
        .into_iter()
        .map(crate::daemon::ProjectResponse::from)
        .collect();
    let config = crate::session::resolve_config(profile)?;
    let mut raw_tool_names: Vec<_> = config.tools.keys().cloned().collect();
    raw_tool_names.sort_unstable();
    let mut auxiliary_tools: Vec<_> = config
        .tools
        .into_iter()
        .filter(|(_, tool)| !tool.background && !tool.command.is_empty())
        .map(|(name, _)| name)
        .collect();
    auxiliary_tools.sort_unstable();
    Ok(ProfileDetails {
        description,
        projects,
        status_hooks: config.status_hooks,
        auxiliary_tools,
        raw_tool_names,
    })
}

#[derive(Clone, Copy)]
pub(crate) enum StatusCommit {
    Passive,
    Lifecycle,
    LaunchReady,
}

pub(super) fn replace_committed_profiles<const N: usize>(
    current: &mut Vec<Instance>,
    metadata: &mut CanonicalMetadata,
    mut committed: [(&str, Vec<Instance>, Vec<crate::session::Group>); N],
    committed_status: impl Fn(&str) -> Option<StatusCommit>,
) -> Result<(), ReloadFailure> {
    let invalid_profile = |profile: &str, message: &'static str| ReloadFailure {
        health: crate::daemon::RuntimeHealth::Degraded {
            code: crate::daemon::ReloadFailureCode::Metadata,
            profiles: vec![profile.to_owned()],
        },
        source: anyhow::anyhow!(message),
    };
    let mut profile_indices = [0; N];
    for (index, (profile, _, _)) in committed.iter().enumerate() {
        let profile_index = metadata
            .profiles
            .iter()
            .position(|item| item.name == *profile)
            .ok_or_else(|| {
                invalid_profile(
                    profile,
                    "committed profile is missing from canonical metadata",
                )
            })?;
        if profile_indices[..index].contains(&profile_index) {
            return Err(invalid_profile(
                profile,
                "profile occurs more than once in a commit",
            ));
        }
        profile_indices[index] = profile_index;
    }
    let prior: std::collections::HashMap<_, _> = current
        .iter()
        .filter(|row| {
            committed
                .iter()
                .any(|(profile, _, _)| row.source_profile == *profile)
        })
        .map(|row| (row.id.as_str(), row))
        .collect();
    for (profile, rows, _) in &mut committed {
        for row in rows {
            if row.source_profile != *profile {
                (*profile).clone_into(&mut row.source_profile);
            }
            if let Some(previous) = prior.get(row.id.as_str()) {
                let status = row.status;
                let idle_entered_at = row.idle_entered_at;
                let committed_error = row.last_error.take();
                if previous.source_profile == *profile {
                    row.merge_runtime_from_reload(previous);
                } else {
                    row.merge_runtime_for_profile_move(previous);
                }
                if let Some(commit) = committed_status(&row.id) {
                    let owns_status = match commit {
                        StatusCommit::LaunchReady => {
                            previous.lifecycle_generation == row.lifecycle_generation
                                && previous.status == crate::session::Status::Starting
                        }
                        StatusCommit::Passive | StatusCommit::Lifecycle => true,
                    };
                    if owns_status {
                        row.status = status;
                        row.idle_entered_at = idle_entered_at;
                        if !matches!(commit, StatusCommit::Passive) {
                            row.last_error = committed_error;
                        }
                    }
                }
                row.last_accessed_at = row.last_accessed_at.max(previous.last_accessed_at);
            }
            if row.is_archived() {
                row.settle_archived_status();
            }
        }
    }
    let capacity = current.len() - prior.len()
        + committed
            .iter()
            .map(|(_, rows, _)| rows.len())
            .sum::<usize>();
    drop(prior);
    let mut retained = Vec::with_capacity(capacity);
    let mut insertion: [_; N] = std::array::from_fn(|index| (usize::MAX, usize::MAX, index));
    for (position, row) in current.drain(..).enumerate() {
        if let Some(index) = committed
            .iter()
            .position(|(profile, _, _)| row.source_profile == *profile)
        {
            if insertion[index].0 == usize::MAX {
                insertion[index] = (retained.len(), position, index);
            }
        } else {
            retained.push(row);
        }
    }
    insertion.sort_unstable();
    for (offset, _, index) in insertion.into_iter().rev() {
        let offset = offset.min(retained.len());
        retained.splice(offset..offset, committed[index].1.drain(..));
    }
    *current = retained;
    for ((_, _, groups), profile_index) in committed.into_iter().zip(profile_indices) {
        metadata.profiles[profile_index].groups = groups;
    }
    Ok(())
}

/// The reload gate must cover disk sampling through canonical adoption.
pub(crate) async fn adopt_committed_profiles<const N: usize>(
    state: &Arc<AppState>,
    committed: [(&str, Vec<Instance>, Vec<crate::session::Group>); N],
    committed_status: impl Fn(&str) -> Option<StatusCommit>,
    _reload: &tokio::sync::OwnedRwLockWriteGuard<()>,
    _publication: &tokio::sync::RwLockWriteGuard<'_, ()>,
) -> Result<(), ReloadFailure> {
    let mut metadata = state.canonical_metadata.write().await;
    let mut current = state.instances.write().await;
    let prior_statuses: Vec<_> = current
        .iter()
        .filter(|row| committed_status(&row.id).is_some())
        .map(|row| (row.id.clone(), row.status))
        .collect();
    replace_committed_profiles(&mut current, &mut metadata, committed, committed_status)?;
    state
        .mutation_epoch
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    state.runtime.request_publish();
    for (id, old) in prior_statuses {
        if let Some(row) = current.iter().find(|row| row.id == id) {
            publish_status_change(&state.status_tx, row, old);
        }
    }
    Ok(())
}

/// Keep process state without replacing the committed lifecycle or identity.
///
/// The poller guards live in [`Handoff`], shared with the status-poll
/// branch of [`merge_loaded_rows`]: both hand a prior row's launch-owned process
/// state to the fresh row, so both must decide the same way whether that row
/// still runs what the prior one ran.
pub(super) fn merge_runtime_fields(prior: Instance, fresh: &mut Instance) {
    let handoff = Handoff::decide(&prior.capture_poller, prior.held(), fresh.held());
    // `plugin_revival_pending` is `#[serde(skip)]`, so every fresh disk load
    // defaults it; dropping it here would stop counting a plugin revival that is
    // still waking toward its per-plugin cap.
    let plugin_revival_pending = prior.plugin_revival_pending;
    fresh.last_error_check = prior.last_error_check;
    fresh.inherit_runtime(prior, fresh.status == Status::Error);
    handoff.settle(&mut fresh.capture_poller);
    fresh.plugin_revival_pending = plugin_revival_pending;
}

/// Adopt the prior row's launch-owned process state onto a fresh row whose
/// status, panes and detection this tick's tmux scrape already decided, and so
/// must keep them: [`merge_runtime_fields`] would carry the prior row's
/// observations back over the fresh ones. The poller guards are the same
/// [`Handoff`] the full merge applies, because the poller and its repair
/// window are launch-owned whatever decided the status, and `plugin_revival_pending`
/// is `#[serde(skip)]` like the poller, so a fresh disk load defaults it.
fn merge_process_runtime_fields(prior: Instance, fresh: &mut Instance) {
    let handoff = Handoff::decide(&prior.capture_poller, prior.held(), fresh.held());
    // `#[serde(skip)]` like the poller, so a fresh disk load defaults it and a
    // revival slower than one tick would stop counting toward its plugin cap.
    let plugin_revival_pending = prior.plugin_revival_pending;
    fresh.inherit_process_runtime(prior);
    handoff.settle(&mut fresh.capture_poller);
    fresh.plugin_revival_pending = plugin_revival_pending;
}

/// Observations erased by storage loading and carried into the next sample.
#[derive(Debug, Clone, Default)]
pub(super) struct PriorTickTracking {
    lifecycle_generation: u64,
    idle_entered_at: Option<chrono::DateTime<chrono::Utc>>,
    ever_confirmed_present: bool,
    unknown_since: Option<std::time::Instant>,
    detection: crate::session::DetectionState,
    auxiliary_targets: Vec<crate::session::AuxiliaryTarget>,
}

impl PriorTickTracking {
    pub(super) fn of(inst: &Instance) -> Self {
        Self {
            lifecycle_generation: inst.lifecycle_generation,
            idle_entered_at: inst.idle_entered_at,
            ever_confirmed_present: inst.ever_confirmed_present,
            unknown_since: inst.unknown_since,
            detection: inst.detection,
            auxiliary_targets: inst
                .auxiliary
                .iter()
                .map(|observation| observation.target.clone())
                .collect(),
        }
    }
}

/// Restore the escalation clock, confirmations and known auxiliary targets before sampling.
pub(super) fn seed_tick_tracking(
    instances: &mut [Instance],
    prev: &std::collections::HashMap<String, PriorTickTracking>,
) {
    for inst in instances {
        if let Some(prior) = prev.get(&inst.id) {
            inst.ever_confirmed_present = prior.ever_confirmed_present;
            inst.unknown_since = prior.unknown_since;
            inst.detection = prior.detection;
            inst.auxiliary = prior
                .auxiliary_targets
                .iter()
                .cloned()
                .map(|target| crate::session::AuxiliaryObservation {
                    target,
                    pane: Default::default(),
                })
                .collect();
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct SandboxHealth {
    pub generation: u64,
    pub container_name: String,
    pub running: bool,
}

/// Committed launches use the persisted pane birth for their startup grace.
/// Future wall-clock stamps grant no grace; ownership has its separate TTL.
fn starting_status_in_grace(inst: &Instance, now: chrono::DateTime<chrono::Utc>) -> bool {
    let Some(started) = inst.capture_started_at else {
        return false;
    };
    let started = chrono::DateTime::<chrono::Utc>::from(started);
    let grace = chrono::Duration::seconds(Instance::STARTUP_GRACE_SECS as i64);
    started <= now && now < started + grace
}

/// One tick's per-instance status decision: seed each freshly disk-loaded row's
/// live baseline from `prev`, then let tmux speak for the rows tmux owns.
///
/// Split out of `status_poll_loop` with [`observed_transitions`] so the two
/// halves stay testable as a pair. They are a pair by contract: this one decides
/// each row's status, that one reports which of those differ from `prev`. Fold
/// either back inline and the phantom-transition regression this guards (see
/// [`skip_tmux_decision_for_structured`]) loses its only coverage.
pub(super) fn apply_tick_status_decisions(
    instances: &mut [Instance],
    prev: &mut std::collections::HashMap<String, Status>,
    tracking: &std::collections::HashMap<String, PriorTickTracking>,
    suppressed_ids: &std::collections::HashSet<String>,
    pane_metadata: Option<&std::collections::HashMap<String, crate::tmux::PaneMetadata>>,
    sandbox_health: &std::collections::HashMap<String, SandboxHealth>,
) {
    let now = chrono::Utc::now();
    for inst in instances {
        let previous = prev.get_mut(&inst.id);
        let prior_tracking = tracking.get(&inst.id);
        let suppressed = suppressed_ids.contains(&inst.id);
        let baseline = apply_tick_status_decision(
            inst,
            previous.as_deref().copied(),
            prior_tracking,
            suppressed,
            pane_metadata,
            Some(sandbox_health),
            now,
        );
        if let (Some(previous), Some(baseline)) = (previous, baseline) {
            *previous = baseline;
        }
    }
}

fn apply_tick_status_decision(
    inst: &mut Instance,
    previous: Option<Status>,
    tracking: Option<&PriorTickTracking>,
    suppressed: bool,
    pane_metadata: Option<&std::collections::HashMap<String, crate::tmux::PaneMetadata>>,
    sandbox_health: Option<&std::collections::HashMap<String, SandboxHealth>>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<Status> {
    if inst.status == Status::Creating {
        return None;
    }
    if suppressed {
        inst.status = Status::Starting;
        return None;
    }
    let baseline = if inst.is_structured() {
        previous
    } else if let Some((status, prior)) = previous
        .zip(tracking)
        .filter(|(_, prior)| inst.lifecycle_generation <= prior.lifecycle_generation)
    {
        inst.idle_entered_at = prior.idle_entered_at;
        Some(status)
    } else {
        // A fresh observation stamps against this generation, not the old pair.
        previous.map(|_| inst.status)
    };
    inst.live_status_baseline = baseline;
    if inst.is_trashed() {
        if let Some(live) = baseline {
            inst.status = live;
        }
        return baseline;
    }
    if skip_tmux_decision_for_structured(inst) {
        return baseline;
    }
    if inst.has_fresh_lifecycle_reservation(now)
        || (inst.status == Status::Starting && starting_status_in_grace(inst, now))
    {
        return baseline;
    }
    if inst.is_sandboxed()
        && !matches!(
            inst.status,
            Status::Stopped | Status::Starting | Status::Creating | Status::Deleting
        )
        && inst.sandbox_info.as_ref().is_some_and(|sandbox| {
            sandbox_health
                .and_then(|health| health.get(&inst.id))
                .is_some_and(|health| {
                    health.generation == inst.lifecycle_generation
                        && health.container_name == sandbox.container_name
                        && !health.running
                })
        })
    {
        inst.status = Status::Error;
        inst.last_error = Some("Container is not running".into());
        inst.idle_entered_at = None;
        inst.pane_dead_observed = false;
        inst.live_status_baseline = Some(Status::Error);
        return Some(Status::Error);
    }
    let Some(pane_metadata) = pane_metadata else {
        if let Some(live) = baseline {
            inst.status = live;
        }
        return baseline;
    };
    let session_name = crate::tmux::resolve_agent_session_name_in(
        pane_metadata,
        &inst.id,
        &crate::tmux::Session::generate_name(&inst.id, &inst.title),
    );
    inst.update_status_with_metadata(pane_metadata.get(&session_name), Some(&session_name));
    baseline
}

fn supplementary_status_can_be_reused(row: &Instance, status: Status, generation: u64) -> bool {
    !row.is_structured()
        && row.lifecycle_generation <= generation
        && !matches!(
            row.status,
            Status::Stopped | Status::Creating | Status::Deleting
        )
        && !matches!(
            status,
            Status::Stopped | Status::Creating | Status::Deleting
        )
}

pub(super) type SupplementaryTick = std::collections::HashMap<
    ProfileIdentity,
    (
        std::collections::HashMap<String, Status>,
        std::collections::HashMap<String, PriorTickTracking>,
    ),
>;

pub(super) fn supplementary_tick_tracking(cache: &RuntimeReadCache) -> SupplementaryTick {
    let mut prior = SupplementaryTick::new();
    for row in &cache.alias_only_instances {
        if row.is_structured() {
            continue;
        }
        if let Some(profile) = cache
            .inventory
            .iter()
            .find(|profile| profile.name == row.source_profile)
        {
            let (statuses, tracking) = prior.entry(profile.identity).or_default();
            statuses.insert(row.id.clone(), row.status);
            tracking.insert(row.id.clone(), PriorTickTracking::of(row));
        }
    }
    prior
}

pub(super) fn apply_supplementary_tick(
    cache: &mut RuntimeReadCache,
    prior: &SupplementaryTick,
    pane_metadata: Option<&std::collections::HashMap<String, crate::tmux::PaneMetadata>>,
) {
    let now = chrono::Utc::now();
    for row in &mut cache.alias_only_instances {
        if row.is_structured() {
            continue;
        }
        let prior = cache
            .inventory
            .iter()
            .find(|profile| profile.name == row.source_profile)
            .and_then(|profile| prior.get(&profile.identity));
        let reusable = prior.filter(|(statuses, tracking)| {
            statuses
                .get(&row.id)
                .zip(tracking.get(&row.id))
                .is_some_and(|(&status, tracking)| {
                    supplementary_status_can_be_reused(row, status, tracking.lifecycle_generation)
                })
        });
        if let Some((statuses, tracking)) = reusable {
            row.status = statuses[&row.id];
            row.idle_entered_at = tracking[&row.id].idle_entered_at;
            seed_tick_tracking(std::slice::from_mut(row), tracking);
        }
        let previous = reusable.or_else(|| {
            prior.filter(|(_, tracking)| {
                tracking.get(&row.id).is_some_and(|tracking| {
                    row.lifecycle_generation > tracking.lifecycle_generation
                })
            })
        });
        let previous_status = previous
            .and_then(|(statuses, _)| statuses.get(&row.id))
            .copied();
        let previous_tracking = previous.and_then(|(_, tracking)| tracking.get(&row.id));
        apply_tick_status_decision(
            row,
            previous_status,
            previous_tracking,
            false,
            pane_metadata,
            None,
            now,
        );
    }
}

/// Observed status edges as (row index, effective baseline) pairs.
pub(super) fn observed_transitions(
    instances: &[Instance],
    prev: &std::collections::HashMap<String, crate::session::Status>,
) -> Vec<(usize, Status)> {
    instances
        .iter()
        .enumerate()
        .filter_map(|(idx, inst)| {
            let old = *prev.get(&inst.id)?;
            (old != inst.status).then_some((idx, old))
        })
        .collect()
}

/// Report whether the caller must skip the tmux status decision for this row,
/// carrying the acp-authoritative live status onto it when so.
///
/// A structured row has no tmux pane to probe, so the poller has no say in its
/// status: [`apply_acp_overlay_inplace`] re-pins the in-memory value on every
/// reload, and `decide_passive_transition` deliberately never persists the
/// poller's view (#2690 / #2697). Disk therefore stays permanently out of step
/// with live, and `status_poll_loop` compares exactly those two: `prev` comes
/// from `state.instances` (overlaid, live), `fresh` from disk. Left alone, every
/// tick reads that standing mismatch as a brand new transition, which
/// broadcasts a `StatusChange` (logged at publish time) and resets the push
/// dwell timer in `server::push`. Forever, at the 2s tick, surviving daemon
/// restarts because `seed_acp_statuses` re-derives the same live status from the
/// stored event log on boot. One session whose worker died with
/// `AgentStartupError` wrote 81k such lines into a single 43MB log file.
///
/// A phantom whose live side is `Running` costs one more: `mark_unread` in
/// `decide_passive_transition` is not gated on `is_structured`, so it re-marks
/// the row unread seconds after the user reads it. That one needs `old ==
/// Running` specifically, so the `AgentStartupError` case above never reached
/// it.
///
/// Aligning `status` with the baseline the caller just seeded makes that
/// comparison like-for-like, so a structured row reports a transition only when
/// its live status actually moved, which for these rows means an acp event
/// handler moved it.
///
/// Deliberately does not lean on the structured short-circuit in
/// `Instance::update_status_with_metadata_inner`: that path heals `Error` to
/// `Idle` unconditionally, which is correct for the TUI poller and `aoe ps`
/// (neither has an overlay to re-pin the value) but is what mints the phantom
/// here, since the overlay restores `Error` moments later.
pub(super) fn skip_tmux_decision_for_structured(inst: &mut Instance) -> bool {
    if !inst.is_structured() {
        return false;
    }
    if inst.status == crate::session::Status::Creating {
        return true;
    }
    inst.clear_stale_tmux_error();
    // `None` means the row is newer than the last tick and has no live value
    // yet; its disk status is all there is, and the absent baseline already
    // suppresses a transition report.
    if let Some(live) = inst.live_status_baseline {
        inst.status = live;
    }
    true
}

/// Logs health transitions only, so steady-state reloads stay silent.
async fn set_canonical_health(state: &AppState, to: crate::daemon::RuntimeHealth) {
    let mut health = state.canonical_health.write().await;
    if *health == to {
        return;
    }
    if to == crate::daemon::RuntimeHealth::Healthy {
        tracing::info!(target: "server.runtime", from = ?*health, to = ?to, "runtime health recovered");
    } else {
        tracing::warn!(target: "server.runtime", from = ?*health, to = ?to, "runtime health degraded");
    }
    *health = to;
}

// Both polling and disk notifications use this merge; polling remains authoritative.
// Preserve runtime-only fields per ID. DiskOnly also preserves live status and
// detection tracking; TmuxApplied keeps the newly sampled decision and tracking.
// Access timestamps are monotonic. ACP overlay eligibility follows is_structured(),
// not the lazily assigned ACP session ID. Keep prior_by_id intact for that overlay.
// Callers hold namespace and reload-lane guards and capture mutation_epoch before
// reading disk. Publication exclusion rejects stale reads and spans repair/passive
// commits through adoption. Failure retains the last complete canonical state.
// Status effects are dispatched only after successful adoption and publication.

/// Snapshot of the prior in-memory `state.instances` keyed by id, used
/// for per-id merging in `reload_state_instances_from_disk` and the
/// acp-overlay pass. Intentionally exposes no `remove` method, because
/// invariant 5 of the merge contract requires the same map to be
/// populated when `apply_acp_overlay_inplace` runs after the merge loop.
/// The compiler rejects any future `.remove()` call instead of relying on
/// prose.
pub(super) struct PriorById(std::collections::HashMap<String, Instance>);

impl PriorById {
    fn drain_from(current: &mut Vec<Instance>) -> Self {
        Self(
            current
                .drain(..)
                .map(|inst| (inst.id.clone(), inst))
                .collect(),
        )
    }

    fn get(&self, id: &str) -> Option<&Instance> {
        self.0.get(id)
    }

    fn values(&self) -> impl Iterator<Item = &Instance> {
        self.0.values()
    }
}

/// Rows another process changed on disk, as `(session, change)`. This daemon's
/// own commits update the in-memory rows first, so they produce no diff here.
fn external_session_changes<'a>(
    prior_by_id: &'a PriorById,
    merged: &'a [Instance],
) -> Vec<(&'a str, &'static str)> {
    let mut changes = Vec::new();
    for row in merged {
        let Some(prior) = prior_by_id.get(&row.id) else {
            changes.push((row.id.as_str(), "added"));
            continue;
        };
        let diffs = [
            (prior.title != row.title, "title"),
            (prior.group_path != row.group_path, "group"),
            (prior.is_archived() != row.is_archived(), "archived"),
            (prior.snoozed_until != row.snoozed_until, "snoozed"),
            (prior.trashed_at != row.trashed_at, "trashed"),
        ];
        changes.extend(
            diffs
                .into_iter()
                .filter(|(changed, _)| *changed)
                .map(|(_, change)| (row.id.as_str(), change)),
        );
    }
    let present: std::collections::HashSet<&str> = merged.iter().map(|r| r.id.as_str()).collect();
    changes.extend(
        prior_by_id
            .values()
            .filter(|prior| !present.contains(prior.id.as_str()))
            .map(|prior| (prior.id.as_str(), "removed")),
    );
    changes
}

pub(super) async fn reload_state_instances_from_disk(
    state: &Arc<AppState>,
    loaded: LoadedInstances,
    mut live_worker_records: Vec<LiveStructuredWorkerRecord>,
    status_source: StatusSource,
    read_epoch: u64,
    metadata: Result<CanonicalMetadata, ReloadFailure>,
    mut passive: std::collections::HashMap<String, super::status_poll::PassiveTransitionWrites>,
) -> Vec<super::push::StatusChange> {
    let _reload_guard = state.session_service.disk_reload_guard().await;
    let LoadedInstances {
        instances: mut fresh,
        mut cache,
    } = loaded;
    let (mut metadata, native_health, native_diagnostics) = match metadata {
        Ok(metadata) => (metadata, crate::daemon::RuntimeHealth::Healthy, Vec::new()),
        Err(error) => {
            tracing::warn!(target: "server.file_watch", %error, "native reload remains unavailable; accepting independent CLI components");
            live_worker_records.clear();
            passive.clear();
            let diagnostics = error.owner_diagnostics();
            (
                state.canonical_metadata.read().await.clone(),
                error.health,
                diagnostics,
            )
        }
    };
    // Keep suppression stable across the reload decision.
    let suppressed_ids =
        crate::session::recovery::snapshot_recently_restarted(&state.recently_restarted);
    // Keep view-transition ownership through the durable repair and publication.
    // A busy enable/disable owns the desired view and must not be repaired.
    let terminal_ids: std::collections::HashSet<&str> = if live_worker_records.is_empty() {
        std::collections::HashSet::new()
    } else {
        fresh
            .iter()
            .filter(|row| !row.is_structured())
            .map(|row| row.id.as_str())
            .collect()
    };
    let mut repair_records = Vec::new();
    let mut repair_guards = Vec::new();
    for (record, _) in live_worker_records {
        if !terminal_ids.contains(record.session_id.as_str()) {
            continue;
        }
        let lock = state.instance_lock(&record.session_id).await;
        let Ok(guard) = lock.try_lock_owned() else {
            continue;
        };
        // The caller may have sampled this runner before disable finished.
        // Revalidate its owner under the transition lock, using its latest
        // stored conversation id rather than the earlier registry snapshot.
        let Ok(Some(current)) = crate::process::worker_registry::load(&record.session_id) else {
            continue;
        };
        if current.pid != record.pid
            || current.started_at != record.started_at
            || !crate::process::worker_registry::is_record_live(&current)
        {
            continue;
        }
        let Some(acp_session_id) = current
            .stored_acp_session_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
        else {
            continue;
        };
        repair_records.push((current, acp_session_id));
        repair_guards.push(guard);
    }

    let transition = if repair_records.is_empty() && passive.is_empty() {
        None
    } else {
        match tokio::task::spawn_blocking(crate::session::StorageTransition::acquire)
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
        {
            Ok(transition) => Some(Arc::new(transition)),
            Err(error) => {
                tracing::warn!(target: "server.file_watch", %error, "store transition unavailable");
                state
                    .mark_reload_failure(crate::daemon::RuntimeHealth::Degraded {
                        code: crate::daemon::ReloadFailureCode::ProfileData,
                        profiles: metadata
                            .profiles
                            .iter()
                            .map(|profile| profile.name.clone())
                            .collect(),
                    })
                    .await;
                return Vec::new();
            }
        }
    };
    let _publication = state.publication.write().await;
    let mut current = state.instances.write().await;

    // Reject reads that predate an in-memory row change.
    let current_epoch = state
        .mutation_epoch
        .load(std::sync::atomic::Ordering::SeqCst);
    if current_epoch != read_epoch {
        tracing::debug!(
            target: "server.file_watch",
            read_epoch,
            current_epoch,
            "dropping a disk reload whose snapshot predates an in-memory session mutation"
        );
        return Vec::new();
    }
    // Failed enumeration is unknown, not empty.
    if cache.health.enumeration_failed {
        state
            .runtime_read_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .health
            .enumeration_failed = true;
        drop(current);
        if matches!(
            &native_health,
            crate::daemon::RuntimeHealth::Degraded { .. }
        ) {
            set_canonical_health(state, native_health).await;
            *state
                .canonical_diagnostics
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = native_diagnostics;
        }
        state.runtime.request_publish();
        return Vec::new();
    }

    if let Some(transition) = transition.as_ref() {
        let repairs = repair_structured_rows_from_live_workers(&mut fresh, repair_records);
        if let Err(error) = persist_structured_row_repairs(
            state,
            repairs,
            &mut fresh,
            &mut metadata,
            transition,
            &_publication,
        )
        .await
        {
            tracing::warn!(target: "server.file_watch", error = ?error, "retaining last complete state after repair failure");
            let diagnostics = error.owner_diagnostics();
            set_canonical_health(state, error.health).await;
            *state
                .canonical_diagnostics
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = diagnostics;
            state.runtime.request_publish();
            return Vec::new();
        }
        if let Err(error) = super::status_poll::flush_passive_transition_writes(
            state.file_watch.clone(),
            &mut fresh,
            &mut metadata,
            passive,
            transition,
            &_publication,
        )
        .await
        {
            let diagnostics = error.owner_diagnostics();
            set_canonical_health(state, error.health).await;
            *state
                .canonical_diagnostics
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = diagnostics;
            state.runtime.request_publish();
            return Vec::new();
        }
    }

    let changes = merge_loaded_rows(&mut current, fresh, status_source, &suppressed_ids);
    {
        let mut previous_cache = state
            .runtime_read_cache
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        merge_supplementary_cache(&mut previous_cache, &mut cache, status_source);
        *previous_cache = cache;
    }
    drop(current);
    *state.canonical_metadata.write().await = metadata;
    set_canonical_health(state, native_health).await;
    *state
        .canonical_diagnostics
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = native_diagnostics;
    state.runtime.request_publish();
    for change in &changes {
        publish(&state.status_tx, change.clone());
    }
    changes
}

fn merge_supplementary_cache(
    previous: &mut RuntimeReadCache,
    cache: &mut RuntimeReadCache,
    status_source: StatusSource,
) {
    let previous_rows = std::mem::take(&mut previous.alias_only_instances);
    let mut supplementary_prior: std::collections::HashMap<
        ProfileIdentity,
        std::collections::HashMap<String, Instance>,
    > = std::collections::HashMap::new();
    for row in previous_rows {
        if let Some(profile) = previous
            .inventory
            .iter()
            .find(|profile| profile.name == row.source_profile)
        {
            supplementary_prior
                .entry(profile.identity)
                .or_default()
                .insert(row.id.clone(), row);
        }
    }
    for row in &mut cache.alias_only_instances {
        if row.is_structured() {
            continue;
        }
        let Some(profile) = cache
            .inventory
            .iter()
            .find(|profile| profile.name == row.source_profile)
        else {
            continue;
        };
        let Some(prior) = supplementary_prior
            .get_mut(&profile.identity)
            .and_then(|rows| rows.remove(&row.id))
        else {
            continue;
        };
        let prior_status = prior.status;
        let prior_idle = prior.idle_entered_at;
        let prior_accessed = prior.last_accessed_at;
        let tracking = PriorTickTracking::of(&prior);
        row.last_error_check = prior.last_error_check;
        row.last_start_time = prior.last_start_time;
        if row.status == Status::Error {
            row.last_error = prior.last_error;
        }
        row.acp_load_session_capable = prior.acp_load_session_capable;
        row.plugin_revival_pending = prior.plugin_revival_pending;
        if matches!(status_source, StatusSource::DiskOnly)
            && supplementary_status_can_be_reused(row, prior_status, tracking.lifecycle_generation)
        {
            row.status = prior_status;
            row.idle_entered_at = prior_idle;
            row.ever_confirmed_present = tracking.ever_confirmed_present;
            row.unknown_since = tracking.unknown_since;
            row.detection = tracking.detection;
        }
        row.last_accessed_at = prior_accessed.max(row.last_accessed_at);
    }
}

pub(super) fn merge_loaded_rows(
    current: &mut Vec<Instance>,
    fresh: Vec<Instance>,
    status_source: StatusSource,
    suppressed_ids: &std::collections::HashSet<String>,
) -> Vec<super::push::StatusChange> {
    let prior_by_id = PriorById::drain_from(current);
    let mut merged = Vec::with_capacity(fresh.len());
    for mut row in fresh {
        let creating = row.status == crate::session::Status::Creating;
        if let Some(mut prior) = prior_by_id.get(&row.id).cloned() {
            let prior_status = prior.status;
            let prior_last_accessed = prior.last_accessed_at;
            let prior_idle_entered = prior.idle_entered_at;
            let prior_generation = prior.lifecycle_generation;
            if matches!(status_source, StatusSource::TmuxApplied) && !row.is_structured() {
                merge_process_runtime_fields(prior, &mut row);
            } else {
                if matches!(status_source, StatusSource::TmuxApplied) {
                    prior.agent_pane = std::mem::take(&mut row.agent_pane);
                    prior.auxiliary = std::mem::take(&mut row.auxiliary);
                }
                merge_runtime_fields(prior, &mut row);
            }
            if !creating && matches!(status_source, StatusSource::DiskOnly) {
                let purge_in_flight = prior_status == Status::Deleting
                    && row.lifecycle_reservation_is_owned(
                        crate::session::LifecycleOperation::Purge,
                        row.lifecycle_generation,
                    );
                if row.is_structured()
                    || row.lifecycle_generation <= prior_generation
                    || purge_in_flight
                {
                    row.status = prior_status;
                    row.idle_entered_at = prior_idle_entered;
                }
            }
            row.last_accessed_at = prior_last_accessed.max(row.last_accessed_at);
        }
        if creating {
            row.status = crate::session::Status::Creating;
        } else if suppressed_ids.contains(&row.id) {
            row.status = Status::Starting;
        }
        merged.push(row);
    }
    apply_acp_overlay_inplace(&prior_by_id, &mut merged);
    if matches!(status_source, StatusSource::DiskOnly) {
        for (session, change) in external_session_changes(&prior_by_id, &merged) {
            tracing::info!(
                target: "session.sync",
                session,
                change,
                source = "disk",
                "session changed on disk"
            );
        }
    }
    let mut changes = Vec::new();
    let now = chrono::Utc::now();
    for row in &mut merged {
        if row.is_archived() {
            row.settle_archived_status();
        }
        if !row.is_structured() {
            if let Some(prior) = prior_by_id.get(&row.id).filter(|prior| {
                prior.status != row.status
                    && (matches!(status_source, StatusSource::TmuxApplied)
                        || (row.lifecycle_generation > prior.lifecycle_generation
                            && !suppressed_ids.contains(&row.id)))
            }) {
                changes.push(super::push::StatusChange {
                    instance_id: row.id.clone(),
                    instance_title: row.title.clone(),
                    effective_profile: row.effective_profile(),
                    old: prior.status,
                    new: row.status,
                    at: now,
                });
            }
        }
    }

    *current = merged;
    changes
}

/// Apply the acp status / timestamps overlay to `merged`, sourcing
/// values from `prior_by_id`. The merge loop above uses `.get()` (NOT
/// `.remove()`), so this lookup still finds entries here. Filter is
/// `inst.is_structured()` per the invariant above; filtering on
/// the lazy session id would silently drop overlay coverage for
/// pre-handshake rows.
///
/// ## Durability contract (#2690 follow-up)
///
/// Structured rows accept a soft reset of `status` / `last_accessed_at` /
/// `idle_entered_at` on daemon restart, by contract. The values written
/// here come from `prior_by_id`, an in-memory snapshot that the daemon
/// rebuilds each tick from live worker state, and never flow through
/// [`crate::session::PassiveStatusPatch`] (`decide_passive_transition`
/// returns `patch: None` for `is_structured()` rows). After a daemon
/// restart, disk-loaded structured rows read whatever was durably
/// persisted last (initial creation, or an explicit user action). ACP
/// event handlers are responsible for any post-restart re-emission that
/// updates these fields for structured sessions; the passive-status
/// writer at `status_poll_loop` deliberately does not.
pub(super) fn apply_acp_overlay_inplace(prior_by_id: &PriorById, merged: &mut [Instance]) {
    for inst in merged.iter_mut() {
        if !inst.is_structured() || inst.status == crate::session::Status::Creating {
            continue;
        }
        let Some(prior) = prior_by_id.get(&inst.id) else {
            continue;
        };
        inst.status = prior.status;
        inst.last_accessed_at = prior.last_accessed_at;
        inst.idle_entered_at = prior.idle_entered_at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn external_session_changes_reports_added_removed_and_edited_rows() {
        let kept = Instance::new("kept", "/tmp/kept");
        let gone = Instance::new("gone", "/tmp/gone");
        let added = Instance::new("added", "/tmp/added");
        let prior = PriorById::drain_from(&mut vec![kept.clone(), gone.clone()]);
        assert!(external_session_changes(&prior, &[gone.clone(), kept.clone()]).is_empty());

        let mut edited = kept.clone();
        edited.title = "renamed".into();
        edited.group_path = "team/b".into();
        edited.archive();
        edited.trashed_at = Some(Utc::now());
        let mut snoozed = gone.clone();
        snoozed.snoozed_until = Some(Utc::now());
        let merged = vec![edited, added.clone(), snoozed];
        assert_eq!(
            external_session_changes(&prior, &merged),
            vec![
                (kept.id.as_str(), "title"),
                (kept.id.as_str(), "group"),
                (kept.id.as_str(), "archived"),
                (kept.id.as_str(), "trashed"),
                (added.id.as_str(), "added"),
                (gone.id.as_str(), "snoozed"),
            ]
        );
        assert_eq!(
            external_session_changes(&prior, std::slice::from_ref(&kept)),
            vec![(gone.id.as_str(), "removed")]
        );
    }

    #[test]
    fn committed_profile_batch_preserves_moved_runtime_and_rejects_partial_metadata() {
        for missing in [None, Some("source"), Some("target")] {
            let mut moving = Instance::new("moving", "/tmp/moving");
            moving.source_profile = "source".into();
            moving.status = Status::Running;
            let mut source_peer = Instance::new("source peer", "/tmp/source-peer");
            source_peer.source_profile = "source".into();
            let mut target_peer = Instance::new("target peer", "/tmp/target-peer");
            target_peer.source_profile = "target".into();
            target_peer.status = Status::Running;
            let mut unrelated = Instance::new("unrelated", "/tmp/unrelated");
            unrelated.source_profile = "other".into();
            let mut current = vec![
                moving.clone(),
                unrelated.clone(),
                target_peer.clone(),
                source_peer.clone(),
            ];
            let mut metadata = CanonicalMetadata {
                default_profile: "source".into(),
                profiles: ["source", "target", "other"]
                    .into_iter()
                    .filter(|name| Some(*name) != missing)
                    .map(|name| crate::daemon::ProfileSnapshot {
                        name: name.into(),
                        description: Some(format!("{name} description")),
                        groups: vec![crate::session::Group::new("old", "old")],
                        projects: Vec::new(),
                    })
                    .collect(),
                workspace_ordering: vec!["unrelated ordering".into()],
                global_projects: Vec::new(),
                status_hooks: Default::default(),
                auxiliary_tools: Default::default(),
                raw_tool_names: Default::default(),
            };
            let original_rows = serde_json::to_value(&current).unwrap();
            let original_metadata = metadata.clone();
            let mut moved = moving.clone();
            moved.title = "renamed".into();
            moved.group_path = "destination".into();
            moved.status = Status::Idle;
            let mut source_after = source_peer.clone();
            source_after.title = "source peer committed".into();
            let mut target_after = target_peer.clone();
            target_after.title = "target peer committed".into();
            target_after.status = Status::Stopped;
            let source_groups = vec![crate::session::Group::new("empty", "empty")];
            let mut destination = crate::session::Group::new("destination", "destination");
            destination.collapsed = true;
            let target_groups = vec![destination];
            let committed = [
                ("target", vec![target_after, moved], target_groups.clone()),
                ("source", vec![source_after], source_groups.clone()),
            ];
            let result = replace_committed_profiles(&mut current, &mut metadata, committed, |id| {
                (id == target_peer.id).then_some(StatusCommit::Lifecycle)
            });
            if missing.is_some() {
                assert!(result.is_err());
                assert_eq!(serde_json::to_value(&current).unwrap(), original_rows);
                assert_eq!(metadata, original_metadata);
                continue;
            }
            result.unwrap();
            assert_eq!(
                current
                    .iter()
                    .map(|row| row.id.as_str())
                    .collect::<Vec<_>>(),
                [
                    source_peer.id.as_str(),
                    unrelated.id.as_str(),
                    target_peer.id.as_str(),
                    moving.id.as_str()
                ]
            );
            assert_eq!(current[0].title, "source peer committed");
            assert_eq!(current[2].title, "target peer committed");
            assert_eq!(current[2].status, Status::Stopped);
            assert_eq!(current[3].title, "renamed");
            assert_eq!(current[3].group_path, "destination");
            assert_eq!(current[3].source_profile, "target");
            assert_eq!(current[3].status, Status::Running);
            assert_eq!(metadata.profiles[0].groups, source_groups);
            assert_eq!(metadata.profiles[1].groups, target_groups);
            assert_eq!(metadata.profiles[2], original_metadata.profiles[2]);
            assert_eq!(
                metadata.workspace_ordering,
                original_metadata.workspace_ordering
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn global_reload_rejects_partial_profile_data_and_recovers() {
        let _home = crate::session::test_support::isolate_app_dir();
        let first = Storage::new_unwatched("first").unwrap();
        let second = Storage::new_unwatched("second").unwrap();
        for storage in [&first, &second] {
            storage
                .update(|rows, _| {
                    rows.push(Instance::new(storage.profile(), "/tmp/repo"));
                    Ok(())
                })
                .unwrap();
        }
        let intact = std::fs::read(second.sessions_path()).unwrap();
        let mut partial: Vec<serde_json::Value> = serde_json::from_slice(&intact).unwrap();
        partial.push(serde_json::json!({"id": 5}));
        for damaged in [b"{".to_vec(), serde_json::to_vec(&partial).unwrap()] {
            std::fs::write(second.sessions_path(), &damaged).unwrap();
            assert!(load_all_profiles(&FileWatchService::noop()).is_err());
            assert_eq!(std::fs::read(second.sessions_path()).unwrap(), damaged);
            assert!(!second
                .sessions_path()
                .with_file_name("sessions.corrupt.jsonl")
                .exists());
        }
        std::fs::write(second.sessions_path(), intact).unwrap();
        let groups = second.sessions_path().with_file_name("groups.json");
        std::fs::write(&groups, br#"[{"path":5}]"#).unwrap();
        assert!(load_all_profiles(&FileWatchService::noop()).is_err());
        assert!(!groups.with_file_name("groups.corrupt.jsonl").exists());
        std::fs::remove_file(groups).unwrap();
        let config = crate::session::get_app_dir().unwrap().join("config.toml");
        std::fs::write(&config, "[invalid").unwrap();
        let error = load_all_profiles(&FileWatchService::noop())
            .err()
            .expect("invalid global config must keep the runtime degraded");
        assert_eq!(
            error.health,
            crate::daemon::RuntimeHealth::Degraded {
                code: crate::daemon::ReloadFailureCode::Metadata,
                profiles: Vec::new(),
            }
        );
        std::fs::remove_file(config).unwrap();
        let rows = load_all_profiles(&FileWatchService::noop()).unwrap();
        let mut profiles: Vec<_> = rows
            .instances
            .iter()
            .map(|row| row.source_profile.as_str())
            .collect();
        profiles.sort_unstable();
        assert_eq!(profiles, ["first", "second"]);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn canonical_reload_and_failed_probes_keep_lifecycle_status_pairs() {
        let old_idle = "2026-01-01T00:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();
        let disk_idle = "2026-01-02T00:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();
        let cases = [
            (
                3,
                Status::Running,
                None,
                Status::Idle,
                Some(disk_idle),
                Status::Running,
                None,
            ),
            (
                3,
                Status::Idle,
                Some(old_idle),
                Status::Running,
                None,
                Status::Idle,
                Some(old_idle),
            ),
            (
                4,
                Status::Idle,
                Some(old_idle),
                Status::Stopped,
                None,
                Status::Stopped,
                None,
            ),
            (
                4,
                Status::Running,
                None,
                Status::Idle,
                Some(disk_idle),
                Status::Idle,
                Some(disk_idle),
            ),
        ];
        for source in [StatusSource::DiskOnly, StatusSource::TmuxApplied] {
            for (
                generation,
                prior_status,
                prior_idle,
                disk_status,
                fresh_idle,
                expected_status,
                expected_idle,
            ) in cases
            {
                let _home = crate::session::test_support::isolate_app_dir();
                let mut prior = Instance::new("lifecycle", "/repo");
                prior.source_profile = "main".into();
                prior.lifecycle_generation = 3;
                prior.status = prior_status;
                prior.idle_entered_at = prior_idle;
                prior.ever_confirmed_present = true;
                let id = prior.id.clone();
                let mut statuses = std::collections::HashMap::from([(id.clone(), prior.status)]);
                let tracking =
                    std::collections::HashMap::from([(id.clone(), PriorTickTracking::of(&prior))]);
                let state = crate::server::test_support::build_test_app_state(vec![prior.clone()]);
                let mut status_rx = state.status_tx.subscribe();
                let storage = Storage::new_unwatched("main").unwrap();
                let mut fresh = prior;
                fresh.lifecycle_generation = generation;
                fresh.status = disk_status;
                fresh.idle_entered_at = fresh_idle;
                storage
                    .update(|rows, _| {
                        *rows = vec![fresh];
                        Ok(())
                    })
                    .unwrap();
                let epoch = state
                    .mutation_epoch
                    .load(std::sync::atomic::Ordering::SeqCst);
                let (mut loaded, loaded_metadata) =
                    crate::server::reload::load_profiles_for_reload(&state.file_watch);
                if matches!(source, StatusSource::TmuxApplied) {
                    seed_tick_tracking(&mut loaded.instances, &tracking);
                    apply_tick_status_decisions(
                        &mut loaded.instances,
                        &mut statuses,
                        &tracking,
                        &std::collections::HashSet::new(),
                        None,
                        &Default::default(),
                    );
                    let row = loaded.instances.iter().find(|row| row.id == id).unwrap();
                    assert_eq!(
                        (row.status, row.idle_entered_at),
                        (expected_status, expected_idle),
                        "failed probe, generation {generation}"
                    );
                    assert!(
                        observed_transitions(&loaded.instances, &statuses).is_empty(),
                        "a failed probe is not a passive transition across lifecycles"
                    );
                }
                reload_state_instances_from_disk(
                    &state,
                    loaded,
                    vec![],
                    source,
                    epoch,
                    loaded_metadata,
                    Default::default(),
                )
                .await;
                let rows = state.instances.read().await;
                let row = rows.iter().find(|row| row.id == id).unwrap();
                assert_eq!(row.lifecycle_generation, generation);
                assert_eq!(
                    (row.status, row.idle_entered_at),
                    (expected_status, expected_idle),
                    "generation {generation}"
                );
                assert!(
                    row.ever_confirmed_present,
                    "runtime reachability tracking is intentionally retained"
                );
                if prior_status != expected_status {
                    let change = status_rx
                        .try_recv()
                        .expect("the accepted lifecycle move is published");
                    assert_eq!(change.instance_id, id);
                    assert_eq!(change.effective_profile, "main");
                    assert_eq!((change.old, change.new), (prior_status, expected_status));
                }
                assert!(matches!(
                    status_rx.try_recv(),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                ));
            }
        }
    }
    fn terminal_row(status: Status) -> Instance {
        let mut row = Instance::new("reload-publish", "/tmp/aoe-reload-publish");
        row.source_profile = "default".to_string();
        row.tool = "claude".to_string();
        row.status = status;
        row
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_status_commits_publish_once_and_rejections_publish_nothing() {
        use crate::session::SessionStore;
        let _home = crate::session::test_support::isolate_app_dir();
        let row = terminal_row(Status::Running);
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let mut rx = state.status_tx.subscribe();
        let id = row.id.clone();
        let worker_state = state.clone();
        tokio::task::spawn_blocking(move || {
            let native = crate::server::session_store::NativeSessionStore::open(
                worker_state,
                "default",
                Some(id.clone()),
            )
            .unwrap();
            let store: &dyn SessionStore = &native;
            for status in [Status::Starting, Status::Error, Status::Error] {
                store
                    .update(|rows, _| {
                        rows.iter_mut().find(|row| row.id == id).unwrap().status = status;
                        Ok(())
                    })
                    .unwrap();
            }
            let rejected: anyhow::Result<()> = store.update(|rows, _| {
                rows.iter_mut().find(|row| row.id == id).unwrap().status = Status::Idle;
                anyhow::bail!("reject before commit")
            });
            assert!(rejected.is_err());
        })
        .await
        .unwrap();
        for edge in [
            (Status::Running, Status::Starting),
            (Status::Starting, Status::Error),
        ] {
            let change = rx.try_recv().expect("each committed edge is published");
            assert_eq!((change.old, change.new), edge);
            assert_eq!(change.effective_profile, "default");
        }
        assert!(rx.try_recv().is_err(), "no-op and rejection emit nothing");
        assert_eq!(state.instances.read().await[0].status, Status::Error);
        assert_eq!(
            crate::server::test_support::load_instances_from_disk_for_test("default")[0].status,
            Status::Error
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_reload_publishes_the_status_move_it_applies() {
        let _home = crate::session::test_support::isolate_app_dir();
        // (live status, status in the snapshot, source, expected move)
        let cases = [
            (
                Status::Running,
                Status::Error,
                StatusSource::TmuxApplied,
                Some((Status::Running, Status::Error)),
            ),
            (
                Status::Starting,
                Status::Error,
                StatusSource::TmuxApplied,
                Some((Status::Starting, Status::Error)),
            ),
            (Status::Idle, Status::Idle, StatusSource::TmuxApplied, None),
            // Same-generation disk reloads retain the live status.
            (
                Status::Running,
                Status::Stopped,
                StatusSource::DiskOnly,
                None,
            ),
        ];
        for (live, snapshot, source, want) in cases {
            let row = terminal_row(live);
            let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
            let mut rx = state.status_tx.subscribe();
            let mut fresh = row.clone();
            fresh.status = snapshot;
            let read_epoch = state
                .mutation_epoch
                .load(std::sync::atomic::Ordering::SeqCst);

            let metadata = state.canonical_metadata.read().await.clone();
            reload_state_instances_from_disk(
                &state,
                (vec![fresh]).into(),
                Vec::new(),
                source,
                read_epoch,
                Ok(metadata),
                Default::default(),
            )
            .await;

            let got = rx.try_recv().ok().map(|c| (c.old, c.new));
            assert_eq!(got, want, "live={live:?} snapshot={snapshot:?} {source:?}");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_rejected_snapshot_publishes_nothing() {
        let _home = crate::session::test_support::isolate_app_dir();
        let row = terminal_row(Status::Running);
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        let mut rx = state.status_tx.subscribe();
        let read_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        state
            .mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut fresh = row.clone();
        fresh.status = Status::Error;

        let metadata = state.canonical_metadata.read().await.clone();
        reload_state_instances_from_disk(
            &state,
            vec![fresh].into(),
            Vec::new(),
            StatusSource::TmuxApplied,
            read_epoch,
            Ok(metadata),
            Default::default(),
        )
        .await;

        assert!(rx.try_recv().is_err());
        assert_eq!(state.instances.read().await[0].status, Status::Running);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_stale_reload_cannot_undo_or_repeat_an_acknowledged_archive_move() {
        let _home = crate::session::test_support::isolate_app_dir();
        let row = terminal_row(Status::Running);
        let id = row.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let mut rx = state.status_tx.subscribe();
        // Capture the poll's Running observation before the archive commits Idle.
        let fresh = row.clone();
        let read_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let metadata = state.canonical_metadata.read().await.clone();

        let response = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::server::api::sessions::update_session_archive(
                axum::extract::State(Arc::clone(&state)),
                axum::extract::Path(id),
                Ok(axum::Json(crate::daemon::UpdateArchiveBody {
                    archived: true,
                    kill_pane: false,
                })),
            ),
        )
        .await
        .expect("archive must acknowledge without a test-held reload gate");
        assert_eq!(
            axum::response::IntoResponse::into_response(response).status(),
            axum::http::StatusCode::OK,
        );
        assert!(
            state
                .mutation_epoch
                .load(std::sync::atomic::Ordering::SeqCst)
                > read_epoch,
            "acknowledged archive must invalidate the captured snapshot",
        );

        let first = rx.try_recv().expect("the handler published its move");
        assert_eq!((first.old, first.new), (Status::Running, Status::Idle));
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            reload_state_instances_from_disk(
                &state,
                vec![fresh].into(),
                Vec::new(),
                StatusSource::TmuxApplied,
                read_epoch,
                Ok(metadata),
                Default::default(),
            ),
        )
        .await
        .expect("stale snapshot rejection must complete");
        let current = state.instances.read().await;
        assert_eq!(
            current[0].status,
            Status::Idle,
            "stale Running cannot undo archive"
        );
        assert!(current[0].archived_at.is_some());
        assert!(rx.try_recv().is_err(), "the reload must not repeat it");
    }

    fn live_repair_fixture() -> (Arc<AppState>, Instance, LiveStructuredWorkerRecord, Storage) {
        let mut row = Instance::new("repair-transition", "/tmp/repo");
        row.source_profile = "repair-transition".into();
        let storage = Storage::new_unwatched(&row.source_profile).unwrap();
        storage
            .update(|instances, _| {
                instances.push(row.clone());
                Ok(())
            })
            .unwrap();
        let socket = crate::process::worker_registry::socket_path_for(&row.id).unwrap();
        crate::process::worker_registry::touch_live_socket(&socket);
        let record = crate::process::worker_registry::WorkerRecord::new(
            row.id.clone(),
            std::process::id(),
            socket,
            "codex-acp".into(),
            "codex".into(),
            "/tmp/repo".into(),
            None,
            vec![],
            vec![],
            Some("agent-session".into()),
            Some(row.source_profile.clone()),
        );
        crate::process::worker_registry::save(&record).unwrap();
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        *state.canonical_metadata.try_write().unwrap() =
            load_all_profiles(&state.file_watch).unwrap().metadata;
        (state, row, (record, "agent-session".into()), storage)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reload_repair_does_not_undo_terminal_transition_during_teardown() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (state, row, record, storage) = live_repair_fixture();
        // Disable committed terminal view, but session/delete is still running.
        let lock = state.instance_lock(&row.id).await;
        let _transition = lock.lock().await;
        state
            .mutation_epoch
            .store(1, std::sync::atomic::Ordering::SeqCst);
        reload_state_instances_from_disk(
            &state,
            vec![row].into(),
            vec![record],
            StatusSource::DiskOnly,
            1,
            Ok({
                let metadata = state.canonical_metadata.read().await.clone();
                metadata
            }),
            Default::default(),
        )
        .await;
        assert!(!state.instances.read().await[0].is_structured());
        assert!(!storage.load().unwrap()[0].is_structured());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reload_repair_rejects_a_registry_sample_retired_after_the_disk_read() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (state, row, record, storage) = live_repair_fixture();
        // The reload sampled the runner during teardown; disable finished
        // before this reload could acquire the transition lock.
        crate::process::worker_registry::delete_if_owned(&row.id, std::process::id()).unwrap();
        reload_state_instances_from_disk(
            &state,
            vec![row].into(),
            vec![record],
            StatusSource::DiskOnly,
            0,
            Ok({
                let metadata = state.canonical_metadata.read().await.clone();
                metadata
            }),
            Default::default(),
        )
        .await;
        assert!(!state.instances.read().await[0].is_structured());
        assert!(!storage.load().unwrap()[0].is_structured());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reload_repair_commits_before_a_following_terminal_transition() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (state, row, record, storage) = live_repair_fixture();
        let lock = state.instance_lock(&row.id).await;
        reload_state_instances_from_disk(
            &state,
            vec![row.clone()].into(),
            vec![record],
            StatusSource::DiskOnly,
            0,
            Ok({
                let metadata = state.canonical_metadata.read().await.clone();
                metadata
            }),
            Default::default(),
        )
        .await;
        assert!(state.instances.read().await[0].is_structured());
        // The following view transition must observe the durable repair,
        // rather than race an older queued structured write.
        let _transition = tokio::time::timeout(std::time::Duration::from_secs(2), lock.lock())
            .await
            .expect("repair persistence must finish");
        assert!(storage.load().unwrap()[0].is_structured());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn repair_publishes_peer_rows_and_groups_from_its_storage_commit() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (state, row, record, storage) = live_repair_fixture();
        let mut frozen = Instance::new("peer-archived", "/tmp/frozen");
        frozen.status = Status::Waiting;
        let frozen_id = frozen.id.clone();
        storage
            .update(|rows, _| {
                rows.push(frozen);
                Ok(())
            })
            .unwrap();
        let sampled = load_all_profiles(&state.file_watch).unwrap();
        let peer = Instance::new("peer-created", "/tmp/peer");
        let peer_id = peer.id.clone();
        storage
            .update(|rows, groups| {
                rows[0].title = "peer-renamed".into();
                rows.push(peer);
                rows.iter_mut()
                    .find(|item| item.id == frozen_id)
                    .unwrap()
                    .archive();
                let mut group = crate::session::Group::new("group", "peer/group");
                group.collapsed = true;
                groups.push(group);
                Ok(())
            })
            .unwrap();
        reload_state_instances_from_disk(
            &state,
            (sampled.instances).into(),
            vec![record],
            StatusSource::TmuxApplied,
            0,
            Ok(sampled.metadata),
            Default::default(),
        )
        .await;
        let snapshot = state.runtime.publish(&state).await.unwrap();
        let rows = &snapshot.value.contents.sessions;
        assert_eq!(
            rows.iter().find(|item| item.id == row.id).unwrap().title,
            "peer-renamed"
        );
        assert_eq!(
            rows.iter().find(|item| item.id == peer_id).unwrap().title,
            "peer-created"
        );
        let frozen = rows.iter().find(|item| item.id == frozen_id).unwrap();
        assert!(frozen.archived_at.is_some());
        assert_eq!(frozen.status, "Idle");
        let profile = snapshot
            .value
            .contents
            .profiles
            .iter()
            .find(|profile| profile.name == row.source_profile)
            .unwrap();
        assert!(profile
            .groups
            .iter()
            .any(|group| group.path == "peer/group" && group.collapsed));
        assert!(storage
            .load()
            .unwrap()
            .iter()
            .find(|item| item.id == row.id)
            .unwrap()
            .is_structured());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn failed_repair_retains_bundle_until_complete_recovery() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (state, row, record, storage) = live_repair_fixture();
        let metadata = load_all_profiles(&state.file_watch).unwrap().metadata;
        *state.canonical_metadata.write().await = metadata.clone();
        let intact = std::fs::read(storage.sessions_path()).unwrap();
        std::fs::write(storage.sessions_path(), b"{").unwrap();
        let mut stale_sample = row.clone();
        stale_sample.title = "uncommitted title".into();
        reload_state_instances_from_disk(
            &state,
            (vec![stale_sample]).into(),
            vec![record.clone()],
            StatusSource::DiskOnly,
            0,
            Ok(metadata.clone()),
            Default::default(),
        )
        .await;
        assert_eq!(
            *state.canonical_health.read().await,
            crate::daemon::RuntimeHealth::Degraded {
                code: crate::daemon::ReloadFailureCode::ProfileData,
                profiles: vec![row.source_profile.clone()],
            }
        );
        {
            let retained = state.instances.read().await;
            assert_eq!(retained[0].title, row.title);
            assert_eq!(retained[0].agent_name, row.agent_name);
            assert!(!retained[0].is_structured());
        }
        assert_eq!(*state.canonical_metadata.read().await, metadata);
        std::fs::write(storage.sessions_path(), intact).unwrap();
        let loaded = load_all_profiles(&state.file_watch).unwrap();
        reload_state_instances_from_disk(
            &state,
            (loaded.instances).into(),
            vec![record],
            StatusSource::DiskOnly,
            0,
            Ok(loaded.metadata),
            Default::default(),
        )
        .await;
        assert_eq!(
            *state.canonical_health.read().await,
            crate::daemon::RuntimeHealth::Healthy
        );
        assert!(state.instances.read().await[0].is_structured());
        assert!(storage.load().unwrap()[0].is_structured());
    }

    /// A structured row as the poll loop finds it mid-phantom: disk says `Idle`,
    /// the live acp status is `Error` because the worker died with
    /// `AgentStartupError` and `seed_acp_statuses` re-derives that on every boot.
    fn phantom_structured_row(id: &str) -> Instance {
        let mut inst = Instance::new(id, "/tmp/test");
        inst.view = crate::session::View::Structured;
        inst.status = Status::Idle;
        inst
    }

    #[test]
    fn skip_tmux_decision_for_structured_suppresses_the_phantom_transition() {
        // The other half of #2690 / #2697. That pair stopped the poller from
        // *persisting* its (void) view of a structured row's status, but left
        // `status_poll_loop` still comparing the live `prev` against the
        // disk-loaded `fresh`. Those two never converge for a structured row,
        // so the loop reported one fresh transition per 2s tick forever: a
        // `session.status_change` line, a `StatusChange` broadcast, and a reset
        // push dwell timer, plus a re-marked-unread row when the live side was
        // `Running`.
        let mut inst = phantom_structured_row("acp-session");
        inst.live_status_baseline = Some(Status::Error);

        assert!(
            skip_tmux_decision_for_structured(&mut inst),
            "a structured row must skip the tmux status decision"
        );

        // Nothing downstream sees a transition: `observed_transitions` compares
        // `prev` against this status, and the baseline stays in step with it for
        // any later consumer. `update_status_with_metadata` is not involved, the
        // caller's `continue` skips it outright.
        assert_eq!(
            inst.status,
            Status::Error,
            "the live acp status is authoritative, not the disk value"
        );
        assert_eq!(
            inst.live_status_baseline,
            Some(inst.status),
            "baseline must stay in step with the carried status"
        );
    }

    #[test]
    fn tick_reports_no_transition_for_a_structured_phantom() {
        // The regression at tick level, over the two halves together. The
        // helper tests above pass even if the `continue` is dropped from
        // `apply_tick_status_decisions`; this one does not, so it is what
        // actually guards the 81k-log-lines bug.
        let inst = phantom_structured_row("acp-session");
        let mut prev = std::collections::HashMap::from([(inst.id.clone(), Status::Error)]);
        let mut instances = vec![inst];
        let tracking = instances
            .iter()
            .map(|row| (row.id.clone(), PriorTickTracking::of(row)))
            .collect();

        apply_tick_status_decisions(
            &mut instances,
            &mut prev,
            &tracking,
            &std::collections::HashSet::new(),
            Some(&std::collections::HashMap::new()),
            &Default::default(),
        );

        assert_eq!(
            observed_transitions(&instances, &prev),
            vec![],
            "a structured row whose live status did not move must report no \
             transition, so status_tx stays silent and nothing is persisted or \
             marked unread"
        );
        // Note this holds for *every* structured row, not just a phantom: the
        // tick always carries `prev` onto them, so this path reports nothing for
        // them ever. That is the design. A real structured transition comes from
        // `apply_status_intent`, which mutates `state.instances` and broadcasts
        // its own `StatusChange`, so `prev` already carries it next tick. See
        // `tick_forces_a_recently_restarted_row_to_starting` for the proof that
        // the tick still reports transitions it does own.
    }

    #[test]
    fn tick_distinguishes_launch_ownership_from_startup_grace() {
        let now = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let born = std::time::SystemTime::from(now);
        for (case, reserved, launched_at, initial, expected) in [
            (
                "active construction",
                true,
                Some(born),
                Status::Error,
                Status::Error,
            ),
            (
                "active construction outlives startup grace",
                true,
                Some(born - std::time::Duration::from_secs(30)),
                Status::Error,
                Status::Error,
            ),
            (
                "committed birth",
                false,
                Some(born),
                Status::Starting,
                Status::Starting,
            ),
            (
                "committed startup grace",
                false,
                Some(born - std::time::Duration::from_millis(2_999)),
                Status::Starting,
                Status::Starting,
            ),
            (
                "committed grace boundary",
                false,
                Some(born - std::time::Duration::from_secs(3)),
                Status::Starting,
                Status::Running,
            ),
            (
                "committed launch does not borrow ownership TTL",
                false,
                Some(born - std::time::Duration::from_secs(5)),
                Status::Starting,
                Status::Running,
            ),
            (
                "unknown birth",
                false,
                None,
                Status::Starting,
                Status::Running,
            ),
            (
                "future stamp grants no startup hold",
                false,
                Some(born + std::time::Duration::from_secs(60)),
                Status::Starting,
                Status::Running,
            ),
            (
                "fresh birth does not suppress a non-starting row",
                false,
                Some(born),
                Status::Waiting,
                Status::Running,
            ),
        ] {
            let mut instance = Instance::new("launching", "/tmp/launching");
            instance.status = initial;
            instance.capture_started_at = launched_at;
            if reserved {
                instance
                    .try_acquire_lifecycle_reservation(
                        crate::session::LifecycleOperation::Launch,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        now,
                    )
                    .unwrap();
            }
            let tracking = PriorTickTracking::of(&instance);
            apply_tick_status_decision(
                &mut instance,
                Some(Status::Running),
                Some(&tracking),
                false,
                None,
                None,
                now,
            );
            assert_eq!(instance.status, expected, "{case}");
        }
    }

    #[test]
    fn tick_skips_a_row_that_is_new_since_the_last_snapshot() {
        // No `prev` entry means the row was created since the last tick; there
        // is no previous status to have transitioned from.
        let mut instances = vec![phantom_structured_row("acp-session")];
        let mut prev = std::collections::HashMap::new();

        apply_tick_status_decisions(
            &mut instances,
            &mut prev,
            &Default::default(),
            &std::collections::HashSet::new(),
            Some(&std::collections::HashMap::new()),
            &Default::default(),
        );

        assert_eq!(instances[0].status, Status::Idle, "disk status stands");
        assert_eq!(instances[0].live_status_baseline, None);
        assert_eq!(observed_transitions(&instances, &prev), vec![]);
    }

    #[test]
    fn tick_forces_a_recently_restarted_row_to_starting() {
        // Two things at once. The suppression branch must keep winning over the
        // structured carry (a worker mid-restart is Starting, not whatever the
        // last tick saw), and it doubles as the positive control that the
        // structured suppression is not a blanket mute: a transition this tick
        // genuinely owns is still reported. Suppression is the one branch that
        // moves a status without consulting tmux, so it proves that without
        // needing a live pane.
        let inst = phantom_structured_row("acp-session");
        let id = inst.id.clone();
        let mut prev = std::collections::HashMap::from([(id.clone(), Status::Error)]);
        let mut instances = vec![inst];

        apply_tick_status_decisions(
            &mut instances,
            &mut prev,
            &Default::default(),
            &std::collections::HashSet::from([id]),
            Some(&std::collections::HashMap::new()),
            &Default::default(),
        );

        assert_eq!(instances[0].status, Status::Starting);
        assert_eq!(
            observed_transitions(&instances, &prev),
            vec![(0, Status::Error)],
            "a transition the tick does own must still be reported"
        );
    }

    #[test]
    fn tick_holds_tmux_statuses_when_the_batch_probe_fails() {
        for (disk, live) in [
            (Status::Idle, Status::Running),
            (Status::Unknown, Status::Error),
        ] {
            let mut inst = Instance::new("tmux-session", "/tmp/test");
            inst.status = disk;
            let id = inst.id.clone();
            let mut prev = std::collections::HashMap::from([(id, live)]);
            let mut instances = vec![inst];
            let tracking = instances
                .iter()
                .map(|row| (row.id.clone(), PriorTickTracking::of(row)))
                .collect();

            apply_tick_status_decisions(
                &mut instances,
                &mut prev,
                &tracking,
                &std::collections::HashSet::new(),
                None,
                &Default::default(),
            );

            assert_eq!(instances[0].status, live, "disk status was {disk:?}");
            assert_eq!(observed_transitions(&instances, &prev), vec![]);
        }
    }

    #[test]
    fn sandbox_health_requires_a_current_terminal_observation() {
        for (view, status, observed_generation, known, expected) in [
            (
                crate::session::View::Terminal,
                Status::Idle,
                4,
                true,
                Status::Error,
            ),
            (
                crate::session::View::Structured,
                Status::Idle,
                4,
                true,
                Status::Idle,
            ),
            (
                crate::session::View::Terminal,
                Status::Starting,
                4,
                true,
                Status::Starting,
            ),
            (
                crate::session::View::Terminal,
                Status::Stopped,
                4,
                true,
                Status::Stopped,
            ),
            (
                crate::session::View::Terminal,
                Status::Idle,
                3,
                true,
                Status::Idle,
            ),
            (
                crate::session::View::Terminal,
                Status::Idle,
                4,
                false,
                Status::Idle,
            ),
        ] {
            let mut row = Instance::new("sandbox", "/tmp/sandbox");
            row.view = view;
            row.status = status;
            row.lifecycle_generation = 4;
            row.idle_entered_at = Some(chrono::Utc::now());
            row.sandbox_info = Some(crate::session::SandboxInfo {
                provider: None,
                enabled: true,
                container_id: None,
                image: "unused".into(),
                container_name: "owned-container".into(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                before_start_hook_env: Vec::new(),
                container_workdir: None,
            });
            let mut prev = std::collections::HashMap::from([(row.id.clone(), status)]);
            let mut health = std::collections::HashMap::new();
            if known {
                health.insert(
                    row.id.clone(),
                    SandboxHealth {
                        generation: observed_generation,
                        container_name: "owned-container".into(),
                        running: false,
                    },
                );
            }
            let mut canonical = vec![row.clone()];
            let tracking =
                std::collections::HashMap::from([(row.id.clone(), PriorTickTracking::of(&row))]);
            apply_tick_status_decisions(
                std::slice::from_mut(&mut row),
                &mut prev,
                &tracking,
                &Default::default(),
                None,
                &health,
            );
            merge_loaded_rows(
                &mut canonical,
                vec![row],
                StatusSource::TmuxApplied,
                &Default::default(),
            );
            let row = &canonical[0];
            assert_eq!(
                row.status, expected,
                "{view:?}, {status:?}, generation {observed_generation}, known {known}"
            );
            if expected == Status::Error {
                assert!(row.last_error.is_some());
                assert!(row.idle_entered_at.is_none());
            }
        }
    }

    #[test]
    fn skip_tmux_decision_for_structured_keeps_disk_status_without_a_baseline() {
        // A row created since the last tick has no live value yet. Its disk
        // status is all there is, and the absent baseline already suppresses
        // the transition report.
        let mut inst = phantom_structured_row("acp-session");
        inst.status = Status::Running;

        assert!(skip_tmux_decision_for_structured(&mut inst));

        assert_eq!(inst.status, Status::Running);
        assert_eq!(inst.live_status_baseline, None);
    }

    #[test]
    fn skip_tmux_decision_for_structured_clears_a_stale_tmux_error() {
        // Shares `Instance::clear_stale_tmux_error` with the structured
        // short-circuit in `update_status_with_metadata_inner`, for a row
        // converted from a terminal session: the tmux message cannot apply to
        // it any more.
        let mut inst = phantom_structured_row("acp-session");
        inst.last_error = Some(crate::session::TMUX_SESSION_GONE_ERROR.to_string());

        assert!(skip_tmux_decision_for_structured(&mut inst));

        assert_eq!(inst.last_error, None);
    }

    #[test]
    fn skip_tmux_decision_for_structured_leaves_tmux_sessions_to_the_poller() {
        // A terminal session has a real pane; the poller is authoritative and
        // must still run its tmux decision against the disk-loaded row.
        let mut inst = Instance::new("tmux-session", "/tmp/test");
        inst.status = Status::Idle;
        inst.live_status_baseline = Some(Status::Error);
        inst.last_error = Some(crate::session::TMUX_SESSION_GONE_ERROR.to_string());

        assert!(
            !skip_tmux_decision_for_structured(&mut inst),
            "a tmux-backed session must not skip the tmux status decision"
        );

        assert_eq!(inst.status, Status::Idle, "disk status must be untouched");
        assert_eq!(
            inst.last_error.as_deref(),
            Some(crate::session::TMUX_SESSION_GONE_ERROR),
            "a tmux-backed session's tmux error must survive for the poller"
        );
    }

    fn tmux_available() -> bool {
        crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// A proposal must survive disk reload, confirm, and remain stable on skipped captures.
    #[test]
    #[serial_test::serial]
    fn a_proposal_survives_the_tick_that_reloads_its_row_from_disk() {
        let _app = crate::session::test_support::isolate_app_dir();
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }

        let mut on_disk = Instance::new("aoe_test_3642_tick", "/tmp");
        on_disk.status = Status::Running;
        on_disk.tool = "claude".into();
        on_disk.view = crate::session::View::Terminal;

        let session_name = crate::tmux::Session::generate_name(&on_disk.id, &on_disk.title);
        let _kill = crate::tmux::test_helpers::TmuxTestSession::from_name(session_name.clone());
        let mut args = [
            "new-session",
            "-d",
            "-s",
            &session_name,
            "-x",
            "120",
            "-y",
            "40",
            "printf 'turn over\n'; sleep 300",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        crate::tmux::utils::append_pane_base_index_args(&mut args, &session_name);
        let created = crate::tmux::tmux_command()
            .args(&args)
            .output()
            .expect("spawn tmux");
        assert!(
            created.status.success(),
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&created.stderr)
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let pane = crate::tmux::tmux_query_command()
                .args(["capture-pane", "-p", "-t", &session_name])
                .output()
                .expect("capture fixture pane");
            assert!(pane.status.success(), "fixture pane capture failed");
            if String::from_utf8_lossy(&pane.stdout).contains("turn over") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fixture pane did not initialize"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let cache = crate::tmux::SessionCacheGuard::capture();
        cache.force_present(&[session_name.as_str()]);
        let pane = crate::tmux::Session::from_name(&session_name);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !pane.capture_pane(2).unwrap().contains("turn over") {
            assert!(
                std::time::Instant::now() < deadline,
                "fixture output did not reach the agent pane"
            );
            std::thread::yield_now();
        }

        let mut prev = std::collections::HashMap::from([(on_disk.id.clone(), Status::Running)]);
        let mut tracking = std::collections::HashMap::from([(
            on_disk.id.clone(),
            PriorTickTracking::of(&on_disk),
        )]);

        // A forced presence snapshot cannot refresh itself after its TTL.
        let mut tick = |window_activity: Option<i64>| {
            cache.force_present(&[session_name.as_str()]);
            let metadata = std::collections::HashMap::from([(
                session_name.clone(),
                crate::tmux::PaneMetadata {
                    server: None,
                    session_id: "$42".into(),
                    pane_id: "%42".into(),
                    session_kind: None,
                    tool_owner: crate::tmux::ToolPaneOwner::Unmarked,
                    pane_dead: false,
                    pane_current_command: Some("claude".to_string()),
                    pane_start_command_is_protected: false,
                    pane_pid: None,
                    pane_title: None,
                    window_activity,
                    window_size: None,
                },
            )]);
            let mut instances = vec![on_disk.clone()];
            seed_tick_tracking(&mut instances, &tracking);
            apply_tick_status_decisions(
                &mut instances,
                &mut prev,
                &tracking,
                &std::collections::HashSet::new(),
                Some(&metadata),
                &Default::default(),
            );
            tracking = instances
                .iter()
                .map(|i| (i.id.clone(), PriorTickTracking::of(i)))
                .collect();
            // Published passive transitions are persisted before the next tick.
            on_disk.status = instances[0].status;
            prev.insert(instances[0].id.clone(), instances[0].status);
            instances[0].status
        };

        // No activity stamp.
        assert_eq!(
            tick(None),
            Status::Running,
            "an unwitnessed Idle waits for a tick that agrees with it"
        );
        assert_eq!(
            tick(None),
            Status::Idle,
            "the tick that agrees publishes it (#3642)"
        );

        // A stamp whose second is already past.
        let settled = Utc::now().timestamp() - 60;
        assert_eq!(tick(Some(settled)), Status::Idle);
        assert_eq!(
            tick(Some(settled)),
            Status::Idle,
            "a skipped tick must leave the published status standing, not \
             re-derive one from a row it did not capture for"
        );

        let old_idle = "2026-01-01T00:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();
        on_disk.status = Status::Idle;
        on_disk.lifecycle_generation = 3;
        on_disk.idle_entered_at = Some(old_idle);
        on_disk.detection.pending = Some(Status::Idle);
        let tracking = std::collections::HashMap::from([(
            on_disk.id.clone(),
            PriorTickTracking::of(&on_disk),
        )]);
        let mut prev = std::collections::HashMap::from([(on_disk.id.clone(), Status::Idle)]);
        let mut fresh = on_disk.clone();
        fresh.lifecycle_generation = 4;
        fresh.status = Status::Running;
        fresh.idle_entered_at = None;
        let metadata = std::collections::HashMap::from([(
            session_name,
            crate::tmux::PaneMetadata {
                server: None,
                session_id: "$42".into(),
                pane_id: "%42".into(),
                session_kind: None,
                tool_owner: crate::tmux::ToolPaneOwner::Unmarked,
                pane_dead: false,
                pane_current_command: Some("claude".into()),
                pane_start_command_is_protected: false,
                pane_pid: None,
                pane_title: None,
                window_activity: None,
                window_size: None,
            },
        )]);
        let mut instances = vec![fresh];
        seed_tick_tracking(&mut instances, &tracking);
        cache.force_present(&[pane.name()]);
        apply_tick_status_decisions(
            &mut instances,
            &mut prev,
            &tracking,
            &std::collections::HashSet::new(),
            Some(&metadata),
            &Default::default(),
        );
        assert_eq!(instances[0].status, Status::Idle);
        assert_ne!(
            instances[0]
                .idle_entered_at
                .expect("a fresh Idle observation owns its timestamp"),
            old_idle
        );
        assert_eq!(
            observed_transitions(&instances, &prev),
            vec![(0, Status::Running)]
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn supplementary_new_generation_stamps_a_visible_idle_observation() {
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }
        let _home = crate::session::test_support::isolate_app_dir();
        Storage::new_unwatched("main").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = crate::session::get_profile_dir_path("main")
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        std::os::unix::fs::symlink(outside.path(), root.join("external")).unwrap();
        let storage = Storage::open_reference("external").unwrap();
        let working_dir = outside.path().to_str().unwrap();
        let mut stored = Instance::new("external idle", working_dir);
        stored.tool = "claude".into();
        stored.status = Status::Running;
        stored.lifecycle_generation = 1;
        storage
            .update(|rows, _| {
                *rows = vec![stored.clone()];
                Ok(())
            })
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        crate::server::test_support::accept_runtime_read_cache_for_test(&state).await;
        let old_idle = "2026-01-01T00:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();
        {
            let mut cache = state.runtime_read_cache.write().unwrap();
            let row = &mut cache.alias_only_instances[0];
            row.status = Status::Idle;
            row.idle_entered_at = Some(old_idle);
        }
        let prior = supplementary_tick_tracking(&state.runtime_read_cache.read().unwrap());
        storage
            .update(|rows, _| {
                rows[0].lifecycle_generation = 2;
                rows[0].status = Status::Running;
                rows[0].idle_entered_at = None;
                Ok(())
            })
            .unwrap();
        let session = crate::tmux::Session::new(&stored.id, &stored.title).unwrap();
        let session_name = session.name().to_owned();
        let _kill = crate::tmux::test_helpers::TmuxTestSession::from_name(session_name.clone());
        session
            .create_with_size(
                working_dir,
                Some("printf '\u{273b} Worked for 1m 52s\n\u{276f}\n'; exec sleep 300"),
                Some((120, 40)),
                "external",
            )
            .unwrap();
        crate::tmux::test_helpers::wait_for_pane_command(&session_name, "sleep");
        let cache_guard = crate::tmux::SessionCacheGuard::capture();
        cache_guard.force_present(&[&session_name]);
        let metadata = std::collections::HashMap::from([(
            session_name,
            crate::tmux::PaneMetadata {
                server: None,
                session_id: "$42".into(),
                pane_id: "%42".into(),
                session_kind: None,
                tool_owner: crate::tmux::ToolPaneOwner::Unmarked,
                pane_dead: false,
                pane_current_command: Some("claude".into()),
                pane_start_command_is_protected: false,
                pane_pid: None,
                pane_title: None,
                window_activity: None,
                window_size: None,
            },
        )]);
        let (mut loaded, loaded_metadata) =
            crate::server::reload::load_profiles_for_reload(&state.file_watch);
        assert_eq!(loaded.cache.alias_only_instances[0].status, Status::Running);
        assert_eq!(loaded.cache.alias_only_instances[0].idle_entered_at, None);
        apply_supplementary_tick(&mut loaded.cache, &prior, Some(&metadata));
        let row = &loaded.cache.alias_only_instances[0];
        assert_eq!(row.status, Status::Idle);
        let fresh_idle = row
            .idle_entered_at
            .expect("a visible Idle in the new execution owns a timestamp");
        assert_ne!(fresh_idle, old_idle);
        assert_eq!(row.lifecycle_generation, 2);
        reload_state_instances_from_disk(
            &state,
            loaded,
            vec![],
            StatusSource::TmuxApplied,
            0,
            loaded_metadata,
            Default::default(),
        )
        .await;
        let prior = {
            let cache = state.runtime_read_cache.read().unwrap();
            assert_eq!(
                cache.alias_only_instances[0].idle_entered_at,
                Some(fresh_idle)
            );
            supplementary_tick_tracking(&cache)
        };
        let mut loaded = load_all_instances(&state.file_watch);
        apply_supplementary_tick(&mut loaded.cache, &prior, Some(&metadata));
        let row = &loaded.cache.alias_only_instances[0];
        assert_eq!(row.status, Status::Idle);
        assert_eq!(row.idle_entered_at, Some(fresh_idle));
    }

    #[test]
    fn runtime_errors_follow_the_committed_status_transition() {
        for (previous_status, committed_status, expected_error) in [
            (Status::Error, Status::Error, Some("launch failed")),
            (Status::Error, Status::Idle, None),
            (Status::Idle, Status::Idle, None),
        ] {
            let mut prior = Instance::new("seed", "/tmp/seed");
            prior.status = previous_status;
            prior.last_error = Some("launch failed".into());
            let mut fresh = Instance::new("seed", "/tmp/seed");
            fresh.status = committed_status;
            merge_runtime_fields(prior, &mut fresh);
            assert_eq!(fresh.last_error.as_deref(), expected_error);
            assert_eq!(fresh.status, committed_status);
        }

        // A capability the prior row discovered is runtime state too: a reload
        // must not drop it and make the next load re-probe the session.
        let mut prior = Instance::new("seed", "/tmp/seed");
        prior.view = crate::session::View::Structured;
        prior.acp_load_session_capable = Some(true);
        let mut fresh = Instance::new("seed", "/tmp/seed");
        fresh.view = crate::session::View::Structured;
        merge_runtime_fields(prior, &mut fresh);
        assert_eq!(fresh.acp_load_session_capable, Some(true));
    }

    /// `plugin_revival_pending` is `#[serde(skip)]`, so every 2s status-poll tick's fresh
    /// disk load defaults it to `false`; without carrying it here, a revival slower than one
    /// tick would silently stop counting toward its plugin's concurrency cap.
    #[test]
    fn merge_runtime_fields_preserves_plugin_revival_pending() {
        let mut prior = Instance::new("seed", "/tmp/seed");
        prior.plugin_revival_pending = true;

        let mut fresh = Instance::new("seed", "/tmp/seed");
        merge_runtime_fields(prior, &mut fresh);

        assert!(fresh.plugin_revival_pending);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reload_captures_publications_from_a_replaced_execution() {
        use crate::session::{ConversationProvenance, ExecutionBinding};
        use std::os::unix::fs::DirBuilderExt;

        let app = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(app.path());
        let file_watch = FileWatchService::new().unwrap();
        let hook_base = app.path().join("hooks");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&hook_base)
            .unwrap();
        let mut prior = Instance::new("reload-capture", app.path().to_str().unwrap());
        prior.tool = "claude".into();
        prior.source_profile = "reload-capture".into();
        prior.status = Status::Running;
        let hooks = hook_base.join(&prior.id);
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&hooks)
            .unwrap();
        let launch = uuid::Uuid::new_v4().to_string();
        prior.active_execution = Some(
            serde_json::from_value(serde_json::json!({
                "launch_id": launch,
                "binding": ExecutionBinding {
                    agent: "claude".into(),
                    stores: vec![app.path().to_path_buf()],
                    configuration: Vec::new(),
                    exported_default_store: None,
                    cwd: app.path().to_path_buf(),
                    cwd_filesystem: "host".into(),
                    filesystem: "host".into(),
                },
                "capture": { "Hooks": hooks.join(format!("session_id.{launch}")) },
                "container": null,
            }))
            .unwrap(),
        );
        assert_eq!(
            prior.maybe_start_poller(),
            crate::session::PollerStart::Started
        );
        let mut fresh: Instance =
            serde_json::from_str(&serde_json::to_string(&prior).unwrap()).unwrap();
        fresh.source_profile = prior.source_profile.clone();
        let launch = uuid::Uuid::new_v4().to_string();
        let publication = hooks.join(format!("session_id.{launch}"));
        let active = fresh.active_execution.as_mut().unwrap();
        active.launch_id = launch;
        active.capture =
            Some(serde_json::from_value(serde_json::json!({ "Hooks": publication })).unwrap());
        let storage = Storage::new_unwatched(&fresh.source_profile).unwrap();
        storage
            .update(|rows, _| {
                *rows = vec![fresh.clone()];
                Ok(())
            })
            .unwrap();
        let mut reloaded = fresh;
        merge_runtime_fields(prior, &mut reloaded);
        let sid = uuid::Uuid::new_v4().to_string();
        std::fs::write(&publication, &sid).unwrap();
        let snapshot = crate::tmux::LiveSessionSnapshot::from_parts(
            Some(vec![reloaded.tmux_session().unwrap().name().to_string()]),
            None,
        );
        reloaded.reconcile_capture_poller(&snapshot);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            crate::session::sync::drain_and_persist_session_ids(
                std::slice::from_mut(&mut reloaded),
                &file_watch,
            );
            if reloaded.agent_session_id.as_deref() == Some(&sid)
                || std::time::Instant::now() >= deadline
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        reloaded.capture_poller.stop();
        let stored = storage.load().unwrap().remove(0);
        assert_eq!(stored.agent_session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(
            stored.agent_session_binding.unwrap().provenance,
            ConversationProvenance::Observed
        );
    }

    /// The window a row armed is about the execution it armed it for. Both halves of that matter:
    /// a row with nothing to poll holds no poller and no execution, and keeps its window, while a
    /// row another process gave a new execution does not inherit one armed for the old.
    #[test]
    fn a_reload_keeps_the_repair_pacing_of_a_row_about_the_same_execution() {
        let now = std::time::Instant::now();
        let execution = |id: &str| {
            Some(crate::session::ActiveExecution {
                launch_id: id.to_string(),
                binding: crate::session::ExecutionBinding {
                    agent: "claude".into(),
                    stores: Vec::new(),
                    configuration: Vec::new(),
                    cwd: std::path::PathBuf::from("/tmp"),
                    cwd_filesystem: "host".into(),
                    filesystem: "host".into(),
                    exported_default_store: None,
                },
                capture: None,
                container: None,
            })
        };
        let mut mergers: Vec<fn(Instance, Instance) -> Instance> = vec![|prior, mut fresh| {
            merge_runtime_fields(prior, &mut fresh);
            fresh
        }];
        mergers.push(|prior, mut fresh| {
            fresh.merge_runtime_from_reload(&prior);
            fresh
        });
        for merge in mergers {
            // What the repair walk leaves behind on a row with no poller: a re-probe ladder and a
            // store-retry deadline.
            let mut prior = Instance::new("poller-less", "/tmp/poller-less");
            prior.capture_poller.repair_mut().reprobe(now);
            let deadline = now + std::time::Duration::from_secs(30);
            prior.capture_poller.set_retry_after(Some(deadline));
            assert!(prior.capture_poller.handle().is_none());

            let merged = merge(prior, Instance::new("poller-less", "/tmp/poller-less"));

            assert_eq!(
                merged.capture_poller.repair().current_reprobe_delay(),
                Some(std::time::Duration::from_secs(5)),
                "the row keeps its window, or it re-resolves every tick"
            );
            assert_eq!(
                merged.capture_poller.retry_after(),
                Some(deadline),
                "and so does its store-retry deadline"
            );

            // Another process gave the row a new execution while it was waiting: the window was
            // armed for the old one and paces nothing this row can still use.
            let mut prior = Instance::new("replaced", "/tmp/replaced");
            prior.active_execution = execution("launch-1");
            prior.capture_poller.repair_mut().reprobe(now);
            let deadline = now + std::time::Duration::from_secs(30);
            prior.capture_poller.set_retry_after(Some(deadline));
            let mut fresh = Instance::new("replaced", "/tmp/replaced");
            fresh.active_execution = execution("launch-2");

            let merged = merge(prior, fresh);

            assert!(
                merged
                    .capture_poller
                    .repair()
                    .due(std::time::Instant::now()),
                "a window armed for the superseded execution does not hold the new one back"
            );
            assert_eq!(
                merged.capture_poller.retry_after(),
                None,
                "and neither does the deadline that went with it"
            );
        }
    }

    /// A profile move, a TUI edit or `aoe restart` can change a row's agent on disk while it holds
    /// no execution on either side, and a poller that watched no execution belongs to an agent just
    /// as much. Carrying it would leave the row watching the previous agent's capture, and the
    /// repair walk skips on a running poller, so nothing else would replace it.
    ///
    /// Driven through [`merge_loaded_rows`] on the `TmuxApplied` source, not through the function
    /// it delegates to: that is the only path a status-poll tick takes for a terminal row, so a
    /// test calling the inner merge alone passes even when the tick's own branch skips the guard
    /// entirely, once per row per 2s.
    #[test]
    fn a_reload_drops_the_poller_of_an_agent_the_row_no_longer_runs() {
        let mut prior = Instance::new("swapped-agent", "/tmp/swapped-agent");
        prior.tool = "claude".to_string();
        let mut poller = crate::session::poller::SessionPoller::new(
            "test-tmux-swapped-agent".to_string(),
            "claude".to_string(),
            None,
        );
        assert_eq!(
            poller.start(prior.id.clone(), Box::new(|| None), Box::new(|_| {}), None,),
            crate::session::poller::PollerSpawn::Spawned
        );
        let poller = std::sync::Arc::new(std::sync::Mutex::new(poller));
        prior.capture_poller.set_handle(Some(poller.clone()));
        prior.plugin_revival_pending = true;
        assert!(
            prior.active_execution.is_none(),
            "fixture: no execution on either side"
        );
        let mut fresh = Instance::new("swapped-agent", "/tmp/swapped-agent");
        // The merge is keyed by id, so the fresh disk load must be the same row and not merely a
        // second session with the same title.
        fresh.id = prior.id.clone();
        fresh.tool = "codex".to_string();

        let mut canonical = vec![prior];
        merge_loaded_rows(
            &mut canonical,
            vec![fresh],
            StatusSource::TmuxApplied,
            &Default::default(),
        );
        let merged = &canonical[0];

        assert_eq!(merged.tool, "codex");
        assert!(
            merged.capture_poller.handle().is_none(),
            "the previous agent's watcher does not follow the row to a new one"
        );
        // `Instance::capture_poller` holds a `std::sync::Mutex`, so this reads the guard
        // through `matches!` rather than unwrapping a poisoning error the type can raise.
        assert!(
            matches!(poller.lock(), Ok(poller) if !poller.is_running()),
            "and its thread is stopped, not merely dropped"
        );
        assert!(
            merged.plugin_revival_pending,
            "the revival mark is `#[serde(skip)]`, so a poll-path merge that drops it \
             silently un-counts a revival toward its plugin cap every 2s"
        );
    }

    /// The window a row armed is about the agent and the execution it armed it for. A row that has
    /// nothing to poll holds neither, and keeps its window; a row another agent took over does not
    /// inherit one paced for the previous agent.
    #[test]
    fn a_reload_keeps_the_repair_pacing_of_a_row_on_the_same_runtime() {
        let now = std::time::Instant::now();
        let mut mergers: Vec<fn(Instance, Instance) -> Instance> = vec![|prior, mut fresh| {
            merge_runtime_fields(prior, &mut fresh);
            fresh
        }];
        mergers.push(|prior, mut fresh| {
            fresh.merge_runtime_from_reload(&prior);
            fresh
        });
        for merge in mergers {
            let mut prior = Instance::new("poller-less", "/tmp/poller-less");
            prior.capture_poller.repair_mut().reprobe(now);
            let deadline = now + std::time::Duration::from_secs(30);
            prior.capture_poller.set_retry_after(Some(deadline));

            let merged = merge(prior, Instance::new("poller-less", "/tmp/poller-less"));

            assert_eq!(
                merged.capture_poller.repair().current_reprobe_delay(),
                Some(std::time::Duration::from_secs(5)),
                "the row keeps its window, or it re-resolves every tick"
            );
            assert_eq!(merged.capture_poller.retry_after(), Some(deadline));

            // The same row, another agent: the window paces the previous agent's capture.
            let mut prior = Instance::new("other-agent", "/tmp/other-agent");
            prior.tool = "claude".to_string();
            prior.capture_poller.repair_mut().reprobe(now);
            prior
                .capture_poller
                .set_retry_after(Some(now + std::time::Duration::from_secs(30)));
            let mut fresh = Instance::new("other-agent", "/tmp/other-agent");
            fresh.tool = "codex".to_string();

            let merged = merge(prior, fresh);

            assert_eq!(merged.tool, "codex");
            assert!(
                merged
                    .capture_poller
                    .repair()
                    .due(std::time::Instant::now()),
                "a window armed for the previous agent does not hold this one back"
            );
            assert_eq!(
                merged.capture_poller.retry_after(),
                None,
                "and neither does the deadline that went with it"
            );
        }
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn supplementary_tracking_follows_physical_identity_not_display_name_or_canonical_uuid() {
        for source in [StatusSource::TmuxApplied, StatusSource::DiskOnly] {
            let _home = crate::session::test_support::isolate_app_dir();
            let canonical = Storage::new_unwatched("main").unwrap();
            let mut normal = Instance::new("canonical shared", "/repo");
            normal.status = Status::Stopped;
            let shared_id = normal.id.clone();
            let mut stored = normal.clone();
            stored.title = "external shared".into();
            stored.status = Status::Running;
            assert_eq!(normal.id, stored.id);
            canonical
                .update(|rows, _| {
                    *rows = vec![normal];
                    Ok(())
                })
                .unwrap();
            let outside = tempfile::tempdir().unwrap();
            let root = crate::session::get_profile_dir_path("main")
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf();
            let identity = |path: &std::path::Path| {
                let metadata = std::fs::metadata(path).unwrap();
                crate::process::metadata_identity(&metadata)
            };
            assert_ne!(identity(&root.join("main")), identity(outside.path()));
            for name in ["external-a", "external-z"] {
                std::os::unix::fs::symlink(outside.path(), root.join(name)).unwrap();
            }
            Storage::open_reference("external-a")
                .unwrap()
                .update(|rows, _| {
                    *rows = vec![stored];
                    Ok(())
                })
                .unwrap();
            let state = crate::server::test_support::build_test_app_state(Vec::new());
            crate::server::test_support::accept_runtime_read_cache_for_test(&state).await;
            let unknown = std::time::Instant::now();
            let accessed = "2026-01-02T03:04:05Z"
                .parse::<chrono::DateTime<Utc>>()
                .unwrap();
            {
                let mut cache = state.runtime_read_cache.write().unwrap();
                let row = &mut cache.alias_only_instances[0];
                assert_eq!(row.id, shared_id);
                row.status = Status::Waiting;
                row.ever_confirmed_present = true;
                row.unknown_since = Some(unknown);
                row.detection.pending = Some(Status::Idle);
                row.last_accessed_at = Some(accessed);
            }
            std::fs::remove_file(root.join("external-a")).unwrap();
            let prior = supplementary_tick_tracking(&state.runtime_read_cache.read().unwrap());
            let (mut loaded, loaded_metadata) =
                crate::server::reload::load_profiles_for_reload(&state.file_watch);
            if matches!(source, StatusSource::TmuxApplied) {
                apply_supplementary_tick(&mut loaded.cache, &prior, None);
            }
            reload_state_instances_from_disk(
                &state,
                loaded,
                vec![],
                source,
                0,
                loaded_metadata,
                Default::default(),
            )
            .await;
            {
                let cache = state.runtime_read_cache.read().unwrap();
                let row = &cache.alias_only_instances[0];
                assert_eq!(row.source_profile, "external-z");
                assert_eq!(row.id, shared_id);
                assert_eq!(row.title, "external shared");
                assert_eq!(row.status, Status::Waiting);
                assert!(row.ever_confirmed_present);
                assert_eq!(row.unknown_since, Some(unknown));
                assert_eq!(row.detection.pending, Some(Status::Idle));
                assert_eq!(row.last_accessed_at, Some(accessed));
                assert!(row.capture_poller.handle().is_none());
            }
            let replacement = tempfile::tempdir().unwrap();
            assert_ne!(identity(outside.path()), identity(replacement.path()));
            let mut replacement_row = Instance::new("replacement", "/replacement");
            replacement_row.id = shared_id.clone();
            replacement_row.status = Status::Stopped;
            assert_eq!(replacement_row.id, state.instances.read().await[0].id);
            std::fs::write(
                replacement.path().join("sessions.json"),
                serde_json::to_vec(&vec![replacement_row]).unwrap(),
            )
            .unwrap();
            std::fs::remove_file(root.join("external-z")).unwrap();
            std::os::unix::fs::symlink(replacement.path(), root.join("external-z")).unwrap();
            let prior = supplementary_tick_tracking(&state.runtime_read_cache.read().unwrap());
            let (mut loaded, loaded_metadata) =
                crate::server::reload::load_profiles_for_reload(&state.file_watch);
            if matches!(source, StatusSource::TmuxApplied) {
                apply_supplementary_tick(&mut loaded.cache, &prior, None);
            }
            reload_state_instances_from_disk(
                &state,
                loaded,
                vec![],
                source,
                0,
                loaded_metadata,
                Default::default(),
            )
            .await;
            {
                let cache = state.runtime_read_cache.read().unwrap();
                let row = &cache.alias_only_instances[0];
                assert_eq!(row.id, shared_id);
                assert_eq!(row.title, "replacement");
                assert_eq!(row.project_path, "/replacement");
                assert_eq!(row.status, Status::Stopped);
                assert!(!row.ever_confirmed_present);
                assert_eq!(row.unknown_since, None);
                assert_eq!(row.detection.pending, None);
                assert_eq!(row.last_accessed_at, None);
            }
            let canonical_rows = state.instances.read().await;
            assert_eq!(canonical_rows[0].id, shared_id);
            assert_eq!(canonical_rows[0].title, "canonical shared");
            assert_eq!(canonical_rows[0].status, Status::Stopped);
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn supplementary_structured_status_follows_accepted_disk_rows() {
        let _home = crate::session::test_support::isolate_app_dir();
        Storage::new_unwatched("main").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = crate::session::get_profile_dir_path("main")
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        std::os::unix::fs::symlink(outside.path(), root.join("external")).unwrap();
        let storage = Storage::open_reference("external").unwrap();
        let mut row = Instance::new("external", "/repo");
        row.view = crate::session::View::Structured;
        row.status = Status::Running;
        storage
            .update(|rows, _| {
                *rows = vec![row.clone()];
                Ok(())
            })
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        crate::server::test_support::accept_runtime_read_cache_for_test(&state).await;
        let idle = "2026-01-01T00:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();
        for source in [StatusSource::TmuxApplied, StatusSource::DiskOnly] {
            for status in [
                Status::Stopped,
                Status::Waiting,
                Status::Idle,
                Status::Error,
                Status::Running,
            ] {
                storage
                    .update(|rows, _| {
                        rows[0].status = status;
                        rows[0].title = format!("changed-{status:?}");
                        rows[0].idle_entered_at = (status == Status::Idle).then_some(idle);

                        Ok(())
                    })
                    .unwrap();
                let prior = supplementary_tick_tracking(&state.runtime_read_cache.read().unwrap());
                let (mut loaded, loaded_metadata) =
                    crate::server::reload::load_profiles_for_reload(&state.file_watch);
                if matches!(source, StatusSource::TmuxApplied) {
                    apply_supplementary_tick(&mut loaded.cache, &prior, None);
                }
                reload_state_instances_from_disk(
                    &state,
                    loaded,
                    vec![],
                    source,
                    0,
                    loaded_metadata,
                    Default::default(),
                )
                .await;
                let cache = state.runtime_read_cache.read().unwrap();
                let accepted = &cache.alias_only_instances[0];
                assert_eq!(accepted.title, format!("changed-{status:?}"));
                assert_eq!(accepted.status, status);
                assert_eq!(
                    accepted.idle_entered_at,
                    (status == Status::Idle).then_some(idle)
                );

                assert!(state.instances.try_read().unwrap().is_empty());
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn supplementary_terminal_reuses_its_verdict_without_overriding_lifecycle() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut stored = Instance::new("external", "/repo");
        stored.source_profile = "external".into();
        stored.tool = "claude".into();
        stored.status = Status::Running;
        stored.lifecycle_generation = 1;
        let mut cached = stored.clone();
        cached.status = Status::Idle;
        let idle = "2026-01-01T00:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();
        cached.idle_entered_at = Some(idle);
        cached.ever_confirmed_present = true;
        cached.detection.activity = Some(100);
        cached.detection.captured_at = Some(101);
        let mut cache = RuntimeReadCache {
            inventory: vec![SelectableProfile {
                name: "external".into(),
                listed: false,
                aliases: vec![],
                identity: ProfileIdentity {
                    device: 1,
                    inode: 1,
                },
            }],
            alias_only_instances: vec![cached],
            ..Default::default()
        };
        let prior = supplementary_tick_tracking(&cache);
        let name = crate::tmux::Session::generate_name(&stored.id, &stored.title);
        let guard = crate::tmux::SessionCacheGuard::capture();
        guard.force_present(&[&name]);
        let metadata = std::collections::HashMap::from([(
            name,
            crate::tmux::PaneMetadata {
                server: None,
                session_id: "$42".into(),
                pane_id: "%42".into(),
                session_kind: None,
                tool_owner: crate::tmux::ToolPaneOwner::Unmarked,
                pane_dead: false,
                pane_current_command: Some("claude".into()),
                pane_start_command_is_protected: false,
                pane_pid: None,
                pane_title: None,
                window_activity: Some(100),
                window_size: None,
            },
        )]);
        cache.alias_only_instances = vec![stored.clone()];
        apply_supplementary_tick(&mut cache, &prior, Some(&metadata));
        assert_eq!(cache.alias_only_instances[0].status, Status::Idle);
        assert_eq!(cache.alias_only_instances[0].idle_entered_at, Some(idle));

        for status in [Status::Stopped, Status::Creating, Status::Deleting] {
            for metadata in [None, Some(&metadata)] {
                let mut fresh = stored.clone();
                fresh.status = status;
                cache.alias_only_instances = vec![fresh];
                apply_supplementary_tick(&mut cache, &prior, metadata);
                assert_eq!(cache.alias_only_instances[0].status, status);
                assert_eq!(cache.alias_only_instances[0].idle_entered_at, None);
            }
        }
        let mut fresh = stored.clone();
        fresh.lifecycle_generation = 2;
        cache.alias_only_instances = vec![fresh];
        apply_supplementary_tick(&mut cache, &prior, None);
        assert_eq!(cache.alias_only_instances[0].status, Status::Running);
        assert_eq!(cache.alias_only_instances[0].detection.captured_at, None);
        for old_status in [Status::Stopped, Status::Creating, Status::Deleting] {
            cache.alias_only_instances[0] = stored.clone();
            cache.alias_only_instances[0].status = old_status;
            let old = supplementary_tick_tracking(&cache);
            let mut fresh = stored.clone();
            fresh.status = Status::Starting;
            cache.alias_only_instances = vec![fresh];
            apply_supplementary_tick(&mut cache, &old, None);
            assert_eq!(cache.alias_only_instances[0].status, Status::Starting);
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn supplementary_disk_reload_restores_only_eligible_terminal_verdicts() {
        let _home = crate::session::test_support::isolate_app_dir();
        Storage::new_unwatched("main").unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = crate::session::get_profile_dir_path("main")
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        std::os::unix::fs::symlink(outside.path(), root.join("external")).unwrap();
        let storage = Storage::open_reference("external").unwrap();
        let mut stored = Instance::new("external", "/repo");
        stored.status = Status::Running;
        stored.lifecycle_generation = 1;
        storage
            .update(|rows, _| {
                *rows = vec![stored.clone()];
                Ok(())
            })
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        crate::server::test_support::accept_runtime_read_cache_for_test(&state).await;
        let idle = "2026-01-01T00:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap();
        let unknown = std::time::Instant::now();
        for (old, fresh, generation, reuse) in [
            (Status::Idle, Status::Running, 1, true),
            (Status::Idle, Status::Stopped, 1, false),
            (Status::Idle, Status::Creating, 1, false),
            (Status::Idle, Status::Deleting, 1, false),
            (Status::Idle, Status::Running, 2, false),
            (Status::Stopped, Status::Starting, 1, false),
            (Status::Creating, Status::Starting, 1, false),
            (Status::Deleting, Status::Starting, 1, false),
        ] {
            storage
                .update(|rows, _| {
                    rows[0].status = fresh;
                    rows[0].lifecycle_generation = generation;
                    rows[0].idle_entered_at = None;
                    Ok(())
                })
                .unwrap();
            {
                let mut cache = state.runtime_read_cache.write().unwrap();
                let row = &mut cache.alias_only_instances[0];
                row.status = old;
                row.lifecycle_generation = 1;
                row.idle_entered_at = (old == Status::Idle).then_some(idle);
                row.ever_confirmed_present = true;
                row.unknown_since = Some(unknown);
                row.detection.pending = Some(Status::Waiting);
                row.detection.captured_at = Some(101);
            }
            let (loaded, loaded_metadata) =
                crate::server::reload::load_profiles_for_reload(&state.file_watch);
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
            let cache = state.runtime_read_cache.read().unwrap();
            let row = &cache.alias_only_instances[0];
            assert_eq!(row.status, if reuse { old } else { fresh });
            assert_eq!(row.idle_entered_at, reuse.then_some(idle));
            assert_eq!(row.ever_confirmed_present, reuse);
            assert_eq!(row.unknown_since, reuse.then_some(unknown));
            assert_eq!(row.detection.pending, reuse.then_some(Status::Waiting));
            assert_eq!(row.detection.captured_at, reuse.then_some(101));
        }
    }
}
