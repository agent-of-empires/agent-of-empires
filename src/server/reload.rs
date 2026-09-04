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

/// Load sessions from all profiles, matching the TUI's "all profiles" view.
pub(super) fn load_all_instances(
    file_watch: &Arc<FileWatchService>,
) -> anyhow::Result<Vec<Instance>> {
    let profiles = match crate::session::list_profiles() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: "server.file_watch",
                error = %e,
                "list_profiles failed; load_all_instances returning empty set"
            );
            return Ok(Vec::new());
        }
    };
    let mut all = Vec::new();
    for profile in &profiles {
        match Storage::new(profile, file_watch.clone()).and_then(|s| s.load()) {
            Ok(mut instances) => {
                for inst in &mut instances {
                    inst.source_profile = profile.clone();
                }
                all.extend(instances);
            }
            Err(e) => {
                tracing::warn!(
                    target: "server.file_watch",
                    profile = %profile,
                    error = %e,
                    "load_all_instances skipped profile; sessions for this profile will be \
                     absent from state until next successful reload"
                );
            }
        }
    }
    Ok(all)
}

/// Carry over the in-memory-only fields from the prior `state.instances`
/// entry into the freshly-loaded one. These fields are `#[serde(skip)]`
/// on `Instance` and would otherwise be reset to default every 2 s when
/// `status_poll_loop` reloads from disk. Adding a new `#[serde(skip)]`
/// field on `Instance` requires extending this function or the field is
/// silently wiped on every poll tick.
pub(super) fn merge_runtime_fields(prior: Instance, mut fresh: Instance) -> Instance {
    fresh.last_error_check = prior.last_error_check;
    fresh.last_start_time = prior.last_start_time;
    // Only preserve `last_error` while the session is still in Error. A healthy
    // `fresh` clears it in `update_status_with_metadata_inner`; carrying the
    // prior string over unconditionally would re-stick a stale error on a now-green
    // session every poll tick when a healthy transition happened through a path that
    // did not explicitly null `last_error` in-memory (issue #1271).
    if fresh.status == Status::Error {
        fresh.last_error = prior.last_error;
    }
    fresh.session_id_poller = prior.session_id_poller;
    fresh.retroactive_capture_excludes = prior.retroactive_capture_excludes;
    fresh
}

/// The prior tick's `#[serde(skip)]` status bookkeeping for one row, the
/// input to [`seed_tick_tracking`].
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct PriorTickTracking {
    ever_confirmed_present: bool,
    unknown_since: Option<std::time::Instant>,
    detection: crate::session::DetectionState,
}

impl PriorTickTracking {
    pub(super) fn of(inst: &Instance) -> Self {
        Self {
            ever_confirmed_present: inst.ever_confirmed_present,
            unknown_since: inst.unknown_since,
            detection: inst.detection,
        }
    }
}

/// Carry the previous tick's status bookkeeping onto a freshly disk-loaded
/// instance, keyed by id. `load_all_instances` unconditionally resets these
/// `#[serde(skip)]` fields to their defaults on every call, so
/// `status_poll_loop` must call this BEFORE running
/// `update_status_with_metadata` on the fresh instance. Each field breaks
/// differently without it:
///
/// - `unknown_since` restarts at `Instant::now()` every 2s tick, so the
///   bounded Unknown->Error escalation window in
///   `update_status_with_metadata_inner` can never elapse (#2865).
/// - `detection` restarts empty, so a `Running -> Idle` the rules did not
///   read off live chrome proposes itself every tick and never meets the
///   confirming poll that would publish it: a hookless manifest agent stays
///   Running for the life of the session (#3642).
///
/// The counterpart carry for the opposite direction, after the status
/// decision has run, lives in `reload_state_instances_from_disk`'s
/// per-`StatusSource` handling.
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

// INVARIANTS for `reload_state_instances_from_disk` (do not break without
// revisiting `tests/serve_disk_reload_helper_equivalence.rs`):
// 1. Both call sites (`status_poll_loop` and `disk_watcher_consumer`) must
//    invoke this helper. They differ in cadence, in what they do BEFORE
//    calling it (tmux scrape lives only in `status_poll_loop`), and in
//    the StatusSource they pass.
// 2. `merge_runtime_fields` is mandatory per-id. Skipping it wipes the
//    #[serde(skip)] runtime fields (`last_error_check`,
//    `last_start_time`, `last_error`, `session_id_poller`,
//    `retroactive_capture_excludes`) that disk reload zeroes by design.
// 3. `merge_runtime_fields` does NOT carry `status`, `last_accessed_at`,
//    `idle_entered_at`, or the `PriorTickTracking` fields
//    (`ever_confirmed_present`, `unknown_since`, `detection`).
//    Those are handled per StatusSource: DiskOnly takes prior.status,
//    `prior.idle_entered_at.or(fresh.idle_entered_at)`, and prior's
//    tracking verbatim (its `fresh` never went through
//    `update_status_with_metadata`, so those fields are still at their
//    zeroed defaults). TmuxApplied takes fresh's status and tracking:
//    the caller (`status_poll_loop`) already seeded `fresh` from the
//    prior tick's tracking before running the tmux scrape and status
//    decision, so `fresh` already holds this tick's authoritative values;
//    restoring the pre-decision prior snapshot here would erase that
//    decision every tick, re-freezing the Unknown->Error escalation window
//    at zero elapsed time (#2865) and dropping a detection awaiting its
//    confirming poll (#3642). `last_accessed_at` is monotonic-max
//    regardless.
// 4. The acp overlay filter is `inst.is_structured()`, never the lazy
//    ACP session id. The latter is set lazily by the ACP handshake
//    and is None for newly-spawned acp sessions; using it as the
//    filter would silently drop overlay coverage for pre-handshake
//    rows.
// 5. `prior_by_id` is built with `.drain(..)` once, then read with
//    `.get()` rather than `.remove()` in the merge loop, so the same map is
//    still populated when `apply_acp_overlay_inplace` runs.
// 6. Polling is canonical. The watcher path
//    adds latency reduction; correctness still holds when it fails.
// 7. `status_poll_loop` and `disk_watcher_consumer` may interleave
//    per-tick; both serialise on `state.instances.write().await`. A
//    DiskOnly merge between a TmuxApplied write and a subsequent tmux
//    scrape can briefly carry the prior status; it self-corrects on
//    the next 2s tick. Polling is canonical (invariant 6) so this is
//    acceptable.
// 8. Every caller must read `state.mutation_epoch` BEFORE its disk read and
//    pass that value as `read_epoch`. `fresh` is a snapshot of
//    `sessions.json`, and `*current = merged` below replaces
//    `state.instances` wholesale, so a membership change that commits
//    between the read and this call would otherwise be undone. It cuts both
//    ways. A delete: the removed row is still in `fresh` and comes straight
//    back. A create: the new row is absent from `fresh`, and since `merged`
//    is built exclusively from `fresh`, the wholesale replace drops the row
//    the create just put in `state.instances`, so `GET /api/sessions` loses
//    it until the next tick re-reads disk. The epoch check drops such a
//    reload rather than applying either. Dropping ids missing from
//    `prior_by_id` is NOT an alternative; that is also how a session
//    created by another process (the CLI, a peer daemon) legitimately
//    enters `state.instances`. The comparison happens under the
//    `state.instances` write lock, and both the delete and the create bump
//    under that same lock, so they are ordered against each other; comparing
//    before taking the lock reopens the race one lock acquisition later.

/// Reload `state.instances` by merging caller-supplied `fresh` against the
/// prior in-memory snapshot per id, then reapplying the acp overlay.
/// The caller is responsible for the disk read and, on the
/// `TmuxApplied` path only, for emitting `state.status_tx`
/// diffs BEFORE invoking the helper.
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

#[doc(hidden)]
pub(crate) async fn reload_state_instances_from_disk(
    state: &Arc<AppState>,
    fresh: Vec<Instance>,
    live_worker_records: Vec<LiveStructuredWorkerRecord>,
    status_source: StatusSource,
    read_epoch: u64,
) {
    // Snapshot suppression here so a worker that unmarks between the
    // caller's input build and the per-id decision cannot combine a
    // cleared mark with a stale row to re-emit the phantom Error
    // transition the suppression exists to prevent. Idempotent on the
    // poll path, where the caller already applied the same override
    // inside `spawn_blocking`.
    let suppressed_ids =
        crate::session::recovery::snapshot_recently_restarted(&state.recently_restarted);

    let mut current = state.instances.write().await;

    // Invariant 8: `fresh` predates a committed create or delete, so folding it
    // in would put a removed row back, or drop a created one. Drop the whole
    // reload rather than filter it: the next poll tick re-reads disk 2s from
    // now and converges, and both mutations are rare enough (each one is a
    // user action) that losing one tick of status updates costs nothing.
    //
    // Read under the `instances` write lock, and before `drain_from` empties
    // `current`, so this is atomic against the mutation. Checking before
    // taking the lock leaves a hole: a reload could pass the check, park on
    // the lock, let a delete take the lock, remove the row and bump, then wake
    // and write its stale snapshot over the removal. Symmetrically for a
    // create, whose row is missing from the stale snapshot entirely. Both
    // mutations bump inside the same lock scope for the same reason. No memory ordering closes that gap; it
    // is a check-then-act race, so the check has to happen under the lock that
    // orders the two writers.
    let current_epoch = state
        .mutation_epoch
        .load(std::sync::atomic::Ordering::SeqCst);
    if current_epoch != read_epoch {
        tracing::debug!(
            target: "server.file_watch",
            read_epoch,
            current_epoch,
            "dropping a disk reload whose snapshot predates a session create or delete"
        );
        return;
    }

    let prior_by_id = PriorById::drain_from(&mut current);

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
                    row.status = prior_status;
                    row.idle_entered_at = prior_idle_entered.or(row.idle_entered_at);
                    // `row` here is a raw disk load (no tmux scrape ran), so
                    // the `#[serde(skip)]` tracking fields are still at their
                    // zeroed defaults; restore the prior tick's.
                    row.ever_confirmed_present = prior_tracking.ever_confirmed_present;
                    row.unknown_since = prior_tracking.unknown_since;
                    row.detection = prior_tracking.detection;
                }
                StatusSource::TmuxApplied => {
                    // Caller already applied tmux scrape to fresh.status;
                    // that is the authoritative value. idle_entered_at is
                    // recomputed by upstream status-transition logic;
                    // trust fresh. Likewise the tracking fields: the caller
                    // seeded them from the prior tick before running the
                    // status decision, so `row` already carries this tick's
                    // advanced values. See #2865 and #3642.
                }
            }
            row.last_accessed_at = prior_last_accessed.max(row.last_accessed_at);
        }
        if suppressed_ids.contains(&row.id) {
            row.status = Status::Starting;
        }
        merged.push(row);
    }

    let repairs = repair_structured_rows_from_live_workers(&mut merged, live_worker_records);

    apply_acp_overlay_inplace(&prior_by_id, &mut merged);

    *current = merged;
    drop(current);

    persist_structured_row_repairs(state, repairs);
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
            );

            assert_eq!(instances[0].status, live, "disk status was {disk:?}");
            assert_eq!(observed_transitions(&instances, &prev), vec![]);
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

    #[test]
    fn seed_tick_tracking_carries_prior_tick_fields_onto_fresh_instance() {
        // `load_all_instances` always resets these `#[serde(skip)]` fields to
        // their defaults, mimicking status_poll_loop's fresh disk load.
        let mut fresh = vec![Instance::new("sess-1", "/tmp/seed")];
        assert!(!fresh[0].ever_confirmed_present);
        assert_eq!(fresh[0].unknown_since, None);
        assert_eq!(
            fresh[0].detection,
            crate::session::DetectionState::default()
        );

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
    }

    #[test]
    fn seed_tick_tracking_leaves_unknown_ids_untouched() {
        let mut fresh = vec![Instance::new("sess-unseen", "/tmp/seed")];
        let prev = std::collections::HashMap::new();

        seed_tick_tracking(&mut fresh, &prev);

        assert!(!fresh[0].ever_confirmed_present);
        assert_eq!(fresh[0].unknown_since, None);
        assert_eq!(
            fresh[0].detection,
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

    /// #3642: the daemon rebuilds every row from disk each tick, so a
    /// detection awaiting its confirming poll only survives through
    /// `seed_tick_tracking`. Without that carry a hookless manifest agent
    /// re-proposes the same Idle every tick, never meets its own proposal,
    /// and the dashboard shows Running for the life of the session.
    ///
    /// Four ticks over a live pane parked on a screen no Claude rule matches,
    /// each one starting from a fresh disk load as production does. The last
    /// two also cover the capture-skip gate (#3600), which only reaches
    /// production once this carry exists: on a skipped tick the status the
    /// row came off disk with has to be the one that stands.
    #[test]
    #[serial_test::serial]
    fn a_proposal_survives_the_tick_that_reloads_its_row_from_disk() {
        if !tmux_available() {
            eprintln!("skipping: tmux not available");
            return;
        }

        // Never mutated: cloning it is this test's disk load, so every
        // `#[serde(skip)]` field starts at its default exactly as
        // `load_all_instances` leaves it.
        let mut on_disk = Instance::new("aoe_test_3642_tick", "/tmp");
        on_disk.status = Status::Running;
        assert_eq!(
            on_disk.tool, "claude",
            "fixture invariant: this test needs an agent with a manifest"
        );

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
        let mut tracking: std::collections::HashMap<String, PriorTickTracking> =
            std::collections::HashMap::new();

        // One daemon tick, reporting the status it settled on and the rule
        // that decided. `window_activity` is supplied rather than scraped so
        // the capture-skip gate is driven, not raced.
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
                &prev,
                &std::collections::HashSet::new(),
                Some(&metadata),
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
    fn merge_runtime_fields_preserves_last_error_while_still_in_error() {
        // Cascade-Err preservation: prior held the error string, fresh re-derived
        // Error from a still-dead pane without re-attaching the message. Carry it.
        let mut prior = Instance::new("seed", "/tmp/seed");
        prior.status = Status::Error;
        prior.last_error = Some("recovery cascade: foo".to_string());

        let mut fresh = Instance::new("seed", "/tmp/seed");
        fresh.status = Status::Error;
        fresh.last_error = None;

        let merged = merge_runtime_fields(prior, fresh);
        assert_eq!(merged.last_error.as_deref(), Some("recovery cascade: foo"));
    }

    #[test]
    fn merge_runtime_fields_drops_stale_last_error_on_healthy_transition() {
        // Issue #1271: prior errored in-memory, the session recovered to Idle
        // through a path that never nulled `last_error`. The fresh poll must not
        // re-stick the stale string on a now-green session.
        let mut prior = Instance::new("seed", "/tmp/seed");
        prior.status = Status::Error;
        prior.last_error = Some("recovery cascade: foo".to_string());

        let mut fresh = Instance::new("seed", "/tmp/seed");
        fresh.status = Status::Idle;
        fresh.last_error = None;

        let merged = merge_runtime_fields(prior, fresh);
        assert_eq!(merged.last_error, None);
    }

    #[test]
    fn merge_runtime_fields_drops_stale_last_error_idle_to_idle() {
        // Both ends healthy but prior still carried a stale string: don't propagate.
        let mut prior = Instance::new("seed", "/tmp/seed");
        prior.status = Status::Idle;
        prior.last_error = Some("stale".to_string());

        let mut fresh = Instance::new("seed", "/tmp/seed");
        fresh.status = Status::Idle;
        fresh.last_error = None;

        let merged = merge_runtime_fields(prior, fresh);
        assert_eq!(merged.last_error, None);
    }
}
