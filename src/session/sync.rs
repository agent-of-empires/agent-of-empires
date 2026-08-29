//! Drain pollers' session-id mpsc channels and persist observations.
//!
//! Shared by the TUI tick (`apply_session_id_updates`) and the daemon's
//! `status_poll_loop`. Without the daemon-side caller, sessions running
//! under `aoe serve` without an attached TUI never persist post-`/clear`
//! sids through the channel and `sessions.json` stays stale until the
//! next launch's resume-time verify (#2291).
//!
//! The helper takes `&mut [Instance]` and mutates the slice's per-instance
//! `agent_session_id` and `resume_probe_failed_sid` directly. It does NOT
//! take any tokio lock and is safe to call from within `spawn_blocking`.
//! Daemon callers MUST satisfy the lock-ordering invariant in
//! `storage.rs:46`: snapshot the instances under a brief read lock, run the
//! helper on the snapshot inside `spawn_blocking`, then reapply the
//! mutations to live state under a brief write lock.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::file_watch::FileWatchService;
use crate::session::capture::validated_session_id;
use crate::session::poller::{SessionIdGuard, SessionIdObservation};
use crate::session::storage::Storage;
use crate::session::{
    persist_omp_session_to_storage, persist_session_to_storage, Instance, ResumeIntent, SidWrite,
    Status,
};

/// Per-tick result of [`drain_and_persist_session_ids`]. Lists touched
/// instance IDs grouped by the persistence outcome so a caller holding an
/// auxiliary in-memory mirror (e.g. the TUI's `instances` map) can re-sync
/// each affected entry from the slice.
#[derive(Debug, Default, Clone)]
pub(crate) struct SessionIdSyncOutcome {
    /// Instances whose `agent_session_id` was updated to a poller-observed
    /// value (CAS-Applied; `resume_probe_failed_sid` is also reset).
    pub(crate) applied: Vec<String>,
    /// Instances whose in-memory state was reloaded from disk after a
    /// CAS-Skipped persist (peer wrote a different sid first).
    pub(crate) rolled_back: Vec<String>,
    /// Instances whose poller-observed sid was rejected (validation failed,
    /// matched a cleared sid in the per-instance exclusion set, or the
    /// persist returned Failed). The tmux env mirror is republished from
    /// the in-memory value for these so the on_change publish is overwritten.
    pub(crate) filtered: Vec<String>,
}

impl SessionIdSyncOutcome {
    pub(crate) fn touched(&self) -> bool {
        !self.applied.is_empty() || !self.rolled_back.is_empty() || !self.filtered.is_empty()
    }
}

type TmuxEnvSet = (String, String, String);
type TmuxEnvUnset = (String, String);

fn tmux_session_id_env_updates<'a>(
    instances: impl IntoIterator<Item = &'a Instance>,
    mut session_names: impl FnMut(&Instance) -> Vec<String>,
) -> (Vec<TmuxEnvSet>, Vec<TmuxEnvUnset>) {
    let mut set_batch = Vec::new();
    let mut unset_batch = Vec::new();
    for instance in instances {
        for tmux_name in session_names(instance) {
            match instance.operational_agent_session_id() {
                Some(sid) => {
                    set_batch.push((
                        tmux_name.clone(),
                        crate::tmux::env::AOE_INSTANCE_ID_KEY.to_string(),
                        instance.id.clone(),
                    ));
                    set_batch.push((
                        tmux_name,
                        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
                        sid.to_string(),
                    ));
                }
                None => unset_batch.push((
                    tmux_name,
                    crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
                )),
            }
        }
    }
    (set_batch, unset_batch)
}

fn tmux_session_id_env_names(
    instance: &Instance,
    live: &crate::tmux::LiveSessionSnapshot,
    observations: &[TmuxOwnershipObservation],
    allow_ownerless_fallback: bool,
) -> Vec<String> {
    if instance.operational_agent_session_id().is_none() {
        return Vec::new();
    }

    let mut targets: Vec<String> = observations
        .iter()
        .filter(|(name, owner, _)| {
            owner.as_deref() == Some(instance.id.as_str()) && !live.pane_dead(name)
        })
        .map(|(name, _, _)| name.clone())
        .collect();
    if !allow_ownerless_fallback {
        return targets;
    }

    if let Some(candidates) = crate::tmux::live_any_kind_names_for_id_in(live, &instance.id) {
        let mut ownerless = None;
        let mut ambiguous = false;
        for candidate in candidates {
            match observations.iter().find(|(name, _, _)| name == &candidate) {
                Some((_, Some(owner), _)) if owner == &instance.id => {}
                Some((_, None, _)) if ownerless.is_none() => ownerless = Some(candidate),
                _ => {
                    ambiguous = true;
                    break;
                }
            }
        }
        if !ambiguous {
            if let Some(candidate) = ownerless {
                targets.push(candidate);
            }
        }
    }
    targets.sort();
    targets.dedup();
    targets
}

fn partition_unambiguous_instances(instances: &[Instance]) -> (Vec<&Instance>, HashSet<&str>) {
    let mut id_counts: HashMap<&str, usize> = HashMap::new();
    let mut sid_counts: HashMap<&str, usize> = HashMap::new();
    for instance in instances {
        *id_counts.entry(instance.id.as_str()).or_default() += 1;
        if let Some(sid) = instance.operational_agent_session_id() {
            *sid_counts.entry(sid).or_default() += 1;
        }
    }
    let mut ambiguous_ids: HashSet<&str> = id_counts
        .iter()
        .filter_map(|(id, count)| (*count > 1).then_some(*id))
        .collect();
    ambiguous_ids.extend(instances.iter().filter_map(|instance| {
        instance.operational_agent_session_id().and_then(|sid| {
            (sid_counts.get(sid).copied().unwrap_or_default() > 1).then_some(instance.id.as_str())
        })
    }));
    let trusted = instances
        .iter()
        .filter(|instance| !ambiguous_ids.contains(instance.id.as_str()))
        .collect();
    (trusted, ambiguous_ids)
}

type TmuxOwnershipObservation = (String, Option<String>, Option<String>);

fn tmux_id_suffix(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn ambiguous_tmux_id_suffixes(instances: &[Instance]) -> HashSet<&str> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for instance in instances {
        *counts.entry(tmux_id_suffix(&instance.id)).or_default() += 1;
    }
    counts
        .into_iter()
        .filter_map(|(suffix, count)| (count > 1).then_some(suffix))
        .collect()
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ClearedTmuxOwnership {
    entries: Vec<(String, String)>,
}

pub(crate) fn with_tmux_ownership_lock<T>(
    operation: impl FnOnce() -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let _lock = crate::session::storage::acquire_tmux_ownership_lock()?;
    operation()
}

fn read_tmux_ownership_observations(
    names: &[String],
) -> anyhow::Result<Vec<TmuxOwnershipObservation>> {
    let names: Vec<&str> = names
        .iter()
        .filter(|name| crate::tmux::is_aoe_session(name))
        .map(String::as_str)
        .collect();
    let rows = crate::tmux::env::get_hidden_env_keys_batch_strict(
        &names,
        &[
            crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            crate::tmux::env::AOE_INSTANCE_ID_KEY,
        ],
    )?;
    Ok(rows
        .into_iter()
        .map(|(name, values)| {
            let mut values = values.into_iter();
            let captured = values.next().flatten();
            let owner = values.next().flatten();
            (name, owner, captured)
        })
        .collect())
}

fn stale_tmux_ownership_targets<'a>(
    trusted_instances: impl IntoIterator<Item = &'a Instance>,
    observations: impl IntoIterator<Item = TmuxOwnershipObservation>,
) -> Vec<String> {
    let operational: HashSet<(&str, &str)> = trusted_instances
        .into_iter()
        .filter_map(|instance| {
            instance
                .operational_agent_session_id()
                .map(|sid| (instance.id.as_str(), sid))
        })
        .collect();
    observations
        .into_iter()
        .filter_map(|(session, owner, captured)| {
            let captured = captured?;
            let valid = owner
                .as_deref()
                .is_some_and(|owner| operational.contains(&(owner, captured.as_str())));
            (!valid).then_some(session)
        })
        .collect()
}

fn cleared_tmux_ownership(
    instances: &[Instance],
    targets: &[String],
    observations: &[TmuxOwnershipObservation],
) -> ClearedTmuxOwnership {
    let target_set: HashSet<&str> = targets.iter().map(String::as_str).collect();
    let (trusted, _) = partition_unambiguous_instances(instances);
    let operational_ids: HashSet<&str> = trusted
        .into_iter()
        .filter(|instance| instance.operational_agent_session_id().is_some())
        .map(|instance| instance.id.as_str())
        .collect();
    let entries = observations
        .iter()
        .filter_map(|(session, owner, captured)| {
            let owner = owner.as_deref()?;
            let captured = captured.as_ref()?;
            (target_set.contains(session.as_str()) && operational_ids.contains(owner))
                .then(|| (session.clone(), captured.clone()))
        })
        .collect();
    ClearedTmuxOwnership { entries }
}

fn remove_tmux_session_id_ownership(
    instances: &[Instance],
    targets: &[String],
    observations: &[TmuxOwnershipObservation],
) -> anyhow::Result<ClearedTmuxOwnership> {
    let cleared = cleared_tmux_ownership(instances, targets, observations);
    let refs: Vec<(&str, &str)> = targets
        .iter()
        .map(|session| {
            (
                session.as_str(),
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            )
        })
        .collect();
    if let Err(error) = crate::tmux::env::remove_hidden_env_batch(&refs) {
        if let Err(repair_error) = restore_tmux_session_id_ownership_locked(&cleared) {
            anyhow::bail!("{error}; ownership repair failed: {repair_error}");
        }
        return Err(error);
    }
    Ok(cleared)
}

fn reconcile_tmux_session_id_ownership_env_locked(
    instances: &[Instance],
) -> anyhow::Result<ClearedTmuxOwnership> {
    let names = crate::tmux::session_names_strict()?;
    let live = crate::tmux::LiveSessionSnapshot::from_names(names.clone());
    let observations = read_tmux_ownership_observations(&names)?;
    let (trusted, _) = partition_unambiguous_instances(instances);
    let ambiguous_suffixes = ambiguous_tmux_id_suffixes(instances);
    let (set_batch, unset_batch) =
        tmux_session_id_env_updates(trusted.iter().copied(), |instance| {
            tmux_session_id_env_names(
                instance,
                &live,
                &observations,
                !ambiguous_suffixes.contains(tmux_id_suffix(&instance.id)),
            )
        });
    let desired: HashSet<&str> = set_batch
        .iter()
        .filter(|(_, key, _)| key == crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY)
        .map(|(session, _, _)| session.as_str())
        .collect();
    let mut targets =
        stale_tmux_ownership_targets(trusted.iter().copied(), observations.iter().cloned());
    targets.retain(|session| !desired.contains(session.as_str()));
    targets.extend(unset_batch.iter().map(|(session, _)| session.clone()));
    targets.sort();
    targets.dedup();

    let cleared = remove_tmux_session_id_ownership(instances, &targets, &observations)?;
    let refs: Vec<(&str, &str, &str)> = set_batch
        .iter()
        .map(|(session, key, value)| (session.as_str(), key.as_str(), value.as_str()))
        .collect();
    if let Err(error) = crate::tmux::env::set_hidden_env_batch(&refs) {
        if let Err(repair_error) = restore_tmux_session_id_ownership_locked(&cleared) {
            anyhow::bail!("{error}; ownership repair failed: {repair_error}");
        }
        return Err(error);
    }
    Ok(cleared)
}

/// Reconcile durable session IDs across extant tmux sessions.
pub(crate) fn sync_tmux_session_id_env<'a>(
    _instances: impl IntoIterator<Item = &'a Instance>,
    _live: &crate::tmux::LiveSessionSnapshot,
) -> anyhow::Result<()> {
    reconcile_all_profiles_tmux_session_id_ownership_env()
}

fn owned_tmux_ownership_targets(
    instance_ids: &HashSet<&str>,
    observations: impl IntoIterator<Item = TmuxOwnershipObservation>,
) -> Vec<String> {
    observations
        .into_iter()
        .filter_map(|(session, owner, captured)| {
            (captured.is_some()
                && owner
                    .as_deref()
                    .is_some_and(|owner| instance_ids.contains(owner)))
            .then_some(session)
        })
        .collect()
}

/// Remove ownership for rows immediately before deleting them.
pub(crate) fn clear_tmux_session_id_ownership_for_instances_locked(
    instances: &[Instance],
) -> anyhow::Result<ClearedTmuxOwnership> {
    let names = crate::tmux::session_names_strict()?;
    let observations = read_tmux_ownership_observations(&names)?;
    let instance_ids: HashSet<&str> = instances
        .iter()
        .map(|instance| instance.id.as_str())
        .collect();
    let targets = owned_tmux_ownership_targets(&instance_ids, observations.iter().cloned());
    remove_tmux_session_id_ownership(instances, &targets, &observations)
}

pub(crate) fn restore_tmux_session_id_ownership_locked(
    cleared: &ClearedTmuxOwnership,
) -> anyhow::Result<()> {
    let refs: Vec<(&str, &str, &str)> = cleared
        .entries
        .iter()
        .map(|(session, value)| {
            (
                session.as_str(),
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                value.as_str(),
            )
        })
        .collect();
    crate::tmux::env::set_hidden_env_batch(&refs)
}
fn load_profile_instances_excluding(excluded: Option<&str>) -> anyhow::Result<Vec<Instance>> {
    let excluded_identity = excluded
        .map(|profile| {
            let storage = Storage::open_unwatched(profile)
                .map_err(|error| anyhow::anyhow!("open profile {profile}: {error}"))?;
            let directory = storage.sessions_path().parent().ok_or_else(|| {
                anyhow::anyhow!(
                    "sessions path has no profile directory: {}",
                    storage.sessions_path().display()
                )
            })?;
            directory
                .canonicalize()
                .map_err(|error| anyhow::anyhow!("resolve profile {profile}: {error}"))
        })
        .transpose()?;

    let mut instances = Vec::new();
    let mut visited = HashSet::new();
    for profile in crate::session::list_profiles()? {
        let storage = Storage::open_unwatched(&profile)
            .map_err(|error| anyhow::anyhow!("open profile {profile}: {error}"))?;
        let directory = storage.sessions_path().parent().ok_or_else(|| {
            anyhow::anyhow!(
                "sessions path has no profile directory: {}",
                storage.sessions_path().display()
            )
        })?;
        let identity = directory
            .canonicalize()
            .map_err(|error| anyhow::anyhow!("resolve profile {profile}: {error}"))?;
        if !visited.insert(identity.clone()) {
            continue;
        }
        if excluded_identity.as_ref() == Some(&identity) {
            continue;
        }
        let mut rows = storage
            .load_strict()
            .map_err(|error| anyhow::anyhow!("load profile {profile}: {error}"))?;
        instances.append(&mut rows);
    }
    Ok(instances)
}
pub(crate) fn instance_ids_excluding_profile(excluded: &str) -> anyhow::Result<HashSet<String>> {
    Ok(load_profile_instances_excluding(Some(excluded))?
        .into_iter()
        .map(|instance| instance.id)
        .collect())
}

/// Find a durable SID reservation outside `excluded` while the caller holds
/// the app-global ownership lock. Strict loads make unreadable ownership state
/// an error rather than licensing a cross-profile capture or launch.
pub(crate) fn reserved_sid_holder_excluding_profile(
    excluded: &str,
    sid: &str,
) -> anyhow::Result<Option<String>> {
    Ok(load_profile_instances_excluding(Some(excluded))?
        .into_iter()
        .find(|instance| instance.reserves_agent_session_id(sid))
        .map(|instance| instance.id))
}
pub(crate) fn reconcile_all_profiles_tmux_session_id_ownership_env_locked(
) -> anyhow::Result<ClearedTmuxOwnership> {
    let instances = load_profile_instances_excluding(None)?;
    reconcile_tmux_session_id_ownership_env_locked(&instances)
}

/// Strict ownership reconciliation across every profile.
pub(crate) fn reconcile_all_profiles_tmux_session_id_ownership_env() -> anyhow::Result<()> {
    with_tmux_ownership_lock(|| {
        reconcile_all_profiles_tmux_session_id_ownership_env_locked()?;
        Ok(())
    })
}

struct Update {
    id: String,
    sid: String,
    expected_prior: Option<String>,
    profile: String,
    guard: SessionIdGuard,
    observation: SessionIdObservation,
}

struct Rollback {
    id: String,
    disk_sid: Option<String>,
    disk_failed_sid: Option<String>,
    disk_omp_capture_generation: Option<String>,
}

/// Drain and persist captures, acquiring each session's lifecycle flock around
/// its final compare-and-set.
pub(crate) fn drain_and_persist_session_ids(
    instances: &mut [Instance],
    file_watch: &Arc<FileWatchService>,
) -> SessionIdSyncOutcome {
    drain_and_persist_session_ids_inner(instances, file_watch, false)
}

/// Variant for a one-session caller that already holds its lifecycle flock.
pub(crate) fn drain_and_persist_session_ids_lifecycle_locked(
    instances: &mut [Instance],
    file_watch: &Arc<FileWatchService>,
) -> SessionIdSyncOutcome {
    debug_assert_eq!(instances.len(), 1);
    drain_and_persist_session_ids_inner(instances, file_watch, true)
}

fn drain_and_persist_session_ids_inner(
    instances: &mut [Instance],
    file_watch: &Arc<FileWatchService>,
    lifecycle_already_locked: bool,
) -> SessionIdSyncOutcome {
    let mut updates: Vec<Update> = Vec::with_capacity(instances.len());
    let mut filtered_ids: HashSet<String> = HashSet::with_capacity(instances.len());

    // Frozen pre-update ownership snapshot. Collision checks must read this,
    // never a map mutated mid-loop: with two pollers that transiently cross
    // streams (A reports B's id while B reports A's), a dynamic map would
    // accept or reject by slice iteration order. The snapshot rejects every
    // cross-claim deterministically (see #2708).
    let mut sid_owners: HashMap<String, String> = HashMap::with_capacity(instances.len());
    for inst in instances.iter() {
        for sid in inst.reserved_agent_session_ids() {
            sid_owners
                .entry(sid.to_string())
                .or_insert_with(|| inst.id.clone());
        }
    }
    for inst in instances.iter() {
        let Some(observation) = drain_poller(inst) else {
            continue;
        };
        let observed_sid = observation.sid.clone();
        let Some(sid) = validated_session_id(observed_sid) else {
            acknowledge_poller_observation(inst, &observation);
            filtered_ids.insert(inst.id.clone());
            continue;
        };
        // Unguarded and legacy filesystem scans from a stopped session can
        // belong to a peer sharing the cwd. A generation-typed OMP result is
        // bound to the exact old pane and must remain eligible for the
        // restart's post-join final flush.
        if matches!(inst.status, Status::Stopped)
            && !matches!(&observation.guard, SessionIdGuard::OmpGeneration(_))
            && inst.agent_session_id.as_deref() != Some(sid.as_str())
        {
            tracing::debug!(
                target: "session.sync",
                instance = %inst.id,
                sid = %sid,
                "Ignoring poller-reported sid for stopped session",
            );
            acknowledge_poller_observation(inst, &observation);
            filtered_ids.insert(inst.id.clone());
            continue;
        }
        // An explicit set-session-id pin is authoritative until the session
        // itself launches (which promotes Use -> Default). While pinned, the
        // poller must not overwrite it, even with an unowned fresher jsonl the
        // collision guard below would otherwise wave through (#2708 invariant 1).
        if let ResumeIntent::Use(pinned) = &inst.resume_intent {
            if sid != *pinned {
                tracing::debug!(
                    target: "session.sync",
                    instance = %inst.id,
                    sid = %sid,
                    pinned = %pinned,
                    "Ignoring poller-reported sid: contradicts explicit set-session-id pin",
                );
                acknowledge_poller_observation(inst, &observation);
                filtered_ids.insert(inst.id.clone());
                continue;
            }
        }
        // Never adopt an id another instance already owns: that is the
        // same-cwd cross-assignment drift itself (#2708 symptom 1).
        if let Some(owner) = sid_owners.get(sid.as_str()) {
            if owner != &inst.id {
                tracing::warn!(
                    target: "session.sync",
                    instance = %inst.id,
                    sid = %sid,
                    owner = %owner,
                    "Ignoring poller-reported sid already owned by another instance",
                );
                acknowledge_poller_observation(inst, &observation);
                filtered_ids.insert(inst.id.clone());
                continue;
            }
        }
        if inst.retroactive_capture_excludes.contains(&sid) {
            tracing::debug!(
                target: "session.sync",
                instance = %inst.id,
                sid = %sid,
                "Ignoring poller-reported sid: in retroactive_capture_excludes",
            );
            acknowledge_poller_observation(inst, &observation);
            filtered_ids.insert(inst.id.clone());
            continue;
        }
        if inst.agent_session_id.as_deref() == Some(sid.as_str()) {
            acknowledge_poller_observation(inst, &observation);
            continue;
        }
        updates.push(Update {
            id: inst.id.clone(),
            sid,
            expected_prior: inst.agent_session_id.clone(),
            profile: inst.source_profile.clone(),
            guard: observation.guard.clone(),
            observation,
        });
    }

    // Reject, don't arbitrate: if two same-cwd peers both claim the same
    // currently-unowned sid in one tick (neither is in the frozen snapshot, so
    // the collision guard passed both), picking a winner by iteration order is
    // silent misassignment. Drop every claimant and defer; the next tick sees
    // the real owner's anchor advance and the collision guard resolves it (#2708).
    let mut sid_claim_counts: HashMap<String, usize> = HashMap::with_capacity(updates.len());
    for update in &updates {
        *sid_claim_counts.entry(update.sid.clone()).or_insert(0) += 1;
    }
    updates.retain(|update| {
        if sid_claim_counts.get(&update.sid).copied().unwrap_or(0) > 1 {
            tracing::warn!(
                target: "session.sync",
                instance = %update.id,
                sid = %update.sid,
                "Ignoring poller-reported sid claimed by multiple instances this tick",
            );
            acknowledge_poller_observation_for(instances, &update.id, &update.observation);
            filtered_ids.insert(update.id.clone());
            false
        } else {
            true
        }
    });

    if updates.is_empty() && filtered_ids.is_empty() {
        return SessionIdSyncOutcome::default();
    }

    let mut to_apply: Vec<(String, String)> = Vec::with_capacity(updates.len());
    let mut to_rollback: Vec<Rollback> = Vec::with_capacity(updates.len());

    let mut capture_generations: Vec<(String, u64)> = Vec::with_capacity(updates.len());
    for update in &updates {
        let ownership: anyhow::Result<_> = if lifecycle_already_locked {
            Ok(None)
        } else {
            (|| {
                let storage = Storage::new(&update.profile, file_watch.clone())?;
                let lifecycle_lock = storage.acquire_instance_lifecycle_lock(&update.id)?;
                let generation = storage.update(|instances, _groups| {
                    let Some(instance) = instances
                        .iter_mut()
                        .find(|instance| instance.id == update.id)
                    else {
                        anyhow::bail!("session disappeared before capture");
                    };
                    instance
                        .try_acquire_lifecycle_reservation(
                            crate::session::LifecycleOperation::Capture,
                            Instance::LIFECYCLE_RESERVATION_TTL,
                            chrono::Utc::now(),
                        )
                        .map_err(|error| anyhow::anyhow!("capture blocked: {error}"))
                })?;
                Ok(Some((storage, lifecycle_lock, generation)))
            })()
        };
        let mut outcome = match &ownership {
            Err(error) => {
                tracing::warn!(
                    target: "session.sync",
                    instance = %update.id,
                    "capture ownership failed: {error}",
                );
                SidWrite::Failed
            }
            Ok(_) => match &update.guard {
                SessionIdGuard::Unguarded => persist_session_to_storage(
                    &update.profile,
                    &update.id,
                    &update.sid,
                    update.expected_prior.as_deref(),
                    file_watch,
                ),
                SessionIdGuard::OmpLegacy => persist_omp_session_to_storage(
                    &update.profile,
                    &update.id,
                    &update.sid,
                    update.expected_prior.as_deref(),
                    None,
                    file_watch,
                ),
                SessionIdGuard::OmpGeneration(generation) => persist_omp_session_to_storage(
                    &update.profile,
                    &update.id,
                    &update.sid,
                    update.expected_prior.as_deref(),
                    Some(generation),
                    file_watch,
                ),
            },
        };
        if let Ok(Some((storage, _lifecycle_lock, generation))) = ownership {
            let released = storage.update(|instances, _groups| {
                let Some(instance) = instances
                    .iter_mut()
                    .find(|instance| instance.id == update.id)
                else {
                    return Ok(false);
                };
                Ok(instance.release_lifecycle_reservation_if_owned(
                    crate::session::LifecycleOperation::Capture,
                    generation,
                ))
            });
            match released {
                Ok(true) => capture_generations.push((update.id.clone(), generation)),
                Ok(false) => {
                    tracing::warn!(
                        target: "session.sync",
                        instance = %update.id,
                        "capture lost its lifecycle reservation before release",
                    );
                    outcome = SidWrite::Failed;
                }
                Err(error) => {
                    tracing::warn!(
                        target: "session.sync",
                        instance = %update.id,
                        "capture reservation release failed: {error}",
                    );
                    outcome = SidWrite::Failed;
                }
            }
        }
        match outcome {
            SidWrite::Applied => {
                acknowledge_poller_observation_for(instances, &update.id, &update.observation);
                to_apply.push((update.id.clone(), update.sid.clone()));
            }
            SidWrite::Skipped => {
                request_poller_retry(instances, &update.id);
                if let Some(rb) = reload_skipped_from_disk(&update.profile, &update.id, file_watch)
                {
                    if rb.disk_sid.as_deref() == Some(update.sid.as_str()) {
                        acknowledge_poller_observation_for(
                            instances,
                            &update.id,
                            &update.observation,
                        );
                    }
                    to_rollback.push(rb);
                } else {
                    tracing::warn!(
                        target: "session.sync",
                        instance = %update.id,
                        "Skipped reload failed; deferring env reconcile",
                    );
                }
            }
            SidWrite::Failed => {
                request_poller_retry(instances, &update.id);
                filtered_ids.insert(update.id.clone());
            }
        }
    }
    for (id, generation) in &capture_generations {
        if let Some(inst) = instances.iter_mut().find(|instance| instance.id == *id) {
            inst.lifecycle_generation = *generation;
            inst.lifecycle_reservation = None;
        }
    }

    for (id, sid) in &to_apply {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == *id) {
            inst.agent_session_id = Some(sid.clone());
            inst.resume_probe_failed_sid = None;
        }
    }
    for rb in &to_rollback {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == rb.id) {
            inst.agent_session_id = rb.disk_sid.clone();
            inst.resume_probe_failed_sid = rb.disk_failed_sid.clone();
            inst.omp_capture_generation = rb.disk_omp_capture_generation.clone();
        }
    }

    publish_tmux_env();

    SessionIdSyncOutcome {
        applied: to_apply.into_iter().map(|(id, _)| id).collect(),
        rolled_back: to_rollback.into_iter().map(|r| r.id).collect(),
        filtered: filtered_ids.into_iter().collect(),
    }
}

/// Bound for a non-attaching CLI launch (`aoe session start` / import
/// `--launch`) to wait for its poller. Covers the poller's first few ~2s
/// ticks (`POLL_INITIAL_INTERVAL`) while keeping the foreground bounded.
pub(crate) const CLI_SESSION_ID_CAPTURE_TIMEOUT: Duration = Duration::from_secs(8);

/// Bound for `aoe add --launch`, which drains only after `tmux attach`
/// returns: the poller observed for the whole attached session, so the id is
/// almost always already queued and this only covers a detach before tick 1.
pub(crate) const CLI_ATTACHED_SESSION_ID_CAPTURE_TIMEOUT: Duration = Duration::from_secs(2);

/// How often the bounded CLI capture re-drains the poller while waiting. Short
/// enough to land the id promptly once the poller observes it, coarse enough
/// not to busy-spin the storage flock between the poller's ~2s ticks.
const CLI_CAPTURE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Bounded, blocking post-launch capture of `agent_session_id` for the CLI
/// one-shot launch paths.
///
/// The TUI event loop and the `aoe serve` daemon drain each instance's
/// session-id poller on every tick; a bare CLI launch has no such loop, so for
/// a capture-deferred agent (every resume-capable agent except claude and
/// preassigned opencode) the poller-observed id was never persisted and
/// resume/recovery silently broke. `finalize_launch` has already started the
/// poller by the time this runs, so this simply drives the SAME
/// [`drain_and_persist_session_ids`] path the TUI/daemon use, on a
/// single-instance slice, until the id lands or `timeout` elapses.
///
/// Instances with no poller (`ResumeStrategy::Unsupported`, a sandboxed agent
/// whose container is not up, or a budget-exhausted poller) impose no wait.
/// Every poller-backed exit stops and joins the producer, whose Stop boundary
/// performs one final poll, then drains the resulting correction before the
/// one-shot CLI drops the instance.
///
/// Called only from CLI one-shot paths. When `notify` is set it prints a
/// one-line waiting notice after ~1s; parallel `restart --all` workers pass
/// `false` so their notices do not interleave. The timeout note is always
/// printed when the final poll still produced no session id.
pub(crate) fn capture_launched_session_id_blocking(
    inst: &mut Instance,
    file_watch: &Arc<FileWatchService>,
    timeout: Duration,
    notify: bool,
) {
    if inst.session_id_poller.is_none() {
        return;
    }

    let start = Instant::now();
    let deadline = start + timeout;
    let mut notified = false;
    loop {
        // Reuse the fleet drain on a one-element slice. Each pass empties the
        // receiver and keeps its newest observation, so a correction queued
        // behind an obsolete value wins without an intermediate CAS write.
        // Intentionally sleepless: a pass only re-loops while it consumed a real
        // observation, and the producer poller's own poll cadence bounds how
        // fast the channel refills, so the burst self-terminates on an empty
        // channel before the outer sleep below.
        while drain_and_persist_session_ids(std::slice::from_mut(inst), file_watch).touched()
            && Instant::now() < deadline
        {}
        if inst.agent_session_id.is_some() || Instant::now() >= deadline {
            break;
        }
        if notify && !notified && start.elapsed() >= Duration::from_secs(1) {
            eprintln!(
                "{} is up; waiting for it to report its session id…",
                inst.tool
            );
            notified = true;
        }
        std::thread::sleep(CLI_CAPTURE_POLL_INTERVAL);
    }

    // Stop joins the producer and performs its final poll before this last
    // drain, closing the drop-time window where `/clear` or `/new` could queue
    // a replacement sid after the apparent success above.
    inst.stop_and_flush_poller();
    if inst.agent_session_id.is_none() {
        let title: String = inst.title.chars().filter(|c| !c.is_control()).collect();
        eprintln!(
            "Note: session \"{}\" ({}) did not report a session id in time; resume stays unavailable until the TUI or `aoe serve` observes it.",
            title, inst.tool
        );
        tracing::warn!(
            target: "session.sync",
            instance = %inst.id,
            tool = %inst.tool,
            "CLI launch timed out waiting for agent_session_id; resume stays unavailable until a TUI or daemon re-observes it via its own poller",
        );
    }
}

/// Lease one poller's newest observation from its sticky mailbox.
fn drain_poller(inst: &Instance) -> Option<SessionIdObservation> {
    let arc = inst.session_id_poller.as_ref()?;
    let mut guard = match arc.lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::warn!(
                target: "session.sync",
                instance = %inst.id,
                "session_id_poller mutex poisoned; recovering inner guard",
            );
            poisoned.into_inner()
        }
    };
    guard
        .latest_observation()
        .map(|(_instance_id, observation)| observation)
}

fn acknowledge_poller_observation(inst: &Instance, observation: &SessionIdObservation) {
    let Some(poller) = inst.session_id_poller.as_ref() else {
        return;
    };
    let expected = (inst.id.clone(), observation.clone());
    match poller.lock() {
        Ok(mut guard) => {
            guard.acknowledge_observation(&expected);
        }
        Err(poisoned) => {
            tracing::warn!(
                target: "session.sync",
                instance = %inst.id,
                "session_id_poller mutex poisoned while acknowledging observation; recovering inner guard",
            );
            poisoned.into_inner().acknowledge_observation(&expected);
        }
    }
}

fn acknowledge_poller_observation_for(
    instances: &[Instance],
    id: &str,
    observation: &SessionIdObservation,
) {
    if let Some(inst) = instances.iter().find(|inst| inst.id == id) {
        acknowledge_poller_observation(inst, observation);
    }
}

fn request_poller_retry(instances: &[Instance], id: &str) {
    let Some(poller) = instances
        .iter()
        .find(|inst| inst.id == id)
        .and_then(|inst| inst.session_id_poller.as_ref())
    else {
        return;
    };
    match poller.lock() {
        Ok(guard) => guard.retry_last_observation(),
        Err(poisoned) => {
            tracing::warn!(
                target: "session.sync",
                instance = %id,
                "session_id_poller mutex poisoned while requesting retry; recovering inner guard",
            );
            poisoned.into_inner().retry_last_observation();
        }
    }
}

fn reload_skipped_from_disk(
    profile: &str,
    id: &str,
    file_watch: &Arc<FileWatchService>,
) -> Option<Rollback> {
    let storage = Storage::new(profile, file_watch.clone()).ok()?;
    let disk_insts = storage.load().ok()?;
    let disk_inst = disk_insts.iter().find(|i| i.id == id)?;
    Some(Rollback {
        id: id.to_string(),
        disk_sid: disk_inst.agent_session_id.clone(),
        disk_failed_sid: disk_inst.resume_probe_failed_sid.clone(),
        disk_omp_capture_generation: disk_inst.omp_capture_generation.clone(),
    })
}

fn publish_tmux_env() {
    if let Err(error) = reconcile_all_profiles_tmux_session_id_ownership_env() {
        tracing::warn!(target: "session.sync", "Post-CAS env reconcile failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_watch::FileWatchService;
    use crate::session::poller::SessionPoller;
    use crate::session::storage::Storage;
    use crate::session::test_support::EnvGuard;
    use crate::session::{GroupTree, Instance};
    use serial_test::serial;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::{tempdir, TempDir};

    /// Points `HOME` (and, on Linux/macOS, `XDG_CONFIG_HOME`) at `temp`
    /// for the current test body. See [`crate::session::test_support`]:
    /// the snapshot/restore is `EnvGuard`'s, so a non-UTF-8 prior value
    /// round-trips instead of being dropped (#2751).
    fn storage_home_guard(temp: &TempDir) -> EnvGuard {
        let pairs: Vec<(&'static str, PathBuf)> = vec![
            ("HOME", temp.path().to_path_buf()),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            ("XDG_CONFIG_HOME", temp.path().join(".config")),
        ];
        EnvGuard::set(&pairs)
    }

    #[test]
    fn tmux_env_plan_publishes_only_operational_session_ids() {
        let stale = "019342ab-1234-7def-8901-dddddddddddd";
        for (tool, publishes) in [("claude", true), ("qwen", false), ("kiro", false)] {
            let mut instance = Instance::new("owner", "/tmp/x");
            instance.tool = tool.to_string();
            instance.agent_session_id = Some(stale.to_string());

            let (set_batch, unset_batch) =
                tmux_session_id_env_updates([&instance], |row| vec![format!("tmux-{}", row.id)]);
            assert_eq!(
                set_batch.iter().any(|(_, key, value)| {
                    key == crate::tmux::env::AOE_INSTANCE_ID_KEY && value == &instance.id
                }),
                publishes,
                "{tool}: ownership publication"
            );
            assert_eq!(
                set_batch
                    .iter()
                    .find(|(_, key, _)| key == crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY)
                    .map(|(_, _, value)| value.as_str()),
                publishes.then_some(stale),
                "{tool}: publish"
            );
            assert_eq!(
                unset_batch
                    .iter()
                    .any(|(_, key)| key == crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY),
                !publishes,
                "{tool}: unset"
            );
        }
    }

    #[test]
    fn stale_ownership_plan_clears_unsupported_and_orphaned_env() {
        let mut supported = Instance::new("supported", "/tmp/x");
        supported.tool = "claude".to_string();
        supported.agent_session_id = Some("supported-sid".to_string());
        let mut unsupported = Instance::new("unsupported", "/tmp/x");
        unsupported.tool = "qwen".to_string();
        unsupported.agent_session_id = Some("retained-sid".to_string());

        let targets = stale_tmux_ownership_targets(
            &[supported.clone(), unsupported.clone()],
            [
                (
                    "valid".to_string(),
                    Some(supported.id.clone()),
                    Some("supported-sid".to_string()),
                ),
                (
                    "mismatched".to_string(),
                    Some(supported.id.clone()),
                    Some("stale-sid".to_string()),
                ),
                (
                    "unsupported".to_string(),
                    Some(unsupported.id.clone()),
                    Some("retained-sid".to_string()),
                ),
                (
                    "orphan".to_string(),
                    Some("missing-row".to_string()),
                    Some("orphan-sid".to_string()),
                ),
                (
                    "missing-owner".to_string(),
                    None,
                    Some("orphan-sid".to_string()),
                ),
                ("empty".to_string(), None, None),
            ],
        );
        assert_eq!(
            targets,
            vec!["mismatched", "unsupported", "orphan", "missing-owner"]
        );

        let duplicate = supported.clone();
        let ambiguous = [supported, duplicate];
        let (trusted, _) = partition_unambiguous_instances(&ambiguous);
        assert!(trusted.is_empty());
        let targets = stale_tmux_ownership_targets(
            trusted,
            [(
                "ambiguous".to_string(),
                Some(ambiguous[0].id.clone()),
                Some("supported-sid".to_string()),
            )],
        );
        assert_eq!(targets, vec!["ambiguous"]);
    }

    #[test]
    fn deletion_plan_restores_only_proven_operational_ownership() {
        let mut supported = Instance::new("supported", "/tmp/x");
        supported.id = "deleted".to_string();
        supported.tool = "claude".to_string();
        supported.agent_session_id = Some("durable".to_string());
        let mut unsupported = Instance::new("unsupported", "/tmp/y");
        unsupported.id = "inert".to_string();
        unsupported.tool = "qwen".to_string();
        unsupported.agent_session_id = Some("retained".to_string());
        let mut duplicate_a = supported.clone();
        duplicate_a.id = "duplicate".to_string();
        duplicate_a.agent_session_id = Some("duplicate-a".to_string());
        let mut duplicate_b = duplicate_a.clone();
        duplicate_b.agent_session_id = Some("duplicate-b".to_string());
        let instances = [supported, unsupported, duplicate_a, duplicate_b];
        let instance_ids = HashSet::from(["deleted", "inert", "duplicate"]);
        let observations = [
            (
                "deleted-session".to_string(),
                Some("deleted".to_string()),
                Some("newer-than-disk".to_string()),
            ),
            (
                "inert-session".to_string(),
                Some("inert".to_string()),
                Some("retained".to_string()),
            ),
            (
                "duplicate-session".to_string(),
                Some("duplicate".to_string()),
                Some("duplicate-a".to_string()),
            ),
        ];
        let targets = owned_tmux_ownership_targets(&instance_ids, observations.iter().cloned());
        assert_eq!(
            targets,
            vec!["deleted-session", "inert-session", "duplicate-session"]
        );
        assert_eq!(
            cleared_tmux_ownership(&instances, &targets, &observations).entries,
            vec![("deleted-session".to_string(), "newer-than-disk".to_string())]
        );
    }
    #[test]
    #[serial]
    fn global_reconcile_publishes_supported_and_clears_unsupported() {
        if !crate::tmux::is_tmux_available() {
            eprintln!("Skipping: tmux not available");
            return;
        }

        struct Cleanup(Vec<String>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for name in &self.0 {
                    let _ = crate::tmux::tmux_command()
                        .args(["kill-session", "-t", name])
                        .output();
                }
            }
        }

        let temp = tempdir().unwrap();
        let _home = storage_home_guard(&temp);
        let profile = "ownership-reconcile";
        let sid = "019342ab-1234-7def-8901-dddddddddddd";
        let mut supported = Instance::new("supported", "/tmp/x");
        supported.tool = "claude".to_string();
        supported.agent_session_id = Some(sid.to_string());
        let mut unsupported = Instance::new("unsupported", "/tmp/y");
        unsupported.tool = "qwen".to_string();
        unsupported.agent_session_id = Some("retained".to_string());
        let mut unsupported_duplicate = unsupported.clone();
        unsupported_duplicate.title = "unsupported duplicate".to_string();
        let mut ambiguous = Instance::new("ambiguous", "/tmp/z");
        ambiguous.tool = "claude".to_string();
        ambiguous.agent_session_id = Some("019342ab-1234-7def-8901-eeeeeeeeeeee".to_string());
        let mut ambiguous_duplicate = ambiguous.clone();
        ambiguous_duplicate.title = "ambiguous duplicate".to_string();
        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|instances, _groups| {
                *instances = vec![
                    supported.clone(),
                    unsupported.clone(),
                    unsupported_duplicate.clone(),
                    ambiguous.clone(),
                    ambiguous_duplicate.clone(),
                ];
                Ok(())
            })
            .unwrap();

        let supported_name = crate::tmux::Session::generate_name(&supported.id, &supported.title);
        let unsupported_name =
            crate::tmux::Session::generate_name(&unsupported.id, &unsupported.title);
        let ambiguous_name = crate::tmux::Session::generate_name(&ambiguous.id, &ambiguous.title);
        for name in [&supported_name, &unsupported_name, &ambiguous_name] {
            let output = crate::tmux::tmux_command()
                .args(["new-session", "-d", "-s", name])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let _cleanup = Cleanup(vec![
            supported_name.clone(),
            unsupported_name.clone(),
            ambiguous_name.clone(),
        ]);
        crate::tmux::env::set_hidden_env(
            &unsupported_name,
            crate::tmux::env::AOE_INSTANCE_ID_KEY,
            &unsupported.id,
        )
        .unwrap();
        crate::tmux::env::set_hidden_env(
            &unsupported_name,
            crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            "retained",
        )
        .unwrap();

        crate::tmux::env::set_hidden_env(
            &ambiguous_name,
            crate::tmux::env::AOE_INSTANCE_ID_KEY,
            &ambiguous.id,
        )
        .unwrap();
        crate::tmux::env::set_hidden_env(
            &ambiguous_name,
            crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            ambiguous.agent_session_id.as_deref().unwrap(),
        )
        .unwrap();

        reconcile_all_profiles_tmux_session_id_ownership_env().unwrap();
        assert_eq!(
            crate::tmux::env::get_hidden_env_strict(
                &supported_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            )
            .unwrap()
            .as_deref(),
            Some(sid)
        );
        assert_eq!(
            crate::tmux::env::get_hidden_env_strict(
                &unsupported_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            )
            .unwrap(),
            None
        );
        assert_eq!(
            crate::tmux::env::get_hidden_env_strict(
                &ambiguous_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            )
            .unwrap(),
            None
        );
        let vanished = format!(
            "{}vanished_{}",
            crate::tmux::SESSION_PREFIX,
            uuid::Uuid::new_v4().simple()
        );
        let observations =
            read_tmux_ownership_observations(&[supported_name.clone(), vanished]).unwrap();
        assert_eq!(observations.len(), 1);
    }
    #[test]
    fn ownership_planner_rejects_ambiguous_ids_and_suffixes() {
        for tool in ["qwen", "kiro"] {
            let mut instance = Instance::new("owner", "/tmp/x");
            instance.tool = tool.to_string();
            let suffix = &instance.id[..8];
            let agent = format!("{}Dead_{suffix}", crate::tmux::SESSION_PREFIX);
            let terminal = format!("{}Live_{suffix}", crate::tmux::TERMINAL_PREFIX);
            let live = crate::tmux::LiveSessionSnapshot::from_parts(
                Some(vec![agent.clone(), terminal.clone()]),
                Some(HashMap::from([
                    (
                        agent.clone(),
                        crate::tmux::PaneMetadata {
                            pane_dead: false,
                            pane_current_command: None,
                            pane_start_command_is_protected: false,
                        },
                    ),
                    (
                        terminal.clone(),
                        crate::tmux::PaneMetadata {
                            pane_dead: false,
                            pane_current_command: None,
                            pane_start_command_is_protected: false,
                        },
                    ),
                ])),
            );
            let observations = vec![
                (agent.clone(), Some(instance.id.clone()), None),
                (terminal.clone(), None, None),
            ];
            assert_eq!(
                tmux_session_id_env_names(&instance, &live, &observations, false),
                Vec::<String>::new(),
                "{tool}: unsupported rows do not claim suffix matches"
            );
            instance.pending_tmux_ownership_session_id = Some("outgoing-sid".to_string());
            assert_eq!(
                tmux_session_id_env_names(&instance, &live, &observations, true),
                vec![agent.clone(), terminal.clone()],
                "{tool}: a pending outgoing handoff retains live pane ownership"
            );
            instance.pending_tmux_ownership_session_id = None;
            instance.agent_session_id = Some("supported-sid".to_string());
            instance.tool = "claude".to_string();
            assert_eq!(
                tmux_session_id_env_names(&instance, &live, &observations, true),
                vec![agent.clone(), terminal.clone()],
                "one ownerless pane joins its exact-stamped peer"
            );
            let orphan = format!("{}Orphan_{suffix}", crate::tmux::CONTAINER_TERMINAL_PREFIX);
            let crowded_live = crate::tmux::LiveSessionSnapshot::from_parts(
                Some(vec![agent.clone(), terminal.clone(), orphan.clone()]),
                None,
            );
            let crowded_observations = vec![
                (agent.clone(), Some(instance.id.clone()), None),
                (terminal, None, None),
                (orphan, None, None),
            ];
            assert_eq!(
                tmux_session_id_env_names(&instance, &crowded_live, &crowded_observations, true,),
                vec![agent],
                "multiple ownerless suffix matches remain unclaimed"
            );
        }

        let mut first = Instance::new("first", "/tmp/x");
        first.tool = "claude".to_string();
        first.id = "12345678-0000-0000-0000-000000000001".to_string();
        first.agent_session_id = Some("first-sid".to_string());
        let mut second = first.clone();
        second.id = "12345678-0000-0000-0000-000000000002".to_string();
        second.agent_session_id = Some("second-sid".to_string());
        let first_name = format!("{}First_12345678", crate::tmux::SESSION_PREFIX);
        let second_name = format!("{}Second_12345678", crate::tmux::SESSION_PREFIX);
        let live = crate::tmux::LiveSessionSnapshot::from_parts(
            Some(vec![first_name.clone(), second_name.clone()]),
            Some(HashMap::from([
                (
                    first_name.clone(),
                    crate::tmux::PaneMetadata {
                        pane_dead: false,
                        pane_current_command: None,
                        pane_start_command_is_protected: false,
                    },
                ),
                (
                    second_name.clone(),
                    crate::tmux::PaneMetadata {
                        pane_dead: false,
                        pane_current_command: None,
                        pane_start_command_is_protected: false,
                    },
                ),
            ])),
        );
        let observations = vec![
            (first_name.clone(), Some(first.id.clone()), None),
            (second_name.clone(), Some(second.id.clone()), None),
        ];
        assert_eq!(
            tmux_session_id_env_names(&first, &live, &observations, false),
            vec![first_name.clone()]
        );
        assert_eq!(
            tmux_session_id_env_names(&second, &live, &observations, false),
            vec![second_name]
        );

        let ownerless_live = crate::tmux::LiveSessionSnapshot::from_parts(
            Some(vec![first_name.clone()]),
            Some(HashMap::from([(
                first_name.clone(),
                crate::tmux::PaneMetadata {
                    pane_dead: false,
                    pane_current_command: None,
                    pane_start_command_is_protected: false,
                },
            )])),
        );
        let ownerless = vec![(first_name, None, None)];
        assert!(tmux_session_id_env_names(&first, &ownerless_live, &ownerless, false).is_empty());

        let mut shared_conversation = second.clone();
        shared_conversation.id = "87654321-0000-0000-0000-000000000003".to_string();
        shared_conversation.agent_session_id = first.agent_session_id.clone();
        let shared_rows = [first.clone(), shared_conversation];
        let (trusted, ambiguous) = partition_unambiguous_instances(&shared_rows);
        assert!(trusted.is_empty());
        assert!(ambiguous.contains(first.id.as_str()));
        assert!(ambiguous.contains("87654321-0000-0000-0000-000000000003"));
        assert!(tmux_session_id_env_names(&second, &ownerless_live, &ownerless, false).is_empty());
        let colliding_rows = [first.clone(), second];
        assert!(ambiguous_tmux_id_suffixes(&colliding_rows).contains("12345678"));

        let duplicate = first.clone();
        let rows = [first, duplicate];
        let (trusted, ambiguous) = partition_unambiguous_instances(&rows);
        assert!(trusted.is_empty());
        assert!(ambiguous.contains("12345678-0000-0000-0000-000000000001"));
    }

    fn seed_instance_on_disk(profile: &str, inst: &Instance) {
        let storage = Storage::new_unwatched(profile).unwrap();
        let on_disk = inst.clone();
        storage
            .update(|i, g| {
                *i = vec![on_disk.clone()];
                *g = GroupTree::new_with_groups(std::slice::from_ref(&on_disk), &[])
                    .get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    fn seed_instances_on_disk(profile: &str, insts: &[&Instance]) {
        let storage = Storage::new_unwatched(profile).unwrap();
        let owned: Vec<Instance> = insts.iter().map(|i| (*i).clone()).collect();
        storage
            .update(|i, g| {
                *i = owned.clone();
                *g = GroupTree::new_with_groups(&owned, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    #[test]
    #[serial]
    fn survivor_lookup_preserves_duplicates_and_rejects_unreadable_profiles() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        let shared = Instance::new("shared", "/tmp/x");
        seed_instance_on_disk("deleted", &shared);
        seed_instance_on_disk("survivor", &shared);

        let survivor_ids = instance_ids_excluding_profile("deleted").unwrap();
        assert!(survivor_ids.contains(&shared.id));

        #[cfg(unix)]
        {
            let alias_only = Instance::new("alias-only", "/tmp/alias");
            seed_instances_on_disk("deleted", &[&shared, &alias_only]);
            let profiles = crate::session::get_app_dir().unwrap().join("profiles");
            std::os::unix::fs::symlink("deleted", profiles.join("deleted-alias")).unwrap();
            let alias_survivors = instance_ids_excluding_profile("deleted-alias").unwrap();
            assert!(alias_survivors.contains(&shared.id));
            assert!(!alias_survivors.contains(&alias_only.id));
        }

        let corrupt = Instance::new("corrupt", "/tmp/x");
        seed_instance_on_disk("corrupt", &corrupt);
        let corrupt_path = crate::session::get_app_dir()
            .unwrap()
            .join("profiles/corrupt/sessions.json");
        std::fs::write(corrupt_path, b"[null]").unwrap();
        assert!(instance_ids_excluding_profile("deleted").is_err());
    }

    #[test]
    #[serial]
    fn legacy_env_cleanup_loads_unsupported_rows_from_every_profile() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        for (profile, tool) in [("cleanup-alpha", "qwen"), ("cleanup-beta", "kiro")] {
            let mut instance = Instance::new(profile, "/tmp/x");
            instance.tool = tool.to_string();
            instance.source_profile = profile.to_string();
            instance.agent_session_id = Some(format!("{tool}-retained"));
            seed_instance_on_disk(profile, &instance);
        }

        let mut loaded = load_profile_instances_excluding(None)
            .unwrap()
            .into_iter()
            .map(|instance| (instance.title, instance.tool))
            .collect::<Vec<_>>();
        loaded.sort();
        assert_eq!(
            loaded,
            vec![
                ("cleanup-alpha".to_string(), "qwen".to_string()),
                ("cleanup-beta".to_string(), "kiro".to_string()),
            ]
        );
    }

    fn attach_poller_with_update(inst: &mut Instance, sid: &str) {
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_update(&inst.id, sid);
        inst.session_id_poller = Some(Arc::new(Mutex::new(poller)));
    }

    fn attach_poller_with_omp_update(inst: &mut Instance, sid: &str, generation: &str) {
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_omp_update(&inst.id, sid, generation);
        inst.session_id_poller = Some(Arc::new(Mutex::new(poller)));
    }

    fn attach_poller_with_legacy_omp_update(inst: &mut Instance, sid: &str) {
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_omp_legacy_update(&inst.id, sid);
        inst.session_id_poller = Some(Arc::new(Mutex::new(poller)));
    }

    #[test]
    #[serial]
    fn stale_omp_generation_cannot_persist_after_restart() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        let profile = "sync-omp-generation";
        let stale_generation = "launch-a";
        let current_generation = "launch-b";
        let stale_sid = "019342ab-1234-7def-8901-abcdef012345";

        let mut inst = Instance::new("omp-generation-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.tool = "omp".to_string();
        inst.omp_capture_generation = Some(stale_generation.to_string());
        seed_instance_on_disk(profile, &inst);
        Storage::new_unwatched(profile)
            .unwrap()
            .update(|instances, _groups| {
                instances[0].omp_capture_generation = Some(current_generation.to_string());
                Ok(())
            })
            .unwrap();
        attach_poller_with_omp_update(&mut inst, stale_sid, stale_generation);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert!(outcome.applied.is_empty());
        assert_eq!(outcome.rolled_back, vec![instances[0].id.clone()]);
        assert_eq!(instances[0].agent_session_id, None);
        assert_eq!(
            instances[0].omp_capture_generation.as_deref(),
            Some(current_generation)
        );
        let disk = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(disk[0].agent_session_id, None);
        let legacy_sid = "019342ab-1234-7def-8901-abcdef012346";
        attach_poller_with_legacy_omp_update(&mut instances[0], legacy_sid);
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);
        assert_eq!(outcome.rolled_back, vec![instances[0].id.clone()]);
        assert_eq!(instances[0].agent_session_id, None);
        let disk = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(disk[0].agent_session_id, None);
    }

    #[test]
    #[serial]
    fn disk_generation_accepts_typed_observation_when_memory_is_stale() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        let profile = "sync-omp-disk-authority";
        let current_generation = "launch-current";
        let sid = "019342ab-1234-7def-8901-abcdef012347";

        let mut inst = Instance::new("omp-disk-authority-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.tool = "omp".to_string();
        inst.omp_capture_generation = Some("launch-stale-memory".to_string());
        seed_instance_on_disk(profile, &inst);
        Storage::new_unwatched(profile)
            .unwrap()
            .update(|instances, _groups| {
                instances[0].omp_capture_generation = Some(current_generation.to_string());
                Ok(())
            })
            .unwrap();
        attach_poller_with_omp_update(&mut inst, sid, current_generation);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.applied, vec![instances[0].id.clone()]);
        assert_eq!(instances[0].agent_session_id.as_deref(), Some(sid));
        let disk = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(disk[0].agent_session_id.as_deref(), Some(sid));
    }

    #[test]
    #[serial]
    fn drain_applied_updates_memory_and_clears_failed_sid() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let profile = "sync-applied";
        let mut inst = Instance::new("sync-applied-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.agent_session_id = None;
        inst.resume_probe_failed_sid = Some("old-failed".to_string());
        seed_instance_on_disk(profile, &inst);

        let fresh = "019342ab-1234-7def-8901-abcdef012345";
        attach_poller_with_update(&mut inst, fresh);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.applied, vec![instances[0].id.clone()]);
        assert!(outcome.rolled_back.is_empty());
        assert!(outcome.filtered.is_empty());
        assert_eq!(instances[0].agent_session_id.as_deref(), Some(fresh));
        assert_eq!(instances[0].resume_probe_failed_sid, None);

        let storage = Storage::new_unwatched(profile).unwrap();
        let loaded = storage.load().unwrap();
        assert_eq!(loaded[0].agent_session_id.as_deref(), Some(fresh));
        assert_eq!(loaded[0].resume_probe_failed_sid, None);
    }

    #[test]
    #[serial]
    fn drain_filters_invalid_sid_and_leaves_state_unchanged() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let profile = "sync-filtered-validation";
        let mut inst = Instance::new("sync-validation-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.agent_session_id = Some("original-sid".to_string());
        seed_instance_on_disk(profile, &inst);

        attach_poller_with_update(&mut inst, "bad sid!");

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.filtered, vec![instances[0].id.clone()]);
        assert!(outcome.applied.is_empty());
        assert!(outcome.rolled_back.is_empty());
        assert_eq!(
            instances[0].agent_session_id.as_deref(),
            Some("original-sid")
        );
    }

    #[test]
    #[serial]
    fn drain_filters_sid_present_in_retroactive_capture_excludes() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let profile = "sync-filtered-excludes";
        let excluded = "019342ab-1234-7def-8901-abcdef012345";

        let mut inst = Instance::new("sync-excludes-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.agent_session_id = Some("original-sid".to_string());
        inst.retroactive_capture_excludes
            .insert(excluded.to_string());
        seed_instance_on_disk(profile, &inst);

        attach_poller_with_update(&mut inst, excluded);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.filtered, vec![instances[0].id.clone()]);
        assert!(outcome.applied.is_empty());
        assert!(outcome.rolled_back.is_empty());
        assert_eq!(
            instances[0].agent_session_id.as_deref(),
            Some("original-sid")
        );
    }

    #[test]
    #[serial]
    fn drain_rejects_observed_sid_for_stopped_session() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let own = "019342ab-1234-7def-8901-aaaaaaaaaaaa";
        let peer = "019342ab-1234-7def-8901-bbbbbbbbbbbb";
        let mut inst = Instance::new("stopped-title", "/tmp/x");
        inst.source_profile = "sync-stopped".to_string();
        inst.agent_session_id = Some(own.to_string());
        inst.status = Status::Stopped;
        seed_instances_on_disk("sync-stopped", &[&inst]);

        attach_poller_with_update(&mut inst, peer);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.filtered, vec![instances[0].id.clone()]);
        assert!(outcome.applied.is_empty());
        assert_eq!(instances[0].agent_session_id.as_deref(), Some(own));
    }

    #[test]
    #[serial]
    fn drain_defers_capture_while_trash_owns_lifecycle() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        let profile = "sync-trash-owned";
        let sid = "019342ab-1234-7def-8901-bbbbbbbbbbbb";
        let mut instance = Instance::new("trash-owned-title", "/tmp/x");
        instance.source_profile = profile.to_string();
        instance.status = Status::Running;
        instance.lifecycle_generation = 1;
        instance.lifecycle_reservation = Some(crate::session::LifecycleReservation {
            op: crate::session::LifecycleOperation::Trash,
            generation: 1,
            at: chrono::Utc::now(),
        });
        seed_instance_on_disk(profile, &instance);
        attach_poller_with_update(&mut instance, sid);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![instance];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.filtered, vec![instances[0].id.clone()]);
        assert!(outcome.applied.is_empty());
        let stored = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(stored[0].agent_session_id, None);
        assert_eq!(
            stored[0]
                .lifecycle_reservation
                .as_ref()
                .map(|reservation| reservation.op),
            Some(crate::session::LifecycleOperation::Trash)
        );
        assert_eq!(stored[0].lifecycle_generation, 1);
    }

    #[test]
    #[serial]
    fn drain_rejects_observed_sid_contradicting_use_pin() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let pin = "019342ab-1234-7def-8901-aaaaaaaaaaaa";
        let peer = "019342ab-1234-7def-8901-bbbbbbbbbbbb";
        let mut inst = Instance::new("pinned-title", "/tmp/x");
        inst.source_profile = "sync-pinned".to_string();
        inst.agent_session_id = Some(pin.to_string());
        inst.resume_intent = ResumeIntent::Use(pin.to_string());
        // Idle (Instance::new default), so the stopped guard does not fire and
        // the pin guard is what rejects the peer id.
        seed_instances_on_disk("sync-pinned", &[&inst]);

        attach_poller_with_update(&mut inst, peer);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.filtered, vec![instances[0].id.clone()]);
        assert!(outcome.applied.is_empty());
        assert_eq!(instances[0].agent_session_id.as_deref(), Some(pin));
    }

    #[test]
    #[serial]
    fn drain_respects_only_operational_sid_owners() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        for (owner_tool, owned, blocks_claim) in [
            ("claude", "019342ab-1234-7def-8901-cccccccccccc", true),
            ("qwen", "019342ab-1234-7def-8901-dddddddddddd", false),
            ("kiro", "019342ab-1234-7def-8901-eeeeeeeeeeee", false),
        ] {
            let profile = format!("sync-collision-{owner_tool}");
            let mut owner = Instance::new("owner-title", "/tmp/x");
            owner.tool = owner_tool.to_string();
            owner.source_profile = profile.clone();
            owner.agent_session_id = Some(owned.to_string());
            let owner_id = owner.id.clone();

            let mut claimant = Instance::new("claimant-title", "/tmp/x");
            claimant.source_profile = profile.clone();
            seed_instances_on_disk(&profile, &[&owner, &claimant]);
            attach_poller_with_update(&mut claimant, owned);

            let file_watch = FileWatchService::noop();
            let mut instances = vec![owner, claimant];
            let claimant_id = instances[1].id.clone();
            let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

            assert_eq!(
                outcome.filtered,
                blocks_claim
                    .then(|| claimant_id.clone())
                    .into_iter()
                    .collect::<Vec<_>>(),
                "{owner_tool}"
            );
            assert_eq!(
                outcome.applied,
                (!blocks_claim)
                    .then_some(claimant_id)
                    .into_iter()
                    .collect::<Vec<_>>(),
                "{owner_tool}"
            );
            assert_eq!(instances[0].agent_session_id.as_deref(), Some(owned));
            assert_eq!(
                instances[1].agent_session_id.as_deref(),
                (!blocks_claim).then_some(owned),
                "{owner_tool}"
            );

            let stored = Storage::new_unwatched(&profile).unwrap().load().unwrap();
            let disk_claimant = stored
                .iter()
                .find(|instance| instance.title == "claimant-title")
                .unwrap();
            assert_eq!(
                disk_claimant.agent_session_id.as_deref(),
                (!blocks_claim).then_some(owned),
                "{owner_tool}: disk"
            );

            let replacement = "019342ab-1234-7def-8901-bbbbbbbbbbbb";
            let write = persist_session_to_storage(
                &profile,
                &owner_id,
                replacement,
                Some(owned),
                &file_watch,
            );
            assert_eq!(
                write,
                if blocks_claim {
                    SidWrite::Applied
                } else {
                    SidWrite::Skipped
                },
                "{owner_tool}: persistence target"
            );
            let stored = Storage::new_unwatched(&profile).unwrap().load().unwrap();
            let disk_owner = stored
                .iter()
                .find(|instance| instance.id == owner_id)
                .unwrap();
            assert_eq!(
                disk_owner.agent_session_id.as_deref(),
                Some(if blocks_claim { replacement } else { owned }),
                "{owner_tool}: persistence target disk"
            );
        }
    }

    #[test]
    #[serial]
    fn drain_rejects_sid_owned_on_disk_by_unseen_peer() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        // Cross-process shape (#2858): another aoe process (e.g. the serve
        // daemon, while this process is the TUI) has already assigned
        // `contested` to a peer ON DISK, but this process's in-memory slice
        // predates that write, so every in-memory guard waves the claim
        // through. The flock-scoped ownership check inside
        // `persist_session_to_storage` must reject the write and the drain
        // must roll the claimant back to its disk value.
        let contested = "019342ab-1234-7def-8901-eeeeeeeeeeee";
        let profile = "sync-diskowner";

        let mut owner = Instance::new("disk-owner-title", "/tmp/x");
        owner.source_profile = profile.to_string();
        owner.agent_session_id = Some(contested.to_string());

        let mut claimant = Instance::new("claimant-title", "/tmp/x");
        claimant.source_profile = profile.to_string();
        claimant.agent_session_id = None;
        seed_instances_on_disk(profile, &[&owner, &claimant]);

        attach_poller_with_update(&mut claimant, contested);

        let file_watch = FileWatchService::noop();
        // The slice deliberately omits `owner`: its assignment exists only on
        // disk, as after a concurrent process's drain.
        let mut instances = vec![claimant];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.rolled_back, vec![instances[0].id.clone()]);
        assert!(outcome.applied.is_empty());
        assert_eq!(instances[0].agent_session_id, None);

        let storage = Storage::new_unwatched(profile).unwrap();
        let loaded = storage.load().unwrap();
        let disk_owner = loaded
            .iter()
            .find(|i| i.title == "disk-owner-title")
            .unwrap();
        assert_eq!(disk_owner.agent_session_id.as_deref(), Some(contested));
        let disk_claimant = loaded.iter().find(|i| i.title == "claimant-title").unwrap();
        assert_eq!(
            disk_claimant.agent_session_id, None,
            "claimant must not adopt a sid a disk peer already owns"
        );
    }

    #[test]
    #[serial]
    fn drain_rejects_all_claimants_of_same_batch_duplicate_sid() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let contested = "019342ab-1234-7def-8901-dddddddddddd";
        let mut a = Instance::new("peer-a-title", "/tmp/x");
        a.source_profile = "sync-samebatch".to_string();
        a.agent_session_id = None;
        attach_poller_with_update(&mut a, contested);

        let mut b = Instance::new("peer-b-title", "/tmp/x");
        b.source_profile = "sync-samebatch".to_string();
        b.agent_session_id = None;
        seed_instances_on_disk("sync-samebatch", &[&a, &b]);
        attach_poller_with_update(&mut b, contested);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![a, b];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert!(outcome.applied.is_empty());
        assert!(outcome.filtered.contains(&instances[0].id));
        assert!(outcome.filtered.contains(&instances[1].id));
        assert_eq!(instances[0].agent_session_id, None);
        assert_eq!(instances[1].agent_session_id, None);
    }

    #[test]
    #[serial]
    fn cli_capture_persists_poller_observation_to_disk() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let profile = "sync-cli-capture";
        let mut inst = Instance::new("cli-capture-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.agent_session_id = None;
        seed_instance_on_disk(profile, &inst);

        let fresh = "019342ab-1234-7def-8901-abcdef012345";
        attach_poller_with_update(&mut inst, fresh);

        let file_watch = FileWatchService::noop();
        capture_launched_session_id_blocking(&mut inst, &file_watch, Duration::from_secs(2), false);

        assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
        let loaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(loaded[0].agent_session_id.as_deref(), Some(fresh));
    }

    #[test]
    #[serial]
    fn cli_capture_drains_a_queued_correction_before_returning() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let profile = "sync-cli-noop";
        let mut inst = Instance::new("cli-capture-noop-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.agent_session_id = Some("already-here".to_string());
        seed_instance_on_disk(profile, &inst);
        let corrected = "019342ab-1234-7def-8901-cccccccccccc";
        attach_poller_with_update(&mut inst, corrected);

        let file_watch = FileWatchService::noop();
        let start = Instant::now();
        capture_launched_session_id_blocking(
            &mut inst,
            &file_watch,
            Duration::from_secs(30),
            false,
        );

        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(inst.agent_session_id.as_deref(), Some(corrected));
        let loaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(loaded[0].agent_session_id.as_deref(), Some(corrected));
    }

    #[test]
    #[serial]
    fn cli_capture_returns_immediately_without_a_poller() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let mut inst = Instance::new("cli-capture-nopoller-title", "/tmp/x");
        inst.source_profile = "sync-cli-nopoller".to_string();
        inst.agent_session_id = None;

        let file_watch = FileWatchService::noop();
        let start = Instant::now();
        capture_launched_session_id_blocking(
            &mut inst,
            &file_watch,
            Duration::from_secs(30),
            false,
        );

        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(inst.agent_session_id, None);
    }

    #[test]
    #[serial]
    fn cli_capture_waits_for_a_late_poller_observation() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let profile = "sync-cli-late";
        let mut inst = Instance::new("cli-capture-late-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.agent_session_id = None;
        seed_instance_on_disk(profile, &inst);

        let fresh = "019342ab-1234-7def-8901-abcdef999999";
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        let poller = Arc::new(Mutex::new(poller));
        inst.session_id_poller = Some(poller.clone());

        let inst_id = inst.id.clone();
        let injector = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            poller.lock().unwrap().inject_test_update(&inst_id, fresh);
        });

        let file_watch = FileWatchService::noop();
        capture_launched_session_id_blocking(&mut inst, &file_watch, Duration::from_secs(5), false);
        injector.join().unwrap();

        assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
    }

    #[test]
    #[serial]
    fn recurring_drain_persists_newest_and_acknowledges_mailbox() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let profile = "sync-cli-newest";
        let mut inst = Instance::new("cli-capture-newest-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.agent_session_id = None;
        seed_instance_on_disk(profile, &inst);

        let older = "019342ab-1234-7def-8901-aaaaaaaaaaaa";
        let newer = "019342ab-1234-7def-8901-bbbbbbbbbbbb";
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_update(&inst.id, older);
        poller.inject_test_update(&inst.id, newer);
        let poller = Arc::new(Mutex::new(poller));
        inst.session_id_poller = Some(poller.clone());

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.applied, vec![instances[0].id.clone()]);
        assert_eq!(instances[0].agent_session_id.as_deref(), Some(newer));
        assert!(
            poller.lock().unwrap().latest_observation().is_none(),
            "an applied newest observation must acknowledge the sticky mailbox"
        );
    }

    #[test]
    #[serial]
    fn leased_observation_survives_stop_flush_and_stale_ack() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let profile = "sync-sticky-stop-flush";
        let mut inst = Instance::new("sticky-stop-flush", "/tmp/x");
        inst.source_profile = profile.to_string();
        seed_instance_on_disk(profile, &inst);

        let sid = "019342ab-1234-7def-8901-cccccccccccc";
        let newer = "019342ab-1234-7def-8901-dddddddddddd";
        let poller = Arc::new(Mutex::new(SessionPoller::new(format!(
            "test-tmux-{}",
            inst.id
        ))));
        poller.lock().unwrap().inject_test_update(&inst.id, sid);
        inst.session_id_poller = Some(poller.clone());

        let stale_consumer = inst.clone();
        let leased = drain_poller(&stale_consumer).unwrap();
        assert_eq!(leased.sid, sid);

        inst.stop_and_flush_poller();
        let loaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(loaded[0].agent_session_id.as_deref(), Some(sid));

        poller
            .lock()
            .unwrap()
            .inject_test_update(&stale_consumer.id, newer);
        let newer_observation = drain_poller(&stale_consumer).unwrap();
        assert_eq!(newer_observation.sid, newer);
        acknowledge_poller_observation(&stale_consumer, &leased);
        assert_eq!(
            drain_poller(&stale_consumer).unwrap(),
            newer_observation,
            "a late acknowledgement must not erase a newer observation"
        );
    }
}
