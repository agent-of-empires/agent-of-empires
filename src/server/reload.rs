//! Reloading session rows from disk and merging them onto what the daemon
//! already holds in memory.

use crate::file_watch::FileWatchService;
use crate::session::Instance;
use crate::session::Status;
use crate::session::Storage;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ProfileIdentity {
    device: u64,
    inode: u64,
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
        let metadata = match std::fs::metadata(entry.path()) {
            Ok(metadata) if metadata.is_dir() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) if kind.is_symlink() => continue,
            Err(error) => return Err(error.into()),
        };
        let identity = ProfileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
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

#[derive(Default)]
pub(crate) struct RuntimeReadCache {
    pub inventory: Vec<SelectableProfile>,
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

use super::state::{AppState, StatusSource};
use super::structured_repair::{
    persist_structured_row_repairs, repair_structured_rows_from_live_workers,
    LiveStructuredWorkerRecord,
};

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
    let mut loaded = LoadedInstances::default();
    loaded.cache.inventory = match selectable_profiles() {
        Ok(profiles) => profiles,
        Err(error) => {
            tracing::warn!(target: "server.file_watch", %error, "profile enumeration failed");
            loaded.cache.health.enumeration_failed = true;
            return loaded;
        }
    };
    for profile in &loaded.cache.inventory {
        let rows = if profile.listed {
            Storage::new(&profile.name, file_watch.clone()).and_then(|storage| storage.load())
        } else {
            Storage::open_unwatched(&profile.name)
                .and_then(|storage| storage.load_instances_readonly())
        };
        match rows {
            Ok(mut instances) => {
                for instance in &mut instances {
                    instance.source_profile.clone_from(&profile.name);
                }
                if profile.listed {
                    loaded.instances.extend(instances);
                } else {
                    loaded.cache.alias_only_instances.extend(instances);
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
    }
    loaded
}

/// Carry over the in-memory-only fields from the prior `state.instances` entry into the
/// freshly-loaded one.
pub(super) fn merge_runtime_fields(prior: Instance, mut fresh: Instance) -> Instance {
    fresh.adopt_poller(&prior);
    fresh.adopt_poller_repair(&prior);
    merge_scalar_runtime_fields(prior, fresh)
}

fn merge_scalar_runtime_fields(prior: Instance, mut fresh: Instance) -> Instance {
    fresh.last_error_check = prior.last_error_check;
    fresh.last_start_time = prior.last_start_time;
    if fresh.status == Status::Error {
        fresh.last_error = prior.last_error;
    }
    fresh.acp_load_session_capable = prior.acp_load_session_capable;
    fresh.plugin_revival_pending = prior.plugin_revival_pending;
    fresh
}

/// Cached verdict metadata and detector state for one row.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct PriorTickTracking {
    lifecycle_generation: u64,
    idle_entered_at: Option<chrono::DateTime<chrono::Utc>>,
    ever_confirmed_present: bool,
    unknown_since: Option<std::time::Instant>,
    detection: crate::session::DetectionState,
}

impl PriorTickTracking {
    pub(super) fn of(inst: &Instance) -> Self {
        Self {
            lifecycle_generation: inst.lifecycle_generation,
            idle_entered_at: inst.idle_entered_at,
            ever_confirmed_present: inst.ever_confirmed_present,
            unknown_since: inst.unknown_since,
            detection: inst.detection,
        }
    }
}

/// Carry the previous tick's status bookkeeping onto a freshly disk-loaded instance, keyed
/// by id.
pub(super) fn seed_tick_tracking(
    instances: &mut [Instance],
    prev: &std::collections::HashMap<String, PriorTickTracking>,
) {
    for inst in instances {
        if let Some(prior) = prev.get(&inst.id) {
            inst.ever_confirmed_present = prior.ever_confirmed_present;
            inst.unknown_since = prior.unknown_since;
            inst.detection = prior.detection;
        }
    }
}

/// Apply one tick and retain its effective baselines for transition persistence.
pub(super) fn apply_tick_status_decisions(
    instances: &mut [Instance],
    prev: &mut std::collections::HashMap<String, Status>,
    tracking: &std::collections::HashMap<String, PriorTickTracking>,
    suppressed_ids: &std::collections::HashSet<String>,
    pane_metadata: Option<&std::collections::HashMap<String, crate::tmux::PaneMetadata>>,
) {
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
) -> Option<Status> {
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

/// Report whether the caller must skip the tmux status decision for this row, carrying the
/// acp-authoritative live status onto it when so.
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

// INVARIANTS for `reload_state_instances_from_disk` (do not break without revisiting
// `tests/serve_disk_reload_helper_equivalence.rs`).

/// Reload `state.instances` by merging caller-supplied `fresh` against the prior in-memory
/// snapshot per id, then reapplying the acp overlay.
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

#[doc(hidden)]
pub(crate) async fn reload_state_instances_from_disk(
    state: &Arc<AppState>,
    loaded: LoadedInstances,
    live_worker_records: Vec<LiveStructuredWorkerRecord>,
    status_source: StatusSource,
    read_epoch: u64,
) {
    let reload_guard = state.session_service.disk_reload_guard().await;
    let LoadedInstances {
        instances: fresh,
        mut cache,
    } = loaded;
    // Snapshot suppression here so a worker that unmarks between the caller's input build
    // and the per-id decision cannot combine a cleared mark with a stale row to re-emit the
    // phantom Error transition the suppression exists to prevent.
    let suppressed_ids =
        crate::session::recovery::snapshot_recently_restarted(&state.recently_restarted);
    // Repair is a view transition too.
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

    let mut current = state.instances.write().await;

    // Invariant 8.
    let current_epoch = state
        .mutation_epoch
        .load(std::sync::atomic::Ordering::SeqCst);
    if current_epoch != read_epoch {
        tracing::debug!(
            target: "server.file_watch",
            read_epoch,
            current_epoch,
            "dropping a disk reload whose snapshot predates a session lifecycle mutation"
        );
        return;
    }

    let prior_by_id = PriorById::drain_from(&mut current);
    let mut previous_cache = state
        .runtime_read_cache
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let previous_rows = std::mem::take(&mut previous_cache.alias_only_instances);
    let mut supplementary_prior: std::collections::HashMap<
        ProfileIdentity,
        std::collections::HashMap<String, Instance>,
    > = std::collections::HashMap::new();
    for row in previous_rows {
        if let Some(profile) = previous_cache
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

    let mut merged: Vec<Instance> = Vec::with_capacity(fresh.len());
    for mut row in fresh {
        if let Some(prior) = prior_by_id.get(&row.id).cloned() {
            let prior_status = prior.status;
            let prior_last_accessed = prior.last_accessed_at;
            let prior_idle_entered = prior.idle_entered_at;
            let prior_tracking = PriorTickTracking::of(&prior);
            row = merge_runtime_fields(prior, row);
            match status_source {
                StatusSource::DiskOnly => {
                    let purge_in_flight = prior_status == Status::Deleting
                        && row.lifecycle_reservation_is_owned(
                            crate::session::LifecycleOperation::Purge,
                            row.lifecycle_generation,
                        );
                    if row.is_structured()
                        || row.lifecycle_generation <= prior_tracking.lifecycle_generation
                        || purge_in_flight
                    {
                        row.status = prior_status;
                        row.idle_entered_at = prior_idle_entered;
                    }
                    // Reachability and detector state survive lifecycle reservations.
                    row.ever_confirmed_present = prior_tracking.ever_confirmed_present;
                    row.unknown_since = prior_tracking.unknown_since;
                    row.detection = prior_tracking.detection;
                }
                StatusSource::TmuxApplied => {
                    // Caller already applied tmux scrape to fresh.status; that is the
                    // authoritative value.
                }
            }
            row.last_accessed_at = prior_last_accessed.max(row.last_accessed_at);
        }
        if suppressed_ids.contains(&row.id) {
            row.status = Status::Starting;
        }
        merged.push(row);
    }

    let repairs = repair_structured_rows_from_live_workers(&mut merged, repair_records);

    apply_acp_overlay_inplace(&prior_by_id, &mut merged);

    *current = merged;
    *previous_cache = cache;
    drop(previous_cache);
    drop(current);
    drop(reload_guard);

    persist_structured_row_repairs(state, repairs, repair_guards);
}

/// Apply the acp status / timestamps overlay to `merged`, sourcing values from
/// `prior_by_id`.
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
                let mut loaded = load_all_instances(&state.file_watch);
                if matches!(source, StatusSource::TmuxApplied) {
                    seed_tick_tracking(&mut loaded.instances, &tracking);
                    apply_tick_status_decisions(
                        &mut loaded.instances,
                        &mut statuses,
                        &tracking,
                        &std::collections::HashSet::new(),
                        None,
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
                reload_state_instances_from_disk(&state, loaded, vec![], source, epoch).await;
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
            }
        }
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

    /// A structured row as the poll loop finds it mid-phantom.
    fn phantom_structured_row(id: &str) -> Instance {
        let mut inst = Instance::new(id, "/tmp/test");
        inst.view = crate::session::View::Structured;
        inst.status = Status::Idle;
        inst
    }

    /// Tick-level status decisions over the structured and tmux halves of
    /// #2690 / #2697: a structured row whose live status did not move reports no
    /// transition, a row new since the last snapshot keeps its disk status, a
    /// recently restarted row is forced to Starting (and that transition is
    /// reported), and a failed tmux batch probe holds the live statuses.
    #[test]
    fn apply_tick_status_decisions_cases() {
        use std::collections::{HashMap, HashSet};
        let tmux_row = |status| {
            let mut inst = Instance::new("tmux-session", "/tmp/test");
            inst.status = status;
            inst
        };
        let probed = HashMap::new();
        // (name, row, prev status, restarted, metadata, want status, want transitions)
        let cases = [
            (
                "structured phantom",
                phantom_structured_row("acp-session"),
                Some(Status::Error),
                false,
                Some(&probed),
                Status::Error,
                vec![],
            ),
            (
                "new since last snapshot",
                phantom_structured_row("acp-session"),
                None,
                false,
                Some(&probed),
                Status::Idle,
                vec![],
            ),
            (
                "recently restarted",
                phantom_structured_row("acp-session"),
                Some(Status::Error),
                true,
                Some(&probed),
                Status::Starting,
                vec![(0, Status::Error)],
            ),
            (
                "probe failed, idle on disk",
                tmux_row(Status::Idle),
                Some(Status::Running),
                false,
                None,
                Status::Running,
                vec![],
            ),
            (
                "probe failed, unknown on disk",
                tmux_row(Status::Unknown),
                Some(Status::Error),
                false,
                None,
                Status::Error,
                vec![],
            ),
        ];
        for (name, inst, prev_status, restarted, metadata, want, transitions) in cases {
            let id = inst.id.clone();
            let mut prev: HashMap<_, _> =
                prev_status.map(|s| (id.clone(), s)).into_iter().collect();
            let tracking = prev_status
                .map(|_| (id.clone(), PriorTickTracking::of(&inst)))
                .into_iter()
                .collect();
            let suppressed: HashSet<_> = restarted.then_some(id).into_iter().collect();
            let mut instances = vec![inst];
            apply_tick_status_decisions(
                &mut instances,
                &mut prev,
                &tracking,
                &suppressed,
                metadata,
            );
            assert_eq!(instances[0].status, want, "{name}");
            assert_eq!(
                observed_transitions(&instances, &prev),
                transitions,
                "{name}"
            );
            if prev_status.is_none() {
                assert_eq!(instances[0].live_status_baseline, None, "{name}");
            }
        }

        let mut carried = phantom_structured_row("acp-session");
        carried.live_status_baseline = Some(Status::Error);
        assert!(skip_tmux_decision_for_structured(&mut carried));
        assert_eq!(carried.status, Status::Error);
        assert_eq!(carried.live_status_baseline, Some(carried.status));

        // A row created since the last tick has no live value yet.
        let mut fresh = phantom_structured_row("acp-session");
        fresh.status = Status::Running;
        assert!(skip_tmux_decision_for_structured(&mut fresh));
        assert_eq!(fresh.status, Status::Running);
        assert_eq!(fresh.live_status_baseline, None);

        let mut converted = phantom_structured_row("acp-session");
        converted.last_error = Some(crate::session::TMUX_SESSION_GONE_ERROR.to_string());
        assert!(skip_tmux_decision_for_structured(&mut converted));
        assert_eq!(converted.last_error, None);

        let mut tmux = Instance::new("tmux-session", "/tmp/test");
        tmux.status = Status::Idle;
        tmux.live_status_baseline = Some(Status::Error);
        tmux.last_error = Some(crate::session::TMUX_SESSION_GONE_ERROR.to_string());
        assert!(!skip_tmux_decision_for_structured(&mut tmux));
        assert_eq!(tmux.status, Status::Idle, "disk status is untouched");
        assert_eq!(
            tmux.last_error.as_deref(),
            Some(crate::session::TMUX_SESSION_GONE_ERROR),
            "the poller still needs the tmux error"
        );
    }

    #[test]
    fn seed_tick_tracking_carries_prior_tick_fields_onto_fresh_instance() {
        // `load_all_instances` always resets these `#[serde(skip)]` fields to
        // their defaults, mimicking status_poll_loop's fresh disk load.
        let mut fresh = vec![
            Instance::new("sess-1", "/tmp/seed"),
            Instance::new("sess-unseen", "/tmp/seed"),
        ];

        let confirmed_at = std::time::Instant::now() - std::time::Duration::from_secs(3);
        let mut prev = std::collections::HashMap::new();
        prev.insert(
            fresh[0].id.clone(),
            PriorTickTracking {
                ever_confirmed_present: true,
                unknown_since: Some(confirmed_at),
                detection: crate::session::DetectionState {
                    pending: Some(Status::Idle),
                    ..Default::default()
                },
                ..Default::default()
            },
        );

        seed_tick_tracking(&mut fresh, &prev);

        assert!(
            fresh[0].ever_confirmed_present,
            "prior tick's ever_confirmed_present must seed the fresh instance \
             before update_status_with_metadata runs on it"
        );
        assert_eq!(
            fresh[0].unknown_since,
            Some(confirmed_at),
            "prior tick's unknown_since must seed the fresh instance so the \
             Unknown->Error escalation window can actually accumulate elapsed \
             time across ticks (#2865)"
        );
        assert_eq!(
            fresh[0].detection.pending,
            Some(Status::Idle),
            "prior tick's proposal must seed the fresh instance so the poll \
             that agrees with it can publish it (#3642)"
        );
        assert!(
            !fresh[1].ever_confirmed_present,
            "an unseen id is untouched"
        );
        assert_eq!(fresh[1].unknown_since, None);
        assert_eq!(
            fresh[1].detection,
            crate::session::DetectionState::default()
        );
    }

    fn tmux_available() -> bool {
        crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// #3642.
    #[test]
    #[serial_test::serial]
    fn a_proposal_survives_the_tick_that_reloads_its_row_from_disk() {
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }

        let mut on_disk = Instance::new("aoe_test_3642_tick", "/tmp");
        on_disk.status = Status::Running;
        on_disk.tool = "claude".into();

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
        let cache = crate::tmux::SessionCacheGuard::capture();
        cache.force_present(&[session_name.as_str()]);

        let mut prev = std::collections::HashMap::from([(on_disk.id.clone(), Status::Running)]);
        let mut tracking = std::collections::HashMap::from([(
            on_disk.id.clone(),
            PriorTickTracking::of(&on_disk),
        )]);

        // Each tick reloads disk state while retaining the prior detector state.
        let mut tick = |window_activity: Option<i64>| {
            let metadata = std::collections::HashMap::from([(
                session_name.clone(),
                crate::tmux::PaneMetadata {
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
        apply_tick_status_decisions(
            &mut instances,
            &mut prev,
            &tracking,
            &std::collections::HashSet::new(),
            Some(&metadata),
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
        let storage = Storage::open_unwatched("external").unwrap();
        let mut stored = Instance::new("external idle", "/repo");
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
        let session_name = crate::tmux::Session::generate_name(&stored.id, &stored.title);
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
                "printf '\u{273b} Worked for 1m 52s\n\u{276f}\n'; sleep 300",
            ])
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        crate::tmux::test_helpers::wait_for_pane_command(&session_name, "sleep");
        let cache_guard = crate::tmux::SessionCacheGuard::capture();
        cache_guard.force_present(&[&session_name]);
        let metadata = std::collections::HashMap::from([(
            session_name,
            crate::tmux::PaneMetadata {
                pane_dead: false,
                pane_current_command: Some("claude".into()),
                pane_start_command_is_protected: false,
                pane_pid: None,
                pane_title: None,
                window_activity: None,
                window_size: None,
            },
        )]);
        let mut loaded = load_all_instances(&state.file_watch);
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
        reload_state_instances_from_disk(&state, loaded, vec![], StatusSource::TmuxApplied, 0)
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

    /// #1271: a cascade error string is carried only while the row is still in Error;
    /// any healthy fresh status drops it rather than propagating a stale message.
    #[test]
    fn merge_runtime_fields_carries_last_error_only_while_still_in_error() {
        let merged = |prior_status, fresh_status| {
            let mut prior = Instance::new("seed", "/tmp/seed");
            prior.status = prior_status;
            prior.last_error = Some("recovery cascade: foo".to_string());
            let mut fresh = Instance::new("seed", "/tmp/seed");
            fresh.status = fresh_status;
            fresh.last_error = None;
            merge_runtime_fields(prior, fresh).last_error
        };
        assert_eq!(
            merged(Status::Error, Status::Error).as_deref(),
            Some("recovery cascade: foo")
        );
        assert_eq!(merged(Status::Error, Status::Idle), None);
        assert_eq!(merged(Status::Idle, Status::Idle), None);

        let mut prior = Instance::new("seed", "/tmp/seed");
        prior.acp_load_session_capable = Some(true);
        let merged = merge_runtime_fields(prior, Instance::new("seed", "/tmp/seed"));
        assert_eq!(merged.acp_load_session_capable, Some(true));
    }

    /// `plugin_revival_pending` is `#[serde(skip)]`, so every 2s status-poll tick's fresh
    /// disk load defaults it to `false`; without carrying it here, a revival slower than one
    /// tick would silently stop counting toward its plugin's concurrency cap.
    #[test]
    fn merge_runtime_fields_preserves_plugin_revival_pending() {
        let mut prior = Instance::new("seed", "/tmp/seed");
        prior.plugin_revival_pending = true;

        let fresh = Instance::new("seed", "/tmp/seed");
        let merged = merge_runtime_fields(prior, fresh);

        assert!(merged.plugin_revival_pending);
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
        let mergers: [fn(Instance, Instance) -> Instance; 2] =
            [merge_runtime_fields, |prior, mut fresh| {
                fresh.merge_runtime_from_reload(&prior);
                fresh
            }];
        for merge in mergers {
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
                        exported_default_store: None,
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
            let mut reloaded = merge(prior, fresh);
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
        let mut mergers: Vec<fn(Instance, Instance) -> Instance> = vec![merge_runtime_fields];
        mergers.push(|prior, mut fresh| {
            fresh.merge_runtime_from_reload(&prior);
            fresh
        });
        for merge in mergers {
            // What the repair walk leaves behind on a row with no poller: a re-probe ladder and a
            // store-retry deadline.
            let mut prior = Instance::new("poller-less", "/tmp/poller-less");
            prior.poller_repair.reprobe(now);
            let deadline = now + std::time::Duration::from_secs(30);
            prior.session_id_poller_retry_after = Some(deadline);
            assert!(prior.session_id_poller.is_none());

            let merged = merge(prior, Instance::new("poller-less", "/tmp/poller-less"));

            assert_eq!(
                merged.poller_repair.current_reprobe_delay(),
                Some(std::time::Duration::from_secs(5)),
                "the row keeps its window, or it re-resolves every tick"
            );
            assert_eq!(
                merged.session_id_poller_retry_after,
                Some(deadline),
                "and so does its store-retry deadline"
            );

            // Another process gave the row a new execution while it was waiting: the window was
            // armed for the old one and paces nothing this row can still use.
            let mut prior = Instance::new("replaced", "/tmp/replaced");
            prior.active_execution = execution("launch-1");
            prior.poller_repair.reprobe(now);
            let deadline = now + std::time::Duration::from_secs(30);
            prior.session_id_poller_retry_after = Some(deadline);
            let mut fresh = Instance::new("replaced", "/tmp/replaced");
            fresh.active_execution = execution("launch-2");

            let merged = merge(prior, fresh);

            assert!(
                merged.poller_repair.due(std::time::Instant::now()),
                "a window armed for the superseded execution does not hold the new one back"
            );
            assert_eq!(
                merged.session_id_poller_retry_after, None,
                "and neither does the deadline that went with it"
            );
        }
    }

    /// A profile move can change a row's agent on disk while it holds no execution on either side,
    /// and a poller that watched no execution belongs to an agent just as much. Carrying it would
    /// leave the row watching the previous agent's capture, and the repair walk skips on a
    /// running poller, so nothing else would replace it.
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
        prior.session_id_poller = Some(std::sync::Arc::new(std::sync::Mutex::new(poller)));
        assert!(
            prior.active_execution.is_none(),
            "fixture: no execution on either side"
        );
        let mut fresh = Instance::new("swapped-agent", "/tmp/swapped-agent");
        fresh.tool = "codex".to_string();

        let merged = merge_runtime_fields(prior, fresh);

        assert_eq!(merged.tool, "codex");
        assert!(
            merged.session_id_poller.is_none(),
            "the previous agent's watcher does not follow the row to a new one"
        );
    }

    /// The window a row armed is about the agent and the execution it armed it for. A row that has
    /// nothing to poll holds neither, and keeps its window; a row another agent took over does not
    /// inherit one paced for the previous agent.
    #[test]
    fn a_reload_keeps_the_repair_pacing_of_a_row_on_the_same_runtime() {
        let now = std::time::Instant::now();
        let mut mergers: Vec<fn(Instance, Instance) -> Instance> = vec![merge_runtime_fields];
        mergers.push(|prior, mut fresh| {
            fresh.merge_runtime_from_reload(&prior);
            fresh
        });
        for merge in mergers {
            let mut prior = Instance::new("poller-less", "/tmp/poller-less");
            prior.poller_repair.reprobe(now);
            let deadline = now + std::time::Duration::from_secs(30);
            prior.session_id_poller_retry_after = Some(deadline);

            let merged = merge(prior, Instance::new("poller-less", "/tmp/poller-less"));

            assert_eq!(
                merged.poller_repair.current_reprobe_delay(),
                Some(std::time::Duration::from_secs(5)),
                "the row keeps its window, or it re-resolves every tick"
            );
            assert_eq!(merged.session_id_poller_retry_after, Some(deadline));

            // The same row, another agent: the window paces the previous agent's capture.
            let mut prior = Instance::new("other-agent", "/tmp/other-agent");
            prior.tool = "claude".to_string();
            prior.poller_repair.reprobe(now);
            prior.session_id_poller_retry_after = Some(now + std::time::Duration::from_secs(30));
            let mut fresh = Instance::new("other-agent", "/tmp/other-agent");
            fresh.tool = "codex".to_string();

            let merged = merge(prior, fresh);

            assert_eq!(merged.tool, "codex");
            assert!(
                merged.poller_repair.due(std::time::Instant::now()),
                "a window armed for the previous agent does not hold this one back"
            );
            assert_eq!(
                merged.session_id_poller_retry_after, None,
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
                (metadata.dev(), metadata.ino())
            };
            assert_ne!(identity(&root.join("main")), identity(outside.path()));
            for name in ["external-a", "external-z"] {
                std::os::unix::fs::symlink(outside.path(), root.join(name)).unwrap();
            }
            Storage::open_unwatched("external-a")
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
            let mut loaded = load_all_instances(&state.file_watch);
            if matches!(source, StatusSource::TmuxApplied) {
                apply_supplementary_tick(&mut loaded.cache, &prior, None);
            }
            reload_state_instances_from_disk(&state, loaded, vec![], source, 0).await;
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
                assert!(row.session_id_poller.is_none());
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
            let mut loaded = load_all_instances(&state.file_watch);
            if matches!(source, StatusSource::TmuxApplied) {
                apply_supplementary_tick(&mut loaded.cache, &prior, None);
            }
            reload_state_instances_from_disk(&state, loaded, vec![], source, 0).await;
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
        let storage = Storage::open_unwatched("external").unwrap();
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
                let mut loaded = load_all_instances(&state.file_watch);
                if matches!(source, StatusSource::TmuxApplied) {
                    apply_supplementary_tick(&mut loaded.cache, &prior, None);
                }
                reload_state_instances_from_disk(&state, loaded, vec![], source, 0).await;
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
        let storage = Storage::open_unwatched("external").unwrap();
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
            let loaded = load_all_instances(&state.file_watch);
            reload_state_instances_from_disk(&state, loaded, vec![], StatusSource::DiskOnly, 0)
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
