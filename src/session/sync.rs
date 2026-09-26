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
use crate::session::{persist_session_to_storage, Instance, ResumeIntent, SidWrite, Status};

/// Per-tick result of [`drain_and_persist_session_ids`]. Lists touched
/// instance IDs grouped by the persistence outcome so a caller holding an
/// auxiliary in-memory mirror (e.g. the TUI's `instances` map) can re-sync
/// each affected entry from the slice.
#[derive(Debug, Default, Clone)]
pub(crate) struct SessionIdSyncOutcome {
    /// Instances whose poller observation durably changed the conversation,
    /// confirmed an OMP pin, or stored a Pi transcript path. Sid updates also
    /// reset `resume_probe_failed_sid`.
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

struct Update {
    id: String,
    sid: String,
    expected_prior: crate::session::instance::ConversationState,
    profile: String,
    observation: SessionIdObservation,
    confirms_omp_pin: bool,
}

struct Rollback {
    id: String,
    conversation: crate::session::instance::ConversationState,
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
    let mut already_current: Vec<(String, SessionIdObservation)> = Vec::new();

    let mut sid_owners: HashMap<
        &str,
        Vec<(&str, Option<crate::session::instance::ConversationKey<'_>>)>,
    > = HashMap::with_capacity(instances.len());
    for inst in instances.iter() {
        if let Some(sid) = inst.agent_session_id.as_deref() {
            sid_owners.entry(sid).or_default().push((
                inst.id.as_str(),
                inst.agent_session_binding
                    .as_ref()
                    .filter(|binding| binding.session_id == sid)
                    .and_then(crate::session::ConversationBinding::key),
            ));
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
        if observation.execution != inst.active_execution {
            acknowledge_poller_observation(inst, &observation);
            filtered_ids.insert(inst.id.clone());
            continue;
        }
        let confirms_omp_pin = observation.confirms_omp_pin(&inst.resume_intent);
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
        // While an explicit set-session-id pin is armed, the poller must not
        // overwrite it with an unowned fresher jsonl that the collision guard
        // below would otherwise wave through (#2708 invariant 1).
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
        // A guarded pin confirmation does not claim a new sid. Its disk CAS
        // verifies that this row already owns it, so stale in-memory ownership
        // and capture exclusions must not mask the launch confirmation.
        if !confirms_omp_pin {
            // Never adopt an id another instance already owns: that is the
            // same-cwd cross-assignment drift itself (#2708 symptom 1).
            if sid_owners.get(sid.as_str()).is_some_and(|owners| {
                owners.iter().any(|(owner, key)| {
                    *owner != inst.id.as_str()
                        && match (*key, observation.conversation_key()) {
                            (Some(peer), Some(captured)) => peer == captured,
                            _ => true,
                        }
                })
            }) {
                acknowledge_poller_observation(inst, &observation);
                filtered_ids.insert(inst.id.clone());
                continue;
            }
            if inst.is_capture_excluded(&sid, observation.source.as_ref()) {
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
        }
        let same_binding = match (
            observation.source.as_ref(),
            inst.agent_session_binding.as_ref(),
        ) {
            (Some(source), Some(binding)) => {
                binding.session_id == sid
                    && binding.execution.as_ref() == Some(source)
                    && binding.transcript_path == observation.transcript_path
                    && binding.provenance == crate::session::ConversationProvenance::Observed
            }
            (None, binding) => binding.is_none_or(|binding| binding.execution.is_none()),
            _ => false,
        };
        if inst.agent_session_id.as_deref() == Some(sid.as_str())
            && same_binding
            && !confirms_omp_pin
        {
            // The pane published the id this row already holds, so there is no sid to write.
            already_current.push((inst.id.clone(), observation));
            continue;
        }
        updates.push(Update {
            id: inst.id.clone(),
            sid,
            expected_prior: inst.conversation_state(),
            profile: inst.source_profile.clone(),
            observation,
            confirms_omp_pin,
        });
    }

    drop(sid_owners);
    let mut claims = HashMap::new();
    let mut raw_claims = HashMap::new();
    let mut unknown_claims = HashSet::new();
    for update in &updates {
        if update.confirms_omp_pin {
            continue;
        }
        let key = update.observation.conversation_key();
        *claims.entry((update.sid.as_str(), key)).or_insert(0usize) += 1;
        *raw_claims.entry(update.sid.as_str()).or_insert(0usize) += 1;
        if key.is_none() {
            unknown_claims.insert(update.sid.as_str());
        }
    }
    let collisions: HashSet<String> = updates
        .iter()
        .filter(|update| {
            !update.confirms_omp_pin
                && (claims
                    .get(&(update.sid.as_str(), update.observation.conversation_key()))
                    .copied()
                    .unwrap_or(0)
                    > 1
                    || (unknown_claims.contains(update.sid.as_str())
                        && raw_claims.get(update.sid.as_str()).copied().unwrap_or(0) > 1))
        })
        .map(|update| update.id.clone())
        .collect();
    drop((claims, raw_claims, unknown_claims));
    updates.retain(|update| {
        if collisions.contains(&update.id) {
            acknowledge_poller_observation_for(instances, &update.id, &update.observation);
            filtered_ids.insert(update.id.clone());
            false
        } else {
            true
        }
    });

    let mut path_only_applied = Vec::new();
    for (id, observation) in &already_current {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == *id) {
            let observed_path = match &observation.guard {
                SessionIdGuard::InstanceSidecar {
                    transcript: Some(path),
                } => Some(path.as_str()),
                _ => None,
            };
            let path_changed =
                observed_path.is_some_and(|path| inst.pi_session_path.as_deref() != Some(path));
            // An unacknowledged path whose write failed is retried on the next drain.
            if inst.persist_observed_pi_transcript(observation) {
                if path_changed && inst.pi_session_path.as_deref() == observed_path {
                    path_only_applied.push(id.clone());
                }
                acknowledge_poller_observation(inst, observation);
            }
        }
    }

    if updates.is_empty() && filtered_ids.is_empty() {
        return SessionIdSyncOutcome {
            applied: path_only_applied,
            ..SessionIdSyncOutcome::default()
        };
    }

    let mut to_apply: Vec<&Update> = Vec::with_capacity(updates.len());
    let mut to_rollback: Vec<Rollback> = Vec::with_capacity(updates.len());

    let mut capture_generations: Vec<(String, u64)> = Vec::with_capacity(updates.len());
    for update in &updates {
        let ownership: anyhow::Result<_> = if lifecycle_already_locked || update.confirms_omp_pin {
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
            Ok(_) => persist_session_to_storage(
                &update.profile,
                &update.id,
                &update.observation,
                &update.expected_prior,
                file_watch,
            ),
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
            // Acknowledged once its transcript path is stored, in the loop below.
            SidWrite::Applied => {
                to_apply.push(update);
            }
            SidWrite::Skipped | SidWrite::PinnedForeign => {
                if let Some(mut rb) =
                    reload_skipped_from_disk(&update.profile, &update.id, file_watch)
                {
                    if !update.confirms_omp_pin
                        && rb.conversation.session_id.as_deref() == Some(update.sid.as_str())
                    {
                        // A matching disk sid may still belong to a different execution.
                        if let Some(inst) = instances.iter_mut().find(|i| i.id == update.id) {
                            if inst.persist_observed_pi_transcript(&update.observation) {
                                if matches!(
                                    &update.observation.guard,
                                    SessionIdGuard::InstanceSidecar {
                                        transcript: Some(_)
                                    }
                                ) {
                                    if let Some(current) = reload_skipped_from_disk(
                                        &update.profile,
                                        &update.id,
                                        file_watch,
                                    ) {
                                        rb = current;
                                    }
                                }
                                acknowledge_poller_observation(inst, &update.observation);
                            }
                        }
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

    for update in &to_apply {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == update.id) {
            inst.apply_conversation_observation(&update.observation);
            let confirms_omp_pin = &update.confirms_omp_pin;
            if *confirms_omp_pin {
                inst.resume_intent = ResumeIntent::Default;
                inst.resume_binding = None;
            } else {
                inst.resume_probe_failed_sid = None;
            }
            // The conversation CAS also stores the path when both fields name the same file.
            let path_committed_with_sid = matches!(
                &update.observation.guard,
                SessionIdGuard::InstanceSidecar {
                    transcript: Some(path)
                } if update.observation.pi_session_path.as_deref() == Some(path.as_str())
            );
            if path_committed_with_sid || inst.persist_observed_pi_transcript(&update.observation) {
                acknowledge_poller_observation(inst, &update.observation);
            }
        }
    }
    for rb in &to_rollback {
        if let Some(inst) = instances.iter_mut().find(|i| i.id == rb.id) {
            inst.adopt_conversation_state(rb.conversation.clone());
            inst.resume_probe_failed_sid = rb.disk_failed_sid.clone();
            inst.omp_capture_generation = rb.disk_omp_capture_generation.clone();
        }
    }

    publish_tmux_env(instances, &to_apply, &to_rollback, &filtered_ids);

    SessionIdSyncOutcome {
        applied: path_only_applied
            .into_iter()
            .chain(to_apply.into_iter().map(|update| update.id.clone()))
            .collect(),
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
    capture_launched_session_id_with_wait(inst, file_watch, timeout, notify, std::thread::sleep);
}

fn capture_launched_session_id_with_wait(
    inst: &mut Instance,
    file_watch: &Arc<FileWatchService>,
    timeout: Duration,
    notify: bool,
    mut wait: impl FnMut(Duration),
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
        wait(CLI_CAPTURE_POLL_INTERVAL);
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

/// Drain newly queued observations into the sticky mailbox, then test the pending one without
/// cloning it. The predicate sees only the newest observation.
pub(crate) fn pending_poller_observation_matches(
    inst: &Instance,
    predicate: impl FnOnce(&SessionIdObservation) -> bool,
) -> bool {
    let Some(arc) = inst.session_id_poller.as_ref() else {
        return false;
    };
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
    guard.pending_observation_matches(predicate)
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
        conversation: disk_inst.conversation_state(),
        disk_failed_sid: disk_inst.resume_probe_failed_sid.clone(),
        disk_omp_capture_generation: disk_inst.omp_capture_generation.clone(),
    })
}

fn publish_tmux_env(
    instances: &[Instance],
    to_apply: &[&Update],
    to_rollback: &[Rollback],
    filtered_ids: &HashSet<String>,
) {
    let touched_count = to_apply.len() + to_rollback.len() + filtered_ids.len();
    let mut set_batch: Vec<(String, String, String)> = Vec::with_capacity(touched_count);
    let mut unset_batch: Vec<(String, String)> = Vec::with_capacity(touched_count);

    let touched_ids = to_apply
        .iter()
        .map(|update| update.id.as_str())
        .chain(to_rollback.iter().map(|r| r.id.as_str()))
        .chain(filtered_ids.iter().map(|s| s.as_str()));

    for id in touched_ids {
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            continue;
        };
        let tmux_name = match inst.tmux_env_session_name() {
            Some(name) => name,
            None => continue,
        };
        // Re-assert the instance-id alongside the captured sid: this publish
        // replaced the poller's on_change pre-CAS publish (which wrote both
        // keys), and `build_exclusion_set` can only attribute a captured sid
        // to its owner when AOE_INSTANCE_ID is present on the same session.
        set_batch.push((
            tmux_name.clone(),
            crate::tmux::env::AOE_INSTANCE_ID_KEY.to_string(),
            inst.id.clone(),
        ));
        match &inst.agent_session_id {
            Some(sid) => set_batch.push((
                tmux_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
                sid.clone(),
            )),
            None => unset_batch.push((
                tmux_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
            )),
        }
    }

    if !set_batch.is_empty() {
        let refs: Vec<(&str, &str, &str)> = set_batch
            .iter()
            .map(|(s, k, v)| (s.as_str(), k.as_str(), v.as_str()))
            .collect();
        if let Err(e) = crate::tmux::env::set_hidden_env_batch(&refs) {
            tracing::warn!(target: "session.sync", "Post-CAS env publish failed: {e}");
        }
    }
    if !unset_batch.is_empty() {
        let refs: Vec<(&str, &str)> = unset_batch
            .iter()
            .map(|(s, k)| (s.as_str(), k.as_str()))
            .collect();
        if let Err(e) = crate::tmux::env::remove_hidden_env_batch(&refs) {
            tracing::warn!(target: "session.sync", "Post-CAS env unset failed: {e}");
        }
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

    // Points `HOME` (and, on Linux/macOS, `XDG_CONFIG_HOME`) at `temp` for the current test body.
    fn storage_home_guard(temp: &TempDir) -> EnvGuard {
        let pairs: Vec<(&'static str, PathBuf)> = vec![
            ("HOME", temp.path().to_path_buf()),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            ("XDG_CONFIG_HOME", temp.path().join(".config")),
        ];
        EnvGuard::set(&pairs)
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

    fn attach_poller_with_update(inst: &mut Instance, sid: &str) {
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_update(&inst.id, sid);
        inst.session_id_poller = Some(Arc::new(Mutex::new(poller)));
    }

    fn attach_poller_with_omp_update(inst: &mut Instance, sid: &str, generation: &str) {
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        let mut observation = crate::session::poller::SessionIdObservation::omp(
            sid.to_owned(),
            generation.to_owned(),
        );
        observation.execution = inst.active_execution.clone();
        observation.source = inst
            .active_execution
            .as_ref()
            .map(|active| active.binding.clone());
        poller.inject_test_observation(&inst.id, observation);
        inst.session_id_poller = Some(Arc::new(Mutex::new(poller)));
    }

    fn attach_poller_with_legacy_omp_update(inst: &mut Instance, sid: &str) {
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_omp_legacy_update(&inst.id, sid);
        inst.session_id_poller = Some(Arc::new(Mutex::new(poller)));
    }

    fn pinned_omp_instance(profile: &str, sid: &str, generation: &str) -> Instance {
        let mut inst = Instance::new("omp-pin-title", "/tmp/x");
        inst.source_profile = profile.to_string();
        inst.tool = "omp".to_string();
        inst.agent_session_id = Some(sid.to_string());
        inst.resume_intent = ResumeIntent::Use(sid.to_string());
        inst.omp_capture_generation = Some(generation.to_string());
        let execution = crate::session::ExecutionBinding {
            agent: "omp".into(),
            stores: vec!["/native-omp-store".into()],
            configuration: Vec::new(),
            exported_default_store: false,
            cwd: "/tmp/x".into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        };
        let binding = crate::session::ConversationBinding {
            session_id: sid.into(),
            execution: Some(execution.clone()),
            provenance: crate::session::ConversationProvenance::Observed,
            transcript_path: None,
        };
        inst.agent_session_binding = Some(binding.clone());
        inst.resume_binding = Some(crate::session::ConversationBinding {
            provenance: crate::session::ConversationProvenance::Asserted,
            ..binding
        });
        inst.active_execution = Some(crate::session::instance::ActiveExecution {
            launch_id: uuid::Uuid::new_v4().to_string(),
            binding: execution,
            capture: None,
            container: None,
        });
        inst
    }

    #[test]
    #[serial]
    fn exact_omp_generation_confirmation_consumes_pin_idempotently() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        let profile = "sync-omp-pin-exact";
        let sid = "019342ab-1234-7def-8901-abcdef012340";
        let generation = "launch-exact";
        let mut inst = pinned_omp_instance(profile, sid, generation);
        seed_instance_on_disk(profile, &inst);
        attach_poller_with_omp_update(&mut inst, sid, generation);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.applied, vec![instances[0].id.clone()]);
        assert_eq!(instances[0].resume_intent, ResumeIntent::Default);
        let disk = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(disk[0].resume_intent, ResumeIntent::Default);
        let acknowledged = drain_and_persist_session_ids(&mut instances, &file_watch);
        assert!(!acknowledged.touched());

        attach_poller_with_omp_update(&mut instances[0], sid, generation);
        let repeated = drain_and_persist_session_ids(&mut instances, &file_watch);
        assert!(!repeated.touched());
        let disk = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(disk[0].resume_intent, ResumeIntent::Default);
    }

    #[test]
    #[serial]
    fn omp_pin_requires_binding_and_current_typed_generation() {
        let sid = "019342ab-1234-7def-8901-abcdef012340";
        for case in ["unbound", "stale", "legacy"] {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "sync-omp-pin-negative";
            let mut inst = pinned_omp_instance(profile, sid, "launch-current");
            if case == "unbound" {
                inst.agent_session_binding = None;
                inst.resume_binding = None;
                inst.active_execution = None;
            }
            let expected = inst.conversation_state();
            seed_instance_on_disk(profile, &inst);
            match case {
                "legacy" => attach_poller_with_legacy_omp_update(&mut inst, sid),
                "stale" => attach_poller_with_omp_update(&mut inst, sid, "launch-stale"),
                _ => attach_poller_with_omp_update(&mut inst, sid, "launch-current"),
            }
            let mut instances = vec![inst];
            let outcome = drain_and_persist_session_ids(&mut instances, &FileWatchService::noop());
            assert!(outcome.applied.is_empty(), "{case}");
            assert_eq!(instances[0].conversation_state(), expected, "{case}");
            assert_eq!(
                Storage::new_unwatched(profile).unwrap().load().unwrap()[0].conversation_state(),
                expected,
                "{case}"
            );
            assert_eq!(
                instances[0].resume_intent,
                ResumeIntent::Use(sid.into()),
                "{case}"
            );
        }
    }

    #[test]
    #[serial]
    fn omp_pin_confirmation_cas_loss_retries_sticky_observation() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        let profile = "sync-omp-pin-retry";
        let sid = "019342ab-1234-7def-8901-abcdef012345";
        let generation = "launch-observed";
        let mut inst = pinned_omp_instance(profile, sid, generation);
        let original = inst.conversation_state();
        seed_instance_on_disk(profile, &inst);
        let storage = Storage::new_unwatched(profile).unwrap();
        let peer_pin = "019342ab-1234-7def-8901-abcdef012348";
        storage
            .update(|instances, _groups| {
                instances[0].omp_capture_generation = Some("peer-launch".to_string());
                instances[0].resume_intent = ResumeIntent::Use(peer_pin.to_string());
                instances[0].resume_binding.as_mut().unwrap().session_id = peer_pin.into();
                Ok(())
            })
            .unwrap();
        attach_poller_with_omp_update(&mut inst, sid, generation);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![inst];
        let lost = drain_and_persist_session_ids(&mut instances, &file_watch);
        assert_eq!(lost.rolled_back, vec![instances[0].id.clone()]);
        assert_eq!(
            instances[0].resume_intent,
            ResumeIntent::Use(peer_pin.to_string())
        );

        storage
            .update(|disk, _groups| {
                disk[0].omp_capture_generation = Some(generation.to_string());
                disk[0].resume_intent = ResumeIntent::Use(sid.to_string());
                disk[0].resume_binding = original.resume_binding.clone();
                Ok(())
            })
            .unwrap();
        instances[0].adopt_conversation_state(original);
        instances[0].omp_capture_generation = Some(generation.into());
        let retried = drain_and_persist_session_ids(&mut instances, &file_watch);
        assert_eq!(retried.applied, vec![instances[0].id.clone()]);
        assert_eq!(instances[0].resume_intent, ResumeIntent::Default);
        let disk = storage.load().unwrap();
        assert_eq!(disk[0].resume_intent, ResumeIntent::Default);
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

    /// Invalid, excluded, stopped-session and pin-contradicting observations (#2709) are
    /// filtered without touching the row.
    #[test]
    #[serial]
    fn drain_filters_rejected_observations_and_leaves_state_unchanged() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        let own = "019342ab-1234-7def-8901-aaaaaaaaaaaa";
        let peer = "019342ab-1234-7def-8901-bbbbbbbbbbbb";
        let excluded = "019342ab-1234-7def-8901-abcdef012345";
        let cases: [(&str, &str, fn(&mut Instance)); 4] = [
            ("validation", "bad sid!", |_| {}),
            ("excludes", excluded, |_| {}),
            ("stopped", peer, |inst| inst.status = Status::Stopped),
            ("use-pin", peer, |inst| {
                inst.resume_intent = ResumeIntent::Use(inst.agent_session_id.clone().unwrap())
            }),
        ];
        for (case, observed, configure) in cases {
            let profile = format!("sync-filtered-{case}");
            let mut inst = Instance::new("sync-filtered-title", "/tmp/x");
            inst.source_profile = profile.clone();
            inst.agent_session_id = Some(own.to_string());
            inst.retroactive_capture_excludes
                .insert(crate::session::ConversationBinding::unknown(
                    excluded.to_string(),
                ));
            configure(&mut inst);
            seed_instance_on_disk(&profile, &inst);
            attach_poller_with_update(&mut inst, observed);

            let file_watch = FileWatchService::noop();
            let mut instances = vec![inst];
            let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

            assert_eq!(outcome.filtered, vec![instances[0].id.clone()], "{case}");
            assert!(outcome.applied.is_empty(), "{case}");
            assert!(outcome.rolled_back.is_empty(), "{case}");
            assert_eq!(
                instances[0].agent_session_id.as_deref(),
                Some(own),
                "{case}"
            );
        }
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
    fn drain_rejects_sid_owned_by_another_instance() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let owned = "019342ab-1234-7def-8901-cccccccccccc";
        let mut owner = Instance::new("owner-title", "/tmp/x");
        owner.source_profile = "sync-collision".to_string();
        owner.agent_session_id = Some(owned.to_string());

        let mut thief = Instance::new("thief-title", "/tmp/x");
        thief.source_profile = "sync-collision".to_string();
        thief.agent_session_id = None;
        seed_instances_on_disk("sync-collision", &[&owner, &thief]);
        attach_poller_with_update(&mut thief, owned);

        let file_watch = FileWatchService::noop();
        let mut instances = vec![owner, thief];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(outcome.filtered, vec![instances[1].id.clone()]);
        assert!(outcome.applied.is_empty());
        assert_eq!(instances[0].agent_session_id.as_deref(), Some(owned));
        assert_eq!(instances[1].agent_session_id, None);
    }

    #[test]
    #[serial]
    fn drain_rejects_sid_owned_on_disk_by_unseen_peer() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

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

    /// A queued observation (fresh or a correction) is persisted without waiting out the
    /// timeout, and a launch without a poller returns at once (#3169).
    #[test]
    #[serial]
    fn cli_capture_drains_queued_observations_without_waiting() {
        let fresh = "019342ab-1234-7def-8901-abcdef012345";
        let corrected = "019342ab-1234-7def-8901-cccccccccccc";
        // (initial sid, poller observation, expected sid)
        for (initial, observed, expected) in [
            (None, Some(fresh), Some(fresh)),
            (Some("already-here"), Some(corrected), Some(corrected)),
            (None, None, None),
        ] {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "sync-cli-capture";
            let mut inst = Instance::new("cli-capture-title", "/tmp/x");
            inst.source_profile = profile.to_string();
            inst.agent_session_id = initial.map(str::to_string);
            seed_instance_on_disk(profile, &inst);
            if let Some(observed) = observed {
                attach_poller_with_update(&mut inst, observed);
            }

            let start = Instant::now();
            capture_launched_session_id_blocking(
                &mut inst,
                &FileWatchService::noop(),
                Duration::from_secs(30),
                false,
            );

            assert!(start.elapsed() < Duration::from_secs(1), "{observed:?}");
            assert_eq!(inst.agent_session_id.as_deref(), expected);
            let loaded = Storage::new_unwatched(profile).unwrap().load().unwrap();
            assert_eq!(
                loaded[0].agent_session_id.as_deref(),
                expected.or(initial),
                "{observed:?}"
            );
        }
    }

    // The sidecar names the pane, so a conversation started inside it with `/new` is this session's
    // and replaces the anchor.
    #[test]
    #[serial]
    fn pi_accepts_a_sidecar_retarget_and_keeps_its_poller() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let profile = "sync-pi-sidecar";
        let mut inst = Instance::new("pi-sidecar-title", "/tmp/pi-sidecar");
        inst.source_profile = profile.to_string();
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some("pi-launch-conversation".to_string());
        inst.mark_pi_extension_launched_for_test();
        seed_instance_on_disk(profile, &inst);

        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_sidecar_update(&inst.id, "pi-new-conversation", None);
        inst.session_id_poller = Some(Arc::new(Mutex::new(poller)));

        let file_watch = FileWatchService::noop();
        let mut instances = [inst];
        drain_and_persist_session_ids(&mut instances, &file_watch);

        assert_eq!(
            instances[0].agent_session_id.as_deref(),
            Some("pi-new-conversation"),
            "an attributable observation must be adopted"
        );
        assert!(
            instances[0].session_id_poller.is_some(),
            "a sidecar poller keeps watching for the next switch"
        );
    }

    // A captured transcript path stays pending until it is stored, whichever branch adopts its id.
    #[test]
    #[serial]
    fn a_captured_pi_transcript_path_survives_a_failed_write_on_every_branch() {
        use crate::session::instance::FAIL_PI_PATH_WRITES;
        let old = "01a05234-8889-72e2-a7c9-7ebc27b25b78";
        let new = "0192f7a1-4b3c-7d2e-9f10-aa1b2c3d4e5f";
        let path =
            format!("/home/u/.pi/agent/sessions/--proj--/2026-01-02T00-00-00-000Z_{new}.jsonl");
        // (label, sid already on disk, first write fails)
        for (label, disk_sid, fail_first) in [
            ("applied new id", old, false),
            ("applied new id, failed path write", old, true),
            ("another writer stored the id", new, false),
            ("another writer stored the id, failed path write", new, true),
        ] {
            let temp = tempdir().unwrap();
            let _guard = storage_home_guard(&temp);
            let profile = "sync-pi-path-delivery";
            let mut inst = Instance::new("pi-path-delivery", "/tmp/pi-path-delivery");
            inst.source_profile = profile.to_string();
            inst.tool = "pi".to_string();
            inst.mark_pi_extension_launched_for_test();
            let mut on_disk = inst.clone();
            on_disk.agent_session_id = Some(disk_sid.to_string());
            seed_instance_on_disk(profile, &on_disk);
            inst.agent_session_id = Some(old.to_string());

            // No sidecar exists: only the observation carries the path.
            let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
            poller.inject_test_sidecar_update(&inst.id, new, Some(&path));
            inst.session_id_poller = Some(Arc::new(Mutex::new(poller)));
            let file_watch = FileWatchService::noop();
            let mut instances = [inst];
            let stored = || Storage::new_unwatched(profile).unwrap().load().unwrap()[0].clone();

            FAIL_PI_PATH_WRITES.with(|fail| fail.set(fail_first));
            drain_and_persist_session_ids(&mut instances, &file_watch);
            FAIL_PI_PATH_WRITES.with(|fail| fail.set(false));
            assert_eq!(stored().agent_session_id.as_deref(), Some(new), "{label}");
            if fail_first {
                assert_eq!(stored().pi_session_path, None, "{label}");
                // Recovery needs no new publication: the held observation is retried.
                drain_and_persist_session_ids(&mut instances, &file_watch);
            }
            assert_eq!(
                stored().pi_session_path.as_deref(),
                Some(path.as_str()),
                "{label}"
            );
            assert_eq!(
                instances[0].pi_session_path.as_deref(),
                Some(path.as_str()),
                "{label}: the live snapshot must retain the durable path"
            );
            let poller = instances[0].session_id_poller.as_ref().unwrap();
            assert!(
                poller.lock().unwrap().latest_observation().is_none(),
                "{label}: a stored path acknowledges its observation"
            );
        }
    }

    #[test]
    #[serial]
    fn stop_and_flush_retries_a_final_pi_transcript_path_write() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        let profile = "sync-pi-stop-flush-retry";
        let sid = "0192f7a1-4b3c-7d2e-9f10-aa1b2c3d4e5f";
        let path = format!("/tmp/2026-01-02T00-00-00-000Z_{sid}.jsonl");
        let mut inst = Instance::new("pi-stop-flush-retry", "/tmp/pi-stop-flush-retry");
        inst.source_profile = profile.to_string();
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some(sid.to_string());
        seed_instance_on_disk(profile, &inst);

        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_sidecar_update(&inst.id, sid, Some(&path));
        let poller = Arc::new(Mutex::new(poller));
        inst.session_id_poller = Some(poller.clone());
        let fail_next = Instance::fail_next_pi_path_write_for_test();

        inst.stop_and_flush_poller();
        assert!(fail_next.was_consumed());

        assert!(inst.session_id_poller.is_none());
        let stored = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(stored[0].agent_session_id.as_deref(), Some(sid));
        assert_eq!(stored[0].pi_session_path.as_deref(), Some(path.as_str()));
        assert!(poller.lock().unwrap().latest_observation().is_none());
    }

    #[test]
    #[serial]
    fn failed_pi_path_write_does_not_reattach_after_execution_change() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        let profile = "sync-pi-stale-after-failed-path";
        let sid = "0192f7a1-4b3c-7d2e-9f10-aa1b2c3d4e5f";
        let path = format!("/tmp/2026-01-02T00-00-00-000Z_{sid}.jsonl");
        let execution = crate::session::instance::ActiveExecution {
            launch_id: uuid::Uuid::new_v4().to_string(),
            binding: crate::session::ExecutionBinding {
                agent: "pi".into(),
                stores: vec!["/tmp/pi-store".into()],
                configuration: Vec::new(),
                exported_default_store: false,
                cwd: "/tmp/pi-stale-after-failed-path".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
            },
            capture: None,
            container: None,
        };
        let mut inst = Instance::new(
            "pi-stale-after-failed-path",
            "/tmp/pi-stale-after-failed-path",
        );
        inst.source_profile = profile.to_string();
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some(sid.to_string());
        inst.active_execution = Some(execution.clone());
        inst.agent_session_binding = Some(crate::session::ConversationBinding {
            session_id: sid.to_string(),
            execution: Some(execution.binding.clone()),
            provenance: crate::session::ConversationProvenance::Observed,
            transcript_path: None,
        });
        seed_instance_on_disk(profile, &inst);

        let mut observation = crate::session::poller::SessionIdObservation::instance_sidecar(
            sid.to_string(),
            Some(path.clone()),
        );
        observation.execution = Some(execution.clone());
        observation.source = Some(execution.binding.clone());
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_observation(&inst.id, observation);
        let poller = Arc::new(Mutex::new(poller));
        inst.session_id_poller = Some(poller.clone());

        let mut next_execution = execution;
        next_execution.launch_id = uuid::Uuid::new_v4().to_string();
        next_execution.binding.stores = vec!["/tmp/peer-pi-store".into()];
        let next_binding = crate::session::ConversationBinding {
            session_id: sid.to_string(),
            execution: Some(next_execution.binding.clone()),
            provenance: crate::session::ConversationProvenance::Observed,
            transcript_path: None,
        };
        let hook_execution = next_execution.clone();
        let hook_binding = next_binding.clone();
        let hook_profile = profile.to_string();
        let fail_next = Instance::fail_next_pi_path_write_for_test();
        let _hook = Instance::set_after_final_pi_drain_hook_for_test(move |inst| {
            assert!(Instance::fail_next_pi_path_write_consumed_for_test());
            assert_eq!(inst.pi_session_path, None);
            assert!(inst.session_id_poller.is_some());
            assert!(crate::session::sync::pending_poller_observation_matches(
                inst,
                |observation| inst.observation_is_current_pi_path(observation),
            ));
            inst.active_execution = Some(hook_execution.clone());
            inst.agent_session_binding = Some(hook_binding.clone());
            let storage = Storage::new_unwatched(&hook_profile).unwrap();
            storage
                .update(|rows, _| {
                    rows[0].active_execution = Some(hook_execution.clone());
                    rows[0].agent_session_binding = Some(hook_binding.clone());
                    Ok(())
                })
                .unwrap();
        });

        inst.stop_and_flush_poller();
        assert!(fail_next.was_consumed());

        assert!(inst.session_id_poller.is_none());
        assert_eq!(inst.pi_session_path, None);
        let stored = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(stored[0].agent_session_id.as_deref(), Some(sid));
        assert_eq!(stored[0].pi_session_path, None);
        assert_eq!(stored[0].active_execution, Some(next_execution));
        assert_eq!(stored[0].agent_session_binding, Some(next_binding));
        assert!(poller.lock().unwrap().latest_observation().is_some());
    }

    #[test]
    #[serial]
    fn pi_path_in_conversation_cas_needs_no_second_write_to_acknowledge() {
        use crate::session::instance::FAIL_PI_PATH_WRITES;
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        let profile = "sync-pi-path-in-cas";
        let sid = "0192f7a1-4b3c-7d2e-9f10-aa1b2c3d4e5f";
        let path = format!("/tmp/pi_{sid}.jsonl");
        let mut inst = Instance::new("pi-path-in-cas", "/tmp/pi-path-in-cas");
        inst.source_profile = profile.to_string();
        inst.tool = "pi".to_string();
        seed_instance_on_disk(profile, &inst);
        let mut observation = crate::session::poller::SessionIdObservation::instance_sidecar(
            sid.into(),
            Some(path.clone()),
        );
        observation.pi_session_path = Some(path.clone());
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_observation(&inst.id, observation);
        inst.session_id_poller = Some(Arc::new(Mutex::new(poller)));

        FAIL_PI_PATH_WRITES.with(|fail| fail.set(true));
        let mut instances = [inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &FileWatchService::noop());
        FAIL_PI_PATH_WRITES.with(|fail| fail.set(false));
        assert_eq!(outcome.applied, vec![instances[0].id.clone()]);
        let stored = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(stored[0].pi_session_path.as_deref(), Some(path.as_str()));
        assert!(instances[0]
            .session_id_poller
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .latest_observation()
            .is_none());
    }

    #[test]
    #[serial]
    fn pi_transcript_does_not_follow_a_stale_same_sid_execution() {
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);
        let profile = "sync-pi-stale-path";
        let old = "01a05234-8889-72e2-a7c9-7ebc27b25b78";
        let sid = "0192f7a1-4b3c-7d2e-9f10-aa1b2c3d4e5f";
        let stale_path = format!("/tmp/old_{sid}.jsonl");
        let peer_path = format!("/tmp/peer_{sid}.jsonl");
        let mut inst = Instance::new("pi-stale-path", "/tmp/pi-stale-path");
        inst.source_profile = profile.to_string();
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some(old.into());
        let execution = crate::session::instance::ActiveExecution {
            launch_id: uuid::Uuid::new_v4().to_string(),
            binding: crate::session::ExecutionBinding {
                agent: "pi".into(),
                stores: vec!["/tmp/pi-store".into()],
                configuration: Vec::new(),
                exported_default_store: false,
                cwd: "/tmp/pi-stale-path".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
            },
            capture: None,
            container: None,
        };
        inst.active_execution = Some(execution.clone());
        seed_instance_on_disk(profile, &inst);
        let mut peer_execution = execution.clone();
        peer_execution.launch_id = uuid::Uuid::new_v4().to_string();
        peer_execution.binding.stores = vec!["/tmp/peer-pi-store".into()];
        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                rows[0].agent_session_id = Some(sid.into());
                rows[0].active_execution = Some(peer_execution.clone());
                rows[0].agent_session_binding = Some(crate::session::ConversationBinding {
                    session_id: sid.into(),
                    execution: Some(peer_execution.binding.clone()),
                    provenance: crate::session::ConversationProvenance::Observed,
                    transcript_path: None,
                });
                rows[0].pi_session_path = Some(peer_path.clone());
                Ok(())
            })
            .unwrap();
        let mut observation = crate::session::poller::SessionIdObservation::instance_sidecar(
            sid.into(),
            Some(stale_path),
        );
        observation.execution = Some(execution.clone());
        observation.source = Some(execution.binding);
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_observation(&inst.id, observation);
        let old_poller = Arc::new(Mutex::new(poller));
        inst.session_id_poller = Some(old_poller.clone());

        let mut instances = [inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &FileWatchService::noop());
        assert_eq!(outcome.rolled_back, vec![instances[0].id.clone()]);
        assert_eq!(
            instances[0].active_execution.as_ref(),
            Some(&peer_execution)
        );
        assert!(instances[0].session_id_poller.is_none());
        let stored = storage.load().unwrap();
        assert_eq!(stored[0].active_execution, Some(peer_execution));
        assert_eq!(
            stored[0].pi_session_path.as_deref(),
            Some(peer_path.as_str())
        );
        assert_eq!(
            instances[0].pi_session_path.as_deref(),
            Some(peer_path.as_str())
        );
        assert!(old_poller.lock().unwrap().latest_observation().is_none());
    }

    #[test]
    #[serial]
    fn pi_records_its_transcript_path_when_the_observation_repeats_its_own_id() {
        let (_hooks, _base, _hooks_tmp) = crate::hooks::test_support::BaseGuard::ready();
        let temp = tempdir().unwrap();
        let _guard = storage_home_guard(&temp);

        let profile = "sync-pi-same-sid";
        let sid = "01a05234-8889-72e2-a7c9-7ebc27b25b78";
        let mut inst = Instance::new("pi-same-sid-title", "/tmp/pi-same-sid");
        inst.source_profile = profile.to_string();
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some(sid.to_string());
        inst.mark_pi_extension_launched_for_test();
        seed_instance_on_disk(profile, &inst);

        let published = "/home/u/.pi/agent/sessions/--proj--/2026-01-01T00-00-00-000Z_01a05234-8889-72e2-a7c9-7ebc27b25b78.jsonl";
        // The sidecar is already gone: only the observation carries the path.
        let poller = SessionPoller::new(format!("test-tmux-{}", inst.id));
        poller.inject_test_sidecar_update(&inst.id, sid, Some(published));
        inst.session_id_poller = Some(Arc::new(Mutex::new(poller)));

        let file_watch = FileWatchService::noop();
        let mut instances = [inst];
        let outcome = drain_and_persist_session_ids(&mut instances, &file_watch);
        assert_eq!(outcome.applied, vec![instances[0].id.clone()]);
        assert_eq!(instances[0].pi_session_path.as_deref(), Some(published));

        assert_eq!(
            instances[0].agent_session_id.as_deref(),
            Some(sid),
            "an observation that repeats the row's own id changes no sid"
        );
        let stored = Storage::new_unwatched(profile).unwrap().load().unwrap();
        assert_eq!(
            stored[0].pi_session_path.as_deref(),
            Some(published),
            "the transcript path must be durable before any teardown runs"
        );
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
        let mut waited = false;
        let file_watch = FileWatchService::noop();
        capture_launched_session_id_with_wait(
            &mut inst,
            &file_watch,
            Duration::from_secs(5),
            false,
            |_| {
                assert!(
                    !waited,
                    "the late observation should satisfy the next drain"
                );
                waited = true;
                poller.lock().unwrap().inject_test_update(&inst_id, fresh);
            },
        );
        assert!(
            waited,
            "capture must reach its empty-mailbox wait before publication"
        );

        assert_eq!(inst.agent_session_id.as_deref(), Some(fresh));
        assert_eq!(
            Storage::new_unwatched(profile).unwrap().load().unwrap()[0]
                .agent_session_id
                .as_deref(),
            Some(fresh)
        );
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
