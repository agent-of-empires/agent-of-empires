//! Reloading session rows from disk and merging them onto what the daemon
//! already holds in memory.

use crate::file_watch::FileWatchService;
use crate::session::Instance;
use crate::session::Status;
use crate::session::Storage;
use std::sync::Arc;

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
}

pub(crate) struct LoadedProfiles {
    pub instances: Vec<Instance>,
    pub metadata: CanonicalMetadata,
}

#[derive(Debug, thiserror::Error)]
#[error("canonical profile load failed")]
pub(crate) struct ReloadFailure {
    pub health: crate::daemon::RuntimeHealth,
    #[source]
    pub(super) source: anyhow::Error,
}

pub(super) fn load_all_profiles(
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
    let mut status_hooks = std::collections::HashMap::new();
    let mut auxiliary_tools = std::collections::HashMap::new();
    for profile in profiles {
        let (mut instances, groups, details) = (|| {
            let (instances, groups) =
                Storage::open(&profile, file_watch.clone())?.load_complete_with_groups()?;
            let details = load_profile_details(&profile)?;
            Ok::<_, anyhow::Error>((instances, groups, details))
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
        profile_metadata.push(ProfileSnapshot {
            name: profile,
            description: details.description,
            groups,
            projects: details.projects,
        });
    }
    Ok(LoadedProfiles {
        instances: all,
        metadata: CanonicalMetadata {
            default_profile,
            profiles: profile_metadata,
            workspace_ordering,
            global_projects,
            status_hooks,
            auxiliary_tools,
        },
    })
}

pub(super) struct ProfileDetails {
    pub description: Option<String>,
    pub projects: Vec<crate::daemon::ProjectResponse>,
    pub status_hooks: crate::status_hooks::StatusHookConfig,
    pub auxiliary_tools: Vec<String>,
}

pub(super) fn load_profile_details(profile: &str) -> anyhow::Result<ProfileDetails> {
    let description = crate::session::load_profile_config(profile)?.description;
    let projects = crate::session::projects::load_profile(profile)?
        .into_iter()
        .map(crate::daemon::ProjectResponse::from)
        .collect();
    let config = crate::session::resolve_config(profile)?;
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
    })
}

pub(super) enum StatusCommit {
    Passive,
    Lifecycle,
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
                    row.status = status;
                    row.idle_entered_at = idle_entered_at;
                    if matches!(commit, StatusCommit::Lifecycle) {
                        row.last_error = committed_error;
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

/// Adopt exact storage commits under one snapshot exclusion.
pub(crate) async fn adopt_committed_profiles<const N: usize>(
    state: &Arc<AppState>,
    committed: [(&str, Vec<Instance>, Vec<crate::session::Group>); N],
    committed_status: impl Fn(&str) -> bool,
    _publication: &tokio::sync::RwLockWriteGuard<'_, ()>,
) -> Result<(), ReloadFailure> {
    let mut metadata = state.canonical_metadata.write().await;
    let mut current = state.instances.write().await;
    replace_committed_profiles(&mut current, &mut metadata, committed, |id| {
        committed_status(id).then_some(StatusCommit::Lifecycle)
    })?;
    state
        .mutation_epoch
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    state.runtime.request_publish();
    Ok(())
}

/// Keep process state without replacing the committed lifecycle or identity.
pub(super) fn merge_runtime_fields(prior: Instance, fresh: &mut Instance) {
    let same_execution = fresh.active_execution == prior.active_execution;
    if !same_execution {
        prior.stop_poller();
    }
    fresh.last_error_check = prior.last_error_check;
    fresh.inherit_runtime(prior, fresh.status == Status::Error);
    if !same_execution {
        fresh.session_id_poller = None;
        fresh.poller_repair = Default::default();
        fresh.session_id_poller_retry_after = None;
    }
}

/// Observations erased by storage loading and carried into the next sample.
#[derive(Debug, Clone, Default)]
pub(super) struct PriorTickTracking {
    ever_confirmed_present: bool,
    unknown_since: Option<std::time::Instant>,
    detection: crate::session::DetectionState,
    auxiliary_targets: Vec<crate::session::AuxiliaryTarget>,
}

impl PriorTickTracking {
    pub(super) fn of(inst: &Instance) -> Self {
        Self {
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
    mut prev: std::collections::HashMap<String, PriorTickTracking>,
) {
    for inst in instances {
        if let Some(prior) = prev.remove(&inst.id) {
            inst.ever_confirmed_present = prior.ever_confirmed_present;
            inst.unknown_since = prior.unknown_since;
            inst.detection = prior.detection;
            inst.auxiliary = prior
                .auxiliary_targets
                .into_iter()
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
    prev: &std::collections::HashMap<String, crate::session::Status>,
    suppressed_ids: &std::collections::HashSet<String>,
    pane_metadata: Option<&std::collections::HashMap<String, crate::tmux::PaneMetadata>>,
    sandbox_health: &std::collections::HashMap<String, SandboxHealth>,
) {
    for inst in instances.iter_mut() {
        if suppressed_ids.contains(&inst.id) {
            inst.status = Status::Starting;
            continue;
        }
        inst.live_status_baseline = prev.get(&inst.id).copied();
        // A trashed row remains in storage until its retention period ends,
        // but it is no longer a live session. Do not turn its deliberately
        // stopped pane into a synthetic Error, and do not emit a status event
        // that the push consumer could notify about.
        if inst.is_trashed() {
            if let Some(live) = inst.live_status_baseline {
                inst.status = live;
            }
            continue;
        }
        if skip_tmux_decision_for_structured(inst) {
            continue;
        }
        // Launch owns the row until its reservation is committed or rolled
        // back. In particular, a missing pane during provisioning must not
        // overwrite Starting (or a reserved sampled status) with Error.
        if inst.status == Status::Starting
            || inst.has_fresh_lifecycle_reservation(chrono::Utc::now())
        {
            continue;
        }
        if inst.is_sandboxed()
            && !matches!(
                inst.status,
                Status::Stopped | Status::Starting | Status::Creating | Status::Deleting
            )
            && inst.sandbox_info.as_ref().is_some_and(|sandbox| {
                sandbox_health.get(&inst.id).is_some_and(|health| {
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
            continue;
        }
        let Some(pane_metadata) = pane_metadata else {
            // A failed batch probe says nothing about any individual pane.
            // Keep the last live status instead of treating an empty metadata
            // map as proof that every pane disappeared.
            if let Some(live) = inst.live_status_baseline {
                inst.status = live;
            }
            continue;
        };
        let session_name = crate::tmux::resolve_agent_session_name_in(
            pane_metadata,
            &inst.id,
            &crate::tmux::Session::generate_name(&inst.id, &inst.title),
        );
        inst.update_status_with_metadata(pane_metadata.get(&session_name), Some(&session_name));
    }
}

/// The real status transitions this tick observed, as `(index into instances,
/// previous status)` pairs.
///
/// The other half of [`apply_tick_status_decisions`]; see its docstring for why
/// they belong together. A row absent from `prev` is new this tick and has no
/// transition to report. Indices are only valid against the same slice, which
/// the caller consumes immediately.
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
/// tick reads that standing mismatch as a brand new transition, which logs a
/// `session.status_change` line, broadcasts a `StatusChange`, and resets the
/// push dwell timer in `server::push`. Forever, at the 2s tick, surviving daemon
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
    inst.clear_stale_tmux_error();
    // `None` means the row is newer than the last tick and has no live value
    // yet; its disk status is all there is, and the absent baseline already
    // suppresses a transition report.
    if let Some(live) = inst.live_status_baseline {
        inst.status = live;
    }
    true
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
/// acp-overlay pass. Intentionally exposes only `drain_from` and `get`;
/// no `remove` method, because invariant 5 of the merge contract
/// requires the same map to be populated when
/// `apply_acp_overlay_inplace` runs after the merge loop. The compiler
/// rejects any future `.remove()` call instead of relying on prose.
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
}

pub(super) async fn reload_state_instances_from_disk(
    state: &Arc<AppState>,
    mut fresh: Vec<Instance>,
    live_worker_records: Vec<LiveStructuredWorkerRecord>,
    status_source: StatusSource,
    read_epoch: u64,
    mut metadata: CanonicalMetadata,
    passive: std::collections::HashMap<String, super::status_poll::PassiveTransitionWrites>,
) -> Vec<super::push::StatusChange> {
    // Snapshot suppression here so a worker that unmarks between the
    // caller's input build and the per-id decision cannot combine a
    // cleared mark with a stale row to re-emit the phantom Error
    // transition the suppression exists to prevent. Idempotent on the
    // poll path, where the caller already applied the same override
    // inside `spawn_blocking`.
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
            *state.canonical_health.write().await = error.health;
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
            *state.canonical_health.write().await = error.health;
            state.runtime.request_publish();
            return Vec::new();
        }
    }

    let changes = merge_loaded_rows(&mut current, fresh, status_source, &suppressed_ids);
    drop(current);
    *state.canonical_metadata.write().await = metadata;
    *state.canonical_health.write().await = crate::daemon::RuntimeHealth::Healthy;
    state.runtime.request_publish();
    changes
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
        if let Some(mut prior) = prior_by_id.get(&row.id).cloned() {
            let prior_status = prior.status;
            let prior_last_accessed = prior.last_accessed_at;
            let prior_idle_entered = prior.idle_entered_at;
            if matches!(status_source, StatusSource::TmuxApplied) && !row.is_structured() {
                row.inherit_process_runtime(prior);
            } else {
                if matches!(status_source, StatusSource::TmuxApplied) {
                    prior.agent_pane = std::mem::take(&mut row.agent_pane);
                    prior.auxiliary = std::mem::take(&mut row.auxiliary);
                }
                merge_runtime_fields(prior, &mut row);
            }
            if matches!(status_source, StatusSource::DiskOnly) {
                row.status = prior_status;
                row.idle_entered_at = prior_idle_entered.or(row.idle_entered_at);
            }
            row.last_accessed_at = prior_last_accessed.max(row.last_accessed_at);
        }
        if suppressed_ids.contains(&row.id) {
            row.status = Status::Starting;
        }
        merged.push(row);
    }
    apply_acp_overlay_inplace(&prior_by_id, &mut merged);
    let mut changes = Vec::new();
    let now = chrono::Utc::now();
    for row in &mut merged {
        if row.is_archived() {
            row.settle_archived_status();
        }
        if matches!(status_source, StatusSource::TmuxApplied) && !row.is_structured() {
            if let Some(prior) = prior_by_id
                .get(&row.id)
                .filter(|prior| prior.status != row.status)
            {
                changes.push(super::push::StatusChange {
                    instance_id: row.id.clone(),
                    instance_title: row.title.clone(),
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
        if !inst.is_structured() {
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
            vec![row],
            vec![record],
            StatusSource::DiskOnly,
            1,
            {
                let metadata = state.canonical_metadata.read().await.clone();
                metadata
            },
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
            vec![row],
            vec![record],
            StatusSource::DiskOnly,
            0,
            {
                let metadata = state.canonical_metadata.read().await.clone();
                metadata
            },
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
            vec![row.clone()],
            vec![record],
            StatusSource::DiskOnly,
            0,
            {
                let metadata = state.canonical_metadata.read().await.clone();
                metadata
            },
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
            sampled.instances,
            vec![record],
            StatusSource::TmuxApplied,
            0,
            sampled.metadata,
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
            vec![stale_sample],
            vec![record.clone()],
            StatusSource::DiskOnly,
            0,
            metadata.clone(),
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
            loaded.instances,
            vec![record],
            StatusSource::DiskOnly,
            0,
            loaded.metadata,
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
        let prev = std::collections::HashMap::from([(inst.id.clone(), Status::Error)]);
        let mut instances = vec![inst];

        apply_tick_status_decisions(
            &mut instances,
            &prev,
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
    fn tick_preserves_starting_and_fresh_launch_reservations_without_a_pane() {
        for reserved in [false, true] {
            let mut instance = Instance::new("launching", "/tmp/launching");
            instance.status = if reserved {
                Status::Error
            } else {
                Status::Starting
            };
            if reserved {
                instance
                    .try_acquire_lifecycle_reservation(
                        crate::session::LifecycleOperation::Launch,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        chrono::Utc::now(),
                    )
                    .unwrap();
            }
            let id = instance.id.clone();
            let prev = std::collections::HashMap::from([(id, Status::Running)]);
            let mut instances = vec![instance];

            apply_tick_status_decisions(
                &mut instances,
                &prev,
                &std::collections::HashSet::new(),
                Some(&std::collections::HashMap::new()),
                &Default::default(),
            );

            assert_eq!(
                instances[0].status,
                if reserved {
                    Status::Error
                } else {
                    Status::Starting
                }
            );
        }
    }

    #[test]
    fn tick_skips_a_row_that_is_new_since_the_last_snapshot() {
        // No `prev` entry means the row was created since the last tick; there
        // is no previous status to have transitioned from.
        let mut instances = vec![phantom_structured_row("acp-session")];
        let prev = std::collections::HashMap::new();

        apply_tick_status_decisions(
            &mut instances,
            &prev,
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
        let prev = std::collections::HashMap::from([(id.clone(), Status::Error)]);
        let mut instances = vec![inst];

        apply_tick_status_decisions(
            &mut instances,
            &prev,
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
            let prev = std::collections::HashMap::from([(id, live)]);
            let mut instances = vec![inst];

            apply_tick_status_decisions(
                &mut instances,
                &prev,
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
                enabled: true,
                container_id: None,
                image: "unused".into(),
                container_name: "owned-container".into(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });
            let prev = std::collections::HashMap::from([(row.id.clone(), status)]);
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
            apply_tick_status_decisions(
                std::slice::from_mut(&mut row),
                &prev,
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
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }

        // Never mutated: cloning it is this test's disk load, so every
        // `#[serde(skip)]` field starts at its default exactly as
        // `load_all_profiles` leaves it.
        let mut on_disk = Instance::new("aoe_test_3642_tick", "/tmp");
        on_disk.status = Status::Running;
        on_disk.tool = "claude".to_owned();

        let session_name = crate::tmux::Session::generate_name(&on_disk.id, &on_disk.title);
        let _kill = crate::tmux::test_helpers::TmuxTestSession::from_name(session_name.clone());
        let created = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &session_name,
                "-x",
                "120",
                "-y",
                "40",
                "printf 'turn over\n'; sleep 300",
            ])
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

        let mut prev = std::collections::HashMap::from([(on_disk.id.clone(), Status::Running)]);
        let mut tracking: std::collections::HashMap<String, PriorTickTracking> =
            std::collections::HashMap::new();

        // One daemon tick, reporting the status it settled on and the rule
        // that decided. `window_activity` is supplied rather than scraped so
        // the capture-skip gate is driven, not raced.
        let mut tick = |window_activity: Option<i64>| {
            let metadata = std::collections::HashMap::from([(
                session_name.clone(),
                crate::tmux::PaneMetadata {
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
            seed_tick_tracking(&mut instances, std::mem::take(&mut tracking));
            apply_tick_status_decisions(
                &mut instances,
                &prev,
                &std::collections::HashSet::new(),
                Some(&metadata),
                &Default::default(),
            );
            tracking = instances
                .iter()
                .map(|i| (i.id.clone(), PriorTickTracking::of(i)))
                .collect();
            // A passive transition reaches disk in the tick that publishes it
            // (`flush_passive_transition_writes`), so the next tick's disk
            // load agrees with what this one decided.
            on_disk.status = instances[0].status;
            prev.insert(instances[0].id.clone(), instances[0].status);
            (instances[0].status, instances[0].detection.rule)
        };

        // No activity stamp: nothing to skip against, so both ticks decide on
        // a real capture.
        assert_eq!(
            tick(None).0,
            Status::Running,
            "an unwitnessed Idle waits for a tick that agrees with it"
        );
        assert_eq!(
            tick(None).0,
            Status::Idle,
            "the tick that agrees publishes it (#3642)"
        );

        // A stamp whose second is already past: the tick that records it still
        // captures, and the one after it has the proof the gate asks for.
        let settled = Utc::now().timestamp() - 60;
        assert_eq!(tick(Some(settled)).0, Status::Idle);
        assert_eq!(
            tick(Some(settled)),
            (Status::Idle, Some("screen_unchanged")),
            "a skipped tick must leave the published status standing, not \
             re-derive one from a row it did not capture for"
        );
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
        reloaded.repair_session_id_poller_if_needed(&snapshot);
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
        reloaded.stop_poller();
        let stored = storage.load().unwrap().remove(0);
        assert_eq!(stored.agent_session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(
            stored.agent_session_binding.unwrap().provenance,
            ConversationProvenance::Observed
        );
    }
}
