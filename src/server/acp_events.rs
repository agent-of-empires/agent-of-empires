//! The ACP event listener: turning agent events into session status,
//! unread marks, and stored session ids.

use crate::server::push::StatusChange;
use crate::session::Instance;
use crate::session::Status;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use super::state::{instance_lock_in, AppState};
use crate::server::{acp_ws, api};

/// One task instead of two halves the broadcast clone count and locks
/// `state.instances` once per event instead of twice for the events
/// (e.g. `AcpSessionAssigned`) that both consumers care about.
pub(super) async fn acp_event_listener(state: Arc<AppState>) {
    let mut rx = state.acp_events_tx.subscribe();
    loop {
        let frame = match rx.recv().await {
            Ok(f) => f,
            // Lagged: a missed event can desync the sidebar dot or
            // skip persisting an `AcpSessionAssigned`. Status will
            // reconcile on the next event; a missed acp_session_id
            // means at most one restart loses context. Far better to
            // continue than to exit the listener entirely.
            //
            // The unread mark does NOT self-heal like status does: it is
            // edge-triggered on `Running -> Idle`, so a dropped `Stopped` would
            // lose it for good and no later event would reproduce it. The
            // events are durable, recorded before broadcast, so replay the
            // structured rows from the event log before continuing.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    target: "acp.event_listener",
                    skipped,
                    "broadcast lagged; status and acp_session_id may briefly desync"
                );
                let recovered = recover_structured_unread_after_lag(
                    &state.instances,
                    &state.acp_event_store,
                    &state.instance_locks,
                    state.file_watch.clone(),
                    &state.status_tx,
                )
                .await;
                if recovered > 0 {
                    tracing::info!(
                        target: "acp.event_listener",
                        skipped,
                        recovered,
                        "replayed turn-end unread marks missed by the lagged frames"
                    );
                }
                continue;
            }
            // Closed: AppState dropped (shutdown). Exit cleanly.
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::debug!(
                    target: "acp.event_listener",
                    "broadcast channel closed; listener exiting"
                );
                return;
            }
        };

        // Detect wake-fire: a `UserPromptSent` arriving at-or-after a
        // `WakeupScheduled`'s `at` timestamp means the agent's pending
        // wake just fired. Push opt-in to the user's phone so /loop
        // dynamic runs don't need them to keep checking the dashboard.
        // See #1091.
        if matches!(
            frame.event.as_ref(),
            crate::acp::state::Event::UserPromptSent { .. }
        ) {
            match state
                .acp_event_store
                .fired_wakeup_for_prompt(&frame.session_id, frame.seq)
            {
                Some((at, reason)) => {
                    let session_id = frame.session_id.clone();
                    let session_title = state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|i| i.id == session_id)
                        .map(|i| i.title.clone())
                        .unwrap_or_default();
                    tracing::info!(
                        target: "acp.wakeup",
                        session = %session_id,
                        prompt_seq = frame.seq,
                        wake_at = %at,
                        reason = ?reason,
                        "wake-fire detected; dispatching push notification"
                    );
                    let state_for_push = state.clone();
                    tokio::spawn(async move {
                        crate::server::push::fire_wake_fired_push(
                            state_for_push,
                            &session_id,
                            &session_title,
                            reason.as_deref(),
                        )
                        .await;
                    });
                }
                None => {
                    tracing::trace!(
                        target: "acp.wakeup",
                        session = %frame.session_id,
                        prompt_seq = frame.seq,
                        "UserPromptSent: no fired-wake match (regular follow-up)"
                    );
                }
            }
        }

        // Approval push: when the worker emits an `ApprovalRequested`
        // event, trigger a Web Push so the user sees a "needs approval"
        // alert even when the dashboard is backgrounded. Unlike the
        // status-change pushes in `push.rs`, approvals do NOT honour
        // the TUI/web active-session suppression; the service worker
        // still routes focused clients to an in-app toast via the
        // existing `aoe-push` postMessage path. See #1038.
        if let crate::acp::state::Event::ApprovalRequested { approval } = frame.event.as_ref() {
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let approval_title = approval.tool_call.name.clone();
            let destructive = approval.destructive;
            let seq = frame.seq;
            tokio::spawn(async move {
                acp_ws::trigger_approval_push(
                    &state_for_push,
                    &session_id,
                    &approval_title,
                    destructive,
                    seq,
                )
                .await;
            });
        }

        // Clear push: when the approval is handled (on any device), retract
        // the "needs approval" notification that the request push raised, so
        // a backgrounded phone or second computer does not keep showing a
        // stale alert for an already-resolved request. See #2491.
        if matches!(
            frame.event.as_ref(),
            crate::acp::state::Event::ApprovalResolved { .. }
        ) {
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let seq = frame.seq;
            tokio::spawn(async move {
                acp_ws::trigger_approval_clear_push(&state_for_push, &session_id, seq).await;
            });
        }

        // Question push: an `AskUserQuestion` (ElicitationRequested) blocks
        // the turn on the user just like an approval, so it gets the same
        // dedicated, suppression-bypassing push instead of only the generic
        // Waiting one. Same live-event-only path as the approval push above.
        // See #2146.
        if let crate::acp::state::Event::ElicitationRequested { elicitation } = frame.event.as_ref()
        {
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let question = elicitation.message.clone();
            let seq = frame.seq;
            tokio::spawn(async move {
                acp_ws::trigger_question_push(&state_for_push, &session_id, &question, seq).await;
            });
        }

        // Clear push for an answered question, mirroring the approval clear
        // above. See #2491.
        if matches!(
            frame.event.as_ref(),
            crate::acp::state::Event::ElicitationResolved { .. }
        ) {
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let seq = frame.seq;
            tokio::spawn(async move {
                acp_ws::trigger_question_clear_push(&state_for_push, &session_id, seq).await;
            });
        }

        // Recall cache: record the agent's advertised config options so the
        // per-agent defaults settings page can populate its dropdowns without a
        // live session. `record` debounces unchanged snapshots and writes off
        // the async runtime. See #2631.
        if let crate::acp::state::Event::ConfigOptionsUpdated { options } = frame.event.as_ref() {
            if !options.is_empty() {
                let agent = state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|i| i.id == frame.session_id)
                    .map(|i| {
                        i.agent_name
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .unwrap_or(i.tool.as_str())
                            .to_string()
                    });
                if let Some(agent) = agent {
                    let options = options.clone();
                    let now = chrono::Utc::now().to_rfc3339();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = crate::acp::option_catalog::record(&agent, &options, now) {
                            tracing::warn!(
                                target: "acp.event_listener",
                                agent = %agent,
                                error = %e,
                                "failed to record acp option catalog"
                            );
                        }
                    });
                }
            }
        }

        // Smart-rename defer: fire the one-shot only on a clean
        // `prompt_complete` `Event::Stopped`, so it never races the live worker
        // for the same provider API. Fast-path on the event variant BEFORE
        // touching the two sync mutexes so high-volume streaming-delta frames
        // (`AgentMessageChunk`, `ToolCallStarted`, `ThinkingStarted`, ...) skip
        // the locks entirely; the pure predicate then applies the reason
        // allowlist + the two per-session `contains()` checks. See #2348 and
        // the post-merge review nit on #2651.
        let should_rename = matches!(
            frame.event.as_ref(),
            crate::acp::state::Event::Stopped { .. }
        ) && {
            let attempted = state
                .smart_rename_attempted
                .lock()
                .expect("smart_rename_attempted poisoned");
            let inflight = state
                .smart_rename_inflight
                .lock()
                .expect("smart_rename_inflight poisoned");
            crate::session::smart_rename::should_trigger_smart_rename(
                frame.event.as_ref(),
                &frame.session_id,
                &attempted,
                &inflight,
            )
        };
        if should_rename {
            if let Some((first_user_prompt, agent_prose)) =
                state.acp_event_store.first_turn_context(
                    &frame.session_id,
                    crate::session::smart_rename::FIRST_TURN_AGENT_BYTES,
                )
            {
                let state_for_rename = state.clone();
                let session_id = frame.session_id.clone();
                let context = crate::session::smart_rename::render_first_turn(
                    &first_user_prompt,
                    &agent_prose,
                );
                tokio::spawn(async move {
                    crate::session::smart_rename::try_smart_rename(
                        state_for_rename,
                        session_id,
                        crate::session::smart_rename::SmartRenameInput {
                            first_user_prompt,
                            context,
                        },
                        // Automatic turn-end trigger: honor the smart_rename
                        // setting. Only the manual action forces past it (#3039).
                        false,
                    )
                    .await;
                });
            } else {
                // A `prompt_complete` Stopped without any persisted UserPromptSent
                // is unexpected: `publish_user_prompt_with_attachments` runs
                // strictly before `send_prompt` in the ACP handler, so by the
                // time the turn ends the first prompt should be durable in the
                // event store. A silent skip would hide a plumbing bug (attachment
                // rollback, pruning of an old session, race with SessionCleared);
                // surface it at debug so operators can trace it.
                tracing::debug!(
                    target: "smart_rename",
                    session = %frame.session_id,
                    "trigger fired but event store has no first-turn context; skipping"
                );
            }
        }

        // Conversation-summary defer: same clean-turn-boundary discipline as
        // smart-rename. Fast-path on the event variant before the inflight
        // lock so streaming frames skip it; the spawned task re-checks the
        // setting, eligibility, and the byte/turn delta threshold (all of
        // which need config + the event store). See #2808.
        let should_summarize = matches!(
            frame.event.as_ref(),
            crate::acp::state::Event::Stopped { .. }
        ) && {
            let inflight = state
                .summary_inflight
                .lock()
                .expect("summary_inflight poisoned");
            crate::session::conversation_summary::should_trigger_summary(
                frame.event.as_ref(),
                &frame.session_id,
                &inflight,
            )
        };
        if should_summarize {
            let state_for_summary = state.clone();
            let session_id = frame.session_id.clone();
            tokio::spawn(async move {
                crate::session::conversation_summary::try_conversation_summary(
                    state_for_summary,
                    session_id,
                    crate::session::conversation_summary::SummaryTrigger::Auto,
                )
                .await;
            });
        }

        let status_intent = derive_acp_status(frame.event.as_ref());
        let acp_change = derive_acp_session_change(frame.event.as_ref());
        if status_intent.is_none() && acp_change.is_none() {
            continue;
        }

        // Acquire `instances` once for both branches. Releases before
        // the (potentially blocking) sessions.json save.
        let (profile_to_save, unread_profile) = {
            let mut instances = state.instances.write().await;
            let Some(inst) = instances.iter_mut().find(|i| i.id == frame.session_id) else {
                continue;
            };
            if !inst.is_structured() {
                continue;
            }

            // Snapshotting around the call is exactly "the transition
            // `apply_status_intent` actually applied": it assigns `status` in
            // one place and every rejected or no-op path (the trashed /
            // Deleting / Creating guard, the Stopped guard, the ineligible
            // HealError guard, `status == target`) leaves it untouched. We hold
            // the write lock across both reads, so nothing else can move it in
            // between. A future refactor that makes `apply_status_intent`
            // assign `status` more than once has to revisit this.
            let old_status = inst.status;
            apply_status_intent(inst, status_intent, &state.status_tx);
            let unread_profile =
                should_mark_acp_unread(inst, old_status, crate::session::unread_enabled())
                    .then(|| inst.source_profile.clone());

            (
                apply_acp_session_change(inst, &frame.session_id, acp_change.as_ref()),
                unread_profile,
            )
        };

        // The turn just finished, so the row takes the automatic unread mark.
        // This is the sole producer of it for a structured row. The tmux poll
        // loop has no authority over a paneless one, which is why #3162 stopped
        // it reporting phantom transitions for them and so left this gap; the
        // TUI's passive path is gated off them to keep the boolean single-writer.
        //
        // The write has to be durable. `reload_state_instances_from_disk` rebases
        // every row on the disk row on each 2s tick and `merge_runtime_fields`
        // does not carry `unread`, so an in-memory-only mark is gone within two
        // seconds. Memory is mirrored only after the write lands, the same
        // ordering `flush_passive_transition_writes` uses (#2755) and for the
        // same reason: a mark that exists only in daemon memory is served over
        // `/api/sessions`, mirrored into the TUI, and then silently dropped by
        // the next reload.
        //
        // Deliberately not folded into the `profile_to_save` save below:
        // `derive_acp_session_change` yields nothing for `Event::Stopped`, so an
        // identity change and a turn-end can never arrive on the same event and
        // there is no atomicity to win.
        //
        // `persist_and_mirror_unread` owns the lock and commit-check ordering;
        // see its docstring for why both are load-bearing.
        if let Some(profile) = unread_profile {
            let lock = state.instance_lock(&frame.session_id).await;
            persist_and_mirror_unread(
                &state.instances,
                &lock,
                state.file_watch.clone(),
                &frame.session_id,
                profile,
            )
            .await;
        }

        // Persist `acp_session_id` to disk if the field changed.
        // Sync FS (file copy + JSON write) goes through spawn_blocking
        // so the runtime stays responsive under large session lists.
        if let Some(profile) = profile_to_save {
            let session_id_for_log = frame.session_id.clone();
            let session_id_for_save = frame.session_id.clone();
            let profile_for_save = profile.clone();
            let acp_change_for_save = acp_change.clone();
            let file_watch = state.file_watch.clone();
            let save_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let storage = crate::session::Storage::new(&profile_for_save, file_watch)?;
                storage.update(|all, _groups| {
                    if let Some(inst) = all.iter_mut().find(|i| i.id == session_id_for_save) {
                        apply_acp_session_change(
                            inst,
                            &session_id_for_save,
                            acp_change_for_save.as_ref(),
                        );
                    }
                    Ok(())
                })?;
                Ok(())
            })
            .await;
            match save_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(
                        target: "acp.event_listener",
                        session = %session_id_for_log,
                        "save after acp_session_id update: {e}"
                    );
                }
                Err(join_err) => {
                    tracing::warn!(
                        target: "acp.event_listener",
                        session = %session_id_for_log,
                        "spawn_blocking join error during acp_session_id save: {join_err}"
                    );
                }
            }
        }
    }
}

/// Seed each acp-enabled session's `Instance.status` from the most
/// recent lifecycle event in the on-disk event log. Runs once at
/// daemon startup, before the status poll loop and the acp event
/// listener start, so a session that was mid-turn when the previous
/// daemon died doesn't render Idle until the next live event arrives.
/// Acts via the same `apply_status_intent` path as the live listener
/// so push subscribers and the broadcast channel see the seeded
/// transitions as ordinary StatusChange events. See #1103 (B).
pub(crate) async fn seed_acp_statuses(state: Arc<AppState>) {
    let acp_ids: Vec<String> = state
        .instances
        .read()
        .await
        .iter()
        .filter(|i| i.is_structured())
        .map(|i| i.id.clone())
        .collect();
    if acp_ids.is_empty() {
        return;
    }
    for id in acp_ids {
        let Some(event) = state.acp_event_store.latest_seed_status_event(&id) else {
            continue;
        };
        let Some(intent) = derive_acp_status(&event) else {
            continue;
        };
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            // At startup the on-disk event log is the whole truth: nothing
            // is racing the apply, unlike the live stream. If the latest
            // lifecycle event shows a turn was in flight when the previous
            // daemon died, a stale persisted Stopped must not trap the dot
            // grey, so clear it before the Running/Waiting intent applies.
            // A latest Idle/Error (clean or deliberate stop) is left as
            // Stopped by apply_status_intent's guard. See #2248.
            if inst.status == Status::Stopped
                && matches!(intent, StatusIntent::Set(Status::Running | Status::Waiting))
            {
                inst.status = Status::Idle;
            }
            apply_status_intent(inst, Some(intent), &state.status_tx);
        }
    }
}

/// Fold a derived `StatusIntent` into an `Instance`. Pure mutation;
/// callers hold the write lock. Sends a `StatusChange` on
/// `status_tx` so push notifications and the dashboard see the
/// transition like any tmux-driven one.
pub(crate) fn apply_status_intent(
    inst: &mut Instance,
    intent: Option<StatusIntent>,
    status_tx: &broadcast::Sender<StatusChange>,
) {
    let Some(intent) = intent else { return };
    // Genuine in-flight terminal states: never fight them.
    if inst.is_trashed() || matches!(inst.status, Status::Deleting | Status::Creating) {
        return;
    }
    let target = match intent {
        StatusIntent::Set(s) => {
            // A Stopped session must not be woken by a trailing worker
            // event: acp events keep arriving for a few ticks after a
            // Stop, and a deliberate Stop must keep showing Stopped. Only
            // a fresh worker-epoch signal (HealError below) lifts Stopped;
            // the live UserPromptSent that follows the respawn then drives
            // Running. Without this, the chain Stopped -> (trailing prompt)
            // Running -> (trailing stop) Idle would strand a deliberate
            // Stop on Idle.
            if inst.status == Status::Stopped {
                return;
            }
            s
        }
        // HealError comes only from AcpSessionAssigned / RateLimitAuto
        // Resumed, both emitted when a fresh worker attaches and never as
        // trailing post-stop events. So heal a sticky Error AND wake a
        // session out of a stale Stopped (idle-reap or manual stop, then
        // re-prompt): the live worker is provably back. See #2248.
        StatusIntent::HealError => {
            if !matches!(inst.status, Status::Error | Status::Stopped) {
                return;
            }
            Status::Idle
        }
    };
    if inst.status == target {
        return;
    }
    let prev = inst.status;
    inst.status = target;
    let now = chrono::Utc::now();
    // last_accessed_at is deliberately NOT stamped here (#3465 residual):
    // the value relays through DaemonStatusPoller into TUI memory, and
    // save()'s merge_from_tui monotone max persists it ungated, so the
    // touched arm of merge_user_action_diff wiped concurrent archives.
    // Structured rows take real touches from user prompts instead.
    inst.idle_entered_at = if target == Status::Idle {
        Some(now)
    } else {
        None
    };
    let _ = status_tx.send(StatusChange {
        instance_id: inst.id.clone(),
        instance_title: inst.title.clone(),
        old: prev,
        new: target,
        at: now,
    });
}

/// Whether a structured row whose ACP status just moved should take the
/// automatic unread mark, i.e. whether its turn just finished.
///
/// `inst` is the row *after* [`apply_status_intent`] ran and `old_status` is
/// the snapshot taken before it. The predicate is deliberately byte-identical
/// to the one in [`super::status_poll::decide_passive_transition`] and in the
/// TUI's
/// `apply_status_update`, so "a turn just finished" means the same thing on
/// every surface.
///
/// `Running -> Idle` is the whole edge. An approval or elicitation excursion
/// comes back through `Set(Running)` (`ApprovalResolved` /
/// `ElicitationResolved`) before the turn's `Stopped`, so an answered-then-
/// completed turn still ends on this edge and needs no case of its own. A
/// direct `Waiting -> Idle` means the turn stopped while still blocked on the
/// user, who is by construction present for it. Every `Stopped` reason maps to
/// `Idle`, so a rate-limit park marks unread too; that is the same policy
/// terminal sessions get, and a parked session does want attention.
pub(super) fn should_mark_acp_unread(
    inst: &Instance,
    old_status: Status,
    unread_enabled: bool,
) -> bool {
    unread_enabled
        && inst.is_structured()
        && old_status == Status::Running
        && inst.status == Status::Idle
        && !inst.unread
}

/// Write the automatic unread mark for `id` to its profile store, then mirror it
/// into daemon memory. Returns whether the mark actually landed.
///
/// Takes primitives rather than [`AppState`] so it is reachable from tests
/// (`AppState` has no test constructor).
///
/// Ordering rules, both of which cost correctness if dropped:
///
/// 1. **Under `instance_lock`.** The same mutex `PATCH /api/sessions/:id/unread`
///    takes, held across both the write and the mirror. Without it a clear can
///    land between them and leave disk read while memory says unread, and the
///    user's explicit mark-read loses to a mark it happened after. Holding it
///    makes the two orderings the only ones possible, and both are correct: a
///    clear before this marks (the turn genuinely finished afterwards), a clear
///    after this wins (the user read it afterwards).
/// 2. **Only mirror a committed mutation.** `persist_session_update` reports
///    `Ok` for a write whose closure matched no row, so `profile` going stale
///    (a concurrent profile move) would otherwise mark memory off a successful
///    no-op on the *old* profile, and the next reload would drop the
///    notification. The flag reports whether the owning row was really mutated.
///
/// A stale-profile write is not retried. The row is left read rather than
/// half-marked, the turn's mark is simply lost, and the move is rare enough that
/// a re-resolve loop is not worth the added failure surface here.
pub(super) async fn persist_and_mirror_unread(
    instances: &RwLock<Vec<Instance>>,
    instance_lock: &tokio::sync::Mutex<()>,
    file_watch: Arc<crate::file_watch::FileWatchService>,
    id: &str,
    profile: String,
) -> bool {
    let _guard = instance_lock.lock().await;
    let marked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let marked_in_closure = marked.clone();
    let persist_id = id.to_string();
    let persisted = api::persist_session_update(
        profile.clone(),
        "acp turn-end unread",
        file_watch,
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                inst.mark_unread();
                marked_in_closure.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        },
    )
    .await;
    if persisted.is_err() {
        return false;
    }
    if !marked.load(std::sync::atomic::Ordering::SeqCst) {
        tracing::debug!(
            target: "acp.event_listener",
            session = %id,
            profile = %profile,
            "turn-end unread write found no row in that profile store (moved?); \
             not mirroring so memory cannot disagree with disk"
        );
        return false;
    }
    let mut instances = instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        inst.mark_unread();
    }
    true
}

/// Re-derive every structured row's status from the durable event log after the
/// ACP broadcast dropped frames, marking any row whose turn ended while we were
/// not listening. Returns the number of rows marked.
///
/// `acp_events_tx` is a `broadcast` of [`super::state::ACP_CHANNEL_CAPACITY`],
/// and a lagged
/// receiver is told only *how many* frames it missed, never which. Status
/// tolerated that because it is level-triggered: any later event re-derives the
/// right value. The unread mark is edge-triggered, so a dropped `Stopped` loses
/// it permanently, and nothing else would ever produce it.
///
/// The events themselves are durable (recorded before broadcast), so the log is
/// the recovery source. `latest_seed_status_event` is the same query
/// `seed_acp_statuses` uses at boot, and for the same reason: it returns the
/// most recent *lifecycle* event, which is exactly the frame whose loss matters.
///
/// This deliberately does NOT reuse `seed_acp_statuses`, despite the shared
/// shape, because the two differ on both points that matter:
///
/// - **Boot must not mark.** Its replay re-reads history, so a `Stopped` from
///   before the restart would re-mark a row the user has already read, on every
///   restart. Here a `Stopped` under a still-`Running` memory status is evidence
///   of a turn that ended during this daemon's life and was missed.
/// - **Boot lifts a stale `Stopped`**, because a persisted `Stopped` from a
///   daemon that died mid-turn would otherwise trap the dot grey. Mid-run a
///   `Stopped` in memory is a deliberate stop, so `apply_status_intent`'s guard
///   should keep it.
pub(super) async fn recover_structured_unread_after_lag(
    instances: &RwLock<Vec<Instance>>,
    event_store: &crate::acp::event_store::EventStore,
    instance_locks: &RwLock<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    file_watch: Arc<crate::file_watch::FileWatchService>,
    status_tx: &broadcast::Sender<StatusChange>,
) -> usize {
    let ids: Vec<String> = instances
        .read()
        .await
        .iter()
        .filter(|i| i.is_structured())
        .map(|i| i.id.clone())
        .collect();
    let unread_enabled = crate::session::unread_enabled();
    let mut marked = 0usize;
    for id in ids {
        let Some(event) = event_store.latest_seed_status_event(&id) else {
            continue;
        };
        let Some(intent) = derive_acp_status(&event) else {
            continue;
        };
        // Same snapshot-around-apply as the live path, so "the transition that
        // was actually applied" means the same thing in both.
        let unread_profile = {
            let mut guard = instances.write().await;
            let Some(inst) = guard.iter_mut().find(|i| i.id == id) else {
                continue;
            };
            if !inst.is_structured() {
                continue;
            }
            let old_status = inst.status;
            apply_status_intent(inst, Some(intent), status_tx);
            should_mark_acp_unread(inst, old_status, unread_enabled)
                .then(|| inst.source_profile.clone())
        };
        if let Some(profile) = unread_profile {
            let lock = instance_lock_in(instance_locks, &id).await;
            if persist_and_mirror_unread(instances, &lock, file_watch.clone(), &id, profile).await {
                marked += 1;
            }
        }
    }
    marked
}

/// Fold a derived `AcpSessionChange` into an `Instance`. Returns the
/// owning profile when sessions.json needs to be re-saved (so the new
/// `acp_session_id` survives daemon restart), or `None` if the
/// change was a no-op or no change was emitted.
pub(super) fn apply_acp_session_change(
    inst: &mut Instance,
    session_id: &str,
    change: Option<&AcpSessionChange>,
) -> Option<String> {
    match change? {
        AcpSessionChange::Assigned(new_id) => {
            // A worker just initialized (session/new or session/load), so the
            // session is by definition no longer idle-dormant. Clear any
            // marker now: a stale one left by a non-user respawn (e.g. the
            // build-stale respawn #1754, which brings the worker back without
            // a user wake) otherwise makes the reconciler's
            // `!is_idle_dormant()` resume filter refuse to bring the session
            // back after this worker later dies, deadlocking a queued prompt
            // that the client parked waiting for a worker that never returns.
            // See #2237.
            let cleared_stale_dormant = inst.idle_dormant_since.take().is_some();
            let same_acp_session = inst.acp_session_id.as_deref() == Some(new_id.as_str());
            // #2276: clear import_pending only when the assigned id matches the
            // imported one, i.e. the import's session/load actually landed and
            // its replay is now in the event store. A fallback session/new (or
            // a stale worker) reports a different id; consuming the marker then
            // would block a later retry from re-seeding the transcript.
            let cleared_import_pending = if same_acp_session {
                inst.import_pending.take().unwrap_or(false)
            } else {
                false
            };
            if same_acp_session {
                // Same id (a reattach / session/load reuses it). Only persist
                // if we actually cleared a stale dormant marker or the import
                // flag; otherwise the id is already on disk and there is
                // nothing to rewrite.
                if cleared_stale_dormant || cleared_import_pending {
                    tracing::info!(
                        target: "acp.event_listener",
                        session = %session_id,
                        cleared_import_pending,
                        "cleared stale idle-dormant / import marker on worker (re)assign"
                    );
                    return Some(inst.source_profile.clone());
                }
                return None;
            }
            tracing::info!(
                target: "acp.event_listener",
                session = %session_id,
                acp_session_id = %new_id,
                "persisting agent-assigned ACP session id"
            );
            inst.acp_session_id = Some(new_id.clone());
            // A structured fork sets fork_pending + import_pending together at
            // creation and does not pre-pin acp_session_id, so the adapter's
            // new forked id arrives on THIS different-id path. Consume both
            // one-shot markers together: a restart resumes the child via
            // session/load instead of re-forking the parent, and leaving
            // import_pending set would make that resume re-seed the transcript
            // into an already-populated store (duplicate-key corruption, the
            // #2276 class). Gate the import clear on fork_pending having been
            // set, so a non-fork different-id assignment leaves import_pending
            // alone for its own retry.
            if inst.fork_pending.take().is_some() {
                inst.import_pending = None;
            }
        }
        AcpSessionChange::Reset(reason) => {
            tracing::info!(
                target: "acp.event_listener",
                session = %session_id,
                %reason,
                "clearing stored ACP session id after a context reset (session/load or session/fork failure)"
            );
            inst.acp_session_id = None;
            // A structured fork that failed (or was refused by a resume-only
            // agent) reaches here via SessionContextReset. Clear the one-shot
            // fork marker so the reconciler stops re-issuing the same failing
            // `session/fork` on every reattach, and drop the paired
            // import_pending the same way the success path does so the fallback
            // spawn is a clean session/new. A session/load-failure reset has no
            // fork pending, so this is a no-op there.
            if inst.fork_pending.take().is_some() {
                inst.import_pending = None;
            }
        }
        AcpSessionChange::Cleared => {
            tracing::info!(
                target: "acp.event_listener",
                session = %session_id,
                "clearing stored ACP session id after a user /clear"
            );
            // For a profile that forwards its clear alias, AoE never learns the
            // adapter's post-clear conversation id, so the only way to stop a
            // restart from resurrecting the pre-clear conversation via
            // session/load is to drop the stored id now and force a fresh
            // session/new. That leaves the post-clear conversation
            // unresumable, which is why the profiles whose adapters withhold
            // the new id drive the reset themselves instead
            // (`clear_requires_driven_reset`); on that path this arm still
            // runs, but the driven burst ends in `AcpSessionAssigned`, which
            // re-pins the id the adapter just minted. Clear the paired
            // fork/import markers unconditionally too: a /clear issued before
            // a pending fork/import resolves must still restart clean, not
            // re-session/fork the parent. See #3080.
            inst.acp_session_id = None;
            inst.fork_pending = None;
            inst.import_pending = None;
        }
    }
    Some(inst.source_profile.clone())
}

/// What an event tells the ACP-session-id listener to do. `None` means
/// the event is irrelevant. Extracted so the JSON-shape parsing has a
/// pure-function test surface.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(super) enum AcpSessionChange {
    Assigned(String),
    Reset(String),
    /// A user `/clear`: drop the stored resume id so the next worker
    /// restart starts a fresh `session/new` instead of resurrecting the
    /// pre-clear conversation via `session/load`. See #3080.
    Cleared,
}

pub(super) fn derive_acp_session_change(event: &crate::acp::Event) -> Option<AcpSessionChange> {
    use crate::acp::Event;
    match event {
        Event::AcpSessionAssigned { acp_session_id } => {
            Some(AcpSessionChange::Assigned(acp_session_id.clone()))
        }
        Event::SessionContextReset { reason } => Some(AcpSessionChange::Reset(reason.clone())),
        Event::SessionCleared => Some(AcpSessionChange::Cleared),
        _ => None,
    }
}

/// What an acp event implies for the sidebar status. `Set` is an
/// unconditional transition; `HealError` only takes effect if the
/// current status is `Error` (used to recover the sidebar from a
/// sticky `AgentStartupError` banner after a successful respawn
/// without clobbering an in-progress Running/Waiting turn).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StatusIntent {
    Set(Status),
    HealError,
}

pub(crate) fn derive_acp_status(event: &crate::acp::Event) -> Option<StatusIntent> {
    use crate::acp::Event;
    match event {
        Event::UserPromptSent { .. }
        | Event::ApprovalResolved { .. }
        | Event::ElicitationResolved { .. } => Some(StatusIntent::Set(Status::Running)),
        // Agent transcript output means a turn is live even when no
        // UserPromptSent preceded it. A fired ScheduleWakeup or a background
        // TaskOutput notification resumes the turn agent-side, streaming only
        // these events; aoe never publishes a prompt for them, so without this
        // the sidebar dot stayed grey through real work. apply_status_intent's
        // guards keep a deliberate Stopped grey and no-op once already Running.
        Event::ThinkingStarted
        | Event::AgentMessageChunk { .. }
        | Event::ToolCallStarted { .. } => Some(StatusIntent::Set(Status::Running)),
        // A pending approval or elicitation both block the turn on the
        // user, so the sidebar dot goes yellow either way.
        Event::ApprovalRequested { .. } | Event::ElicitationRequested { .. } => {
            Some(StatusIntent::Set(Status::Waiting))
        }
        // All Stopped reasons surface as Idle, including the
        // rate-limit park: the worker is not crashed, the user just
        // hit a provider quota and the session is waiting for reset
        // (or for the user to switch to another ACP backend). The
        // dedicated RateLimit banner carries the reset time, so the
        // sidebar pill staying grey is the right signal. See #1281.
        Event::Stopped { .. } => Some(StatusIntent::Set(Status::Idle)),
        Event::AgentStartupError { .. } => Some(StatusIntent::Set(Status::Error)),
        // A successful session/new or session/load means the agent
        // is alive. Heal a sticky Error banner so the sidebar dot
        // reverts from red to grey; do NOT clobber an in-progress
        // Running/Waiting turn (a respawn during an active turn
        // would otherwise stop the spinner mid-stream).
        Event::AcpSessionAssigned { .. } => Some(StatusIntent::HealError),
        // Auto-resume after a rate-limit park: the worker is coming back.
        // Heal any sticky error so the sidebar dot recovers; the imminent
        // fresh spawn emits AcpSessionAssigned and live events right after.
        Event::RateLimitAutoResumed { .. } => Some(StatusIntent::HealError),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::AcpBroadcastFrame;
    use crate::server::test_support;

    /// #3181: the automatic mark's predicate for a structured row, driven off
    /// the live ACP turn-end event. One table rather than a test per case, per
    /// the repo's compile-cost rule.
    #[test]
    fn should_mark_acp_unread_only_on_a_structured_running_to_idle_turn_end() {
        // (name, structured, old_status, new_status, unread_enabled, already_unread, expected)
        let cases = [
            (
                "turn finished",
                true,
                Status::Running,
                Status::Idle,
                true,
                false,
                true,
            ),
            // The turn stopped while still blocked on the user, who is by
            // construction present for it; an answered approval comes back
            // through Running first, so this is not the answered-then-completed
            // path.
            (
                "still blocked on the user",
                true,
                Status::Waiting,
                Status::Idle,
                true,
                false,
                false,
            ),
            (
                "crashed, not finished",
                true,
                Status::Running,
                Status::Error,
                true,
                false,
                false,
            ),
            (
                "turn starting",
                true,
                Status::Idle,
                Status::Running,
                true,
                false,
                false,
            ),
            (
                "no transition applied",
                true,
                Status::Running,
                Status::Running,
                true,
                false,
                false,
            ),
            (
                "feature off",
                true,
                Status::Running,
                Status::Idle,
                false,
                false,
                false,
            ),
            // Re-marking would churn the flock once per turn, and would undo a
            // read the user has not been given a new turn to earn.
            (
                "already unread",
                true,
                Status::Running,
                Status::Idle,
                true,
                true,
                false,
            ),
            // Terminal rows stay with the tmux poll loop's
            // `decide_passive_transition`.
            (
                "terminal row, owned elsewhere",
                false,
                Status::Running,
                Status::Idle,
                true,
                false,
                false,
            ),
        ];
        for (name, structured, old, new, enabled, already_unread, expected) in cases {
            let mut inst = Instance::new(name, "/tmp/test");
            if structured {
                inst.view = crate::session::View::Structured;
            }
            // The helper reads the row *after* `apply_status_intent` ran.
            inst.status = new;
            inst.unread = already_unread;
            assert_eq!(
                should_mark_acp_unread(&inst, old, enabled),
                expected,
                "{name}"
            );
        }
    }

    /// Seed `profile`'s store with `rows`, so a persist closure has a matching
    /// id to mark. Mirrors the shape used by the `flush_passive_transition_*`
    /// tests in `status_poll.rs`.
    fn seed_profile_store(profile: &str, rows: Vec<Instance>) {
        crate::session::Storage::new_unwatched(profile)
            .expect("storage")
            .update(move |instances, _groups| {
                *instances = rows;
                Ok(())
            })
            .expect("seed write");
    }

    fn load_profile_row(profile: &str, id: &str) -> Option<Instance> {
        crate::session::Storage::new_unwatched(profile)
            .expect("storage")
            .load()
            .expect("load")
            .into_iter()
            .find(|i| i.id == id)
    }

    /// The commit-check half of `persist_and_mirror_unread`: memory is mirrored
    /// only when the write actually mutated the owning row.
    ///
    /// The second call is the profile-move case a reviewer raised on #3530.
    /// `persist_session_update` reports `Ok` for a write whose closure matched
    /// nothing, so mirroring on `is_ok()` alone would mark memory off a
    /// successful no-op against a profile the row no longer lives in, and the
    /// next disk reload would silently drop the notification.
    #[tokio::test]
    #[serial_test::serial]
    async fn persist_and_mirror_unread_mirrors_only_a_committed_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        let owning = "acp-unread-owner";
        let mut inst = Instance::new("acp-session", "/tmp/acp");
        inst.view = crate::session::View::Structured;
        inst.source_profile = owning.to_string();
        let id = inst.id.clone();
        seed_profile_store(owning, vec![inst.clone()]);
        // The profile the row is *not* in. Created empty, so the write there
        // succeeds while matching nothing.
        seed_profile_store("acp-unread-stale", Vec::new());

        let instances = RwLock::new(vec![inst]);
        let lock = tokio::sync::Mutex::new(());

        // Stale profile: write succeeds, matches no row, so nothing is mirrored.
        let landed = persist_and_mirror_unread(
            &instances,
            &lock,
            crate::file_watch::FileWatchService::noop(),
            &id,
            "acp-unread-stale".to_string(),
        )
        .await;
        assert!(!landed, "a no-op write must not report the mark as landed");
        assert!(
            !instances.read().await[0].unread,
            "memory must not be marked off a write that matched no row"
        );
        assert!(
            !load_profile_row(owning, &id).expect("row").unread,
            "the owning profile's row must be untouched by a stale-profile write"
        );

        // Owning profile: the mark lands on disk first, then in memory.
        let landed = persist_and_mirror_unread(
            &instances,
            &lock,
            crate::file_watch::FileWatchService::noop(),
            &id,
            owning.to_string(),
        )
        .await;
        assert!(landed);
        assert!(
            load_profile_row(owning, &id).expect("row").unread,
            "the mark must be durable"
        );
        assert!(
            instances.read().await[0].unread,
            "memory must mirror the committed mark"
        );
    }

    /// A failed write must not strand a memory-only mark, the #2755 rule that
    /// `flush_passive_transition_defers_unread_until_persist_ok` (in
    /// `status_poll.rs`) locks for the tmux poller. Separate test rather than a
    /// row in the one above because it needs the store deliberately broken.
    #[tokio::test]
    #[serial_test::serial]
    async fn persist_and_mirror_unread_skips_the_mirror_on_a_failed_write() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        let profile = "acp-unread-write-failure";
        // Making `sessions.json` a directory makes the read-modify-write fail.
        let dir = crate::session::get_profile_dir(profile).expect("profile dir");
        std::fs::create_dir_all(dir.join("sessions.json")).expect("sessions.json dir");

        let mut inst = Instance::new("acp-session", "/tmp/acp");
        inst.view = crate::session::View::Structured;
        inst.source_profile = profile.to_string();
        let id = inst.id.clone();
        let instances = RwLock::new(vec![inst]);
        let lock = tokio::sync::Mutex::new(());

        let landed = persist_and_mirror_unread(
            &instances,
            &lock,
            crate::file_watch::FileWatchService::noop(),
            &id,
            profile.to_string(),
        )
        .await;

        assert!(!landed);
        assert!(
            !instances.read().await[0].unread,
            "a failed persist must not leave a phantom in-memory unread mark"
        );
    }

    /// Lag replay, the other finding on #3530: the ACP broadcast tells a lagged
    /// receiver only how many frames it missed, never which, so a dropped
    /// `Stopped` would lose the turn-end mark permanently (unlike status, which
    /// is level-triggered and re-derives from any later event). The events are
    /// durable, so recovery reads the log.
    ///
    /// Three rows in one pass, all with a durable `Stopped` as their latest
    /// lifecycle event, so the discriminator is the row rather than the event:
    /// only the structured row still sitting at `Running` represents a turn that
    /// ended unobserved.
    #[tokio::test]
    #[serial_test::serial]
    async fn recover_structured_unread_after_lag_marks_only_a_missed_turn_end() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }
        crate::session::set_unread_enabled(true);

        let profile = "acp-unread-lag-replay";

        // The turn ended while the listener was lagged: memory still says
        // Running, the log already has the Stopped we never saw.
        let mut missed = Instance::new("acp-missed", "/tmp/acp");
        missed.view = crate::session::View::Structured;
        missed.source_profile = profile.to_string();
        missed.status = Status::Running;

        // Already reconciled: the Stopped was observed, so there is no
        // transition left to apply and no second mark to make.
        let mut already = Instance::new("acp-already-idle", "/tmp/acp");
        already.view = crate::session::View::Structured;
        already.source_profile = profile.to_string();
        already.status = Status::Idle;

        // A terminal row is not this producer's to touch at all.
        let mut terminal = Instance::new("tmux-row", "/tmp/tmux");
        terminal.source_profile = profile.to_string();
        terminal.status = Status::Running;

        let (missed_id, already_id, terminal_id) =
            (missed.id.clone(), already.id.clone(), terminal.id.clone());
        let rows = vec![missed.clone(), already.clone(), terminal.clone()];
        seed_profile_store(profile, rows.clone());

        let db = temp.path().join("acp-events.db");
        let store = crate::acp::event_store::EventStore::open(&db, 1000).expect("event store");
        for id in [&missed_id, &already_id, &terminal_id] {
            store
                .record(
                    id,
                    1,
                    &crate::acp::Event::Stopped {
                        reason: "prompt_complete".into(),
                    },
                )
                .expect("record stopped");
        }

        let instances = RwLock::new(rows);
        let locks = RwLock::new(std::collections::HashMap::new());
        let (status_tx, _rx) = broadcast::channel(16);

        let marked = recover_structured_unread_after_lag(
            &instances,
            &store,
            &locks,
            crate::file_watch::FileWatchService::noop(),
            &status_tx,
        )
        .await;

        assert_eq!(marked, 1, "only the missed turn-end is a fresh mark");

        let guard = instances.read().await;
        let row = |id: &str| guard.iter().find(|i| i.id == id).expect("row").clone();

        let missed = row(&missed_id);
        assert_eq!(
            missed.status,
            Status::Idle,
            "replay applies the missed Stopped"
        );
        assert!(missed.unread, "and marks the turn that ended unobserved");
        assert!(
            load_profile_row(profile, &missed_id).expect("row").unread,
            "the replayed mark must be durable, not memory-only"
        );

        assert!(
            !row(&already_id).unread,
            "an already-reconciled row has no transition left, so no second mark"
        );
        assert_eq!(
            row(&terminal_id).status,
            Status::Running,
            "a terminal row is left entirely to the tmux poller"
        );
        assert!(!row(&terminal_id).unread);
    }

    /// End to end over `acp_event_listener` itself, the path that actually
    /// closes #3181. The predicate table and the TUI ownership tests all pass
    /// even if the snapshot is taken *after* `apply_status_intent`, the persist
    /// is dropped, or the mirror is reordered, because none of them run the
    /// listener. This one drives a real `Event::Stopped` frame through the
    /// broadcast and asserts both halves of the write.
    #[tokio::test]
    #[serial_test::serial]
    async fn acp_event_listener_marks_a_finished_turn_unread_on_disk_and_in_memory() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }
        crate::session::set_unread_enabled(true);

        let profile = "acp-listener-turn-end";
        let mut inst = Instance::new("acp-session", "/tmp/acp");
        inst.view = crate::session::View::Structured;
        inst.source_profile = profile.to_string();
        // Mid-turn, so the incoming Stopped is a real Running -> Idle edge.
        inst.status = Status::Running;
        let id = inst.id.clone();
        seed_profile_store(profile, vec![inst.clone()]);

        let state = test_support::build_test_app_state(vec![inst]);
        let listener = tokio::spawn(acp_event_listener(state.clone()));

        // A broadcast only reaches receivers that subscribed before the send,
        // and the spawned listener subscribes as its first act. Wait for that
        // rather than racing it; nothing else in this test subscribes, so the
        // count reaching 1 is precisely "the listener is listening".
        for _ in 0..500 {
            if state.acp_events_tx.receiver_count() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        assert!(
            state.acp_events_tx.receiver_count() > 0,
            "listener never subscribed"
        );

        // The turn ends.
        state
            .acp_events_tx
            .send(AcpBroadcastFrame {
                session_id: id.clone(),
                seq: 1,
                event: Arc::new(crate::acp::Event::Stopped {
                    reason: "prompt_complete".into(),
                }),
            })
            .expect("listener is subscribed");

        // The listener owns the write, so poll rather than sleeping a fixed
        // interval: the persist is a real flock'd file write. Wait on the
        // *mirror*, which is the last step, so this cannot abort the listener
        // in between the disk write and the mirror and then blame the mirror.
        let mut mirrored = false;
        for _ in 0..500 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            if state
                .instances
                .read()
                .await
                .iter()
                .any(|i| i.id == id && i.unread)
            {
                mirrored = true;
                break;
            }
        }
        listener.abort();

        assert!(
            mirrored,
            "daemon memory must mirror the mark, so /api/sessions reports it"
        );
        assert!(
            load_profile_row(profile, &id).is_some_and(|i| i.unread),
            "and the mark must be durable, which is the #3181 fix; a memory-only \
             mark is dropped by the next reload"
        );
        let instances = state.instances.read().await;
        let row = instances.iter().find(|i| i.id == id).expect("row present");
        assert_eq!(row.status, Status::Idle, "the Stopped applied");
    }

    // #2237: a worker coming live (AcpSessionAssigned) must clear a stale
    // idle-dormant marker, even when the acp_session_id is unchanged (a
    // session/load reattach reuses it). Without this, a stale marker left by a
    // non-user respawn keeps the reconciler's resume filter skipping the
    // session forever once the worker dies, deadlocking a queued prompt.
    #[test]
    fn acp_session_assigned_clears_stale_dormant_marker_on_same_id() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.acp_session_id = Some("sid-1".to_string());
        inst.idle_dormant_since = Some(chrono::Utc::now());

        // Same id as already stored: the only reason to persist is the
        // stale-dormant clear, so the function must return Some(profile).
        let persist = apply_acp_session_change(
            &mut inst,
            "seed",
            Some(&AcpSessionChange::Assigned("sid-1".to_string())),
        );
        assert!(
            inst.idle_dormant_since.is_none(),
            "dormant marker must be cleared when a worker (re)assigns"
        );
        assert!(
            persist.is_some(),
            "clearing a stale marker must trigger a persist even on an unchanged id"
        );
    }

    // A structured fork mints a brand-new child id on its first session/fork,
    // so the assigned id differs from the (None) acp_session_id and we take the
    // new-assignment path. That path must consume the one-shot fork_pending seed
    // and persist, so a restart resumes the child via session/load rather than
    // re-forking the parent.
    #[test]
    fn assigning_forked_id_clears_fork_pending_and_persists() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = None;
        inst.fork_pending = Some("parent-acp-id".into());
        inst.import_pending = Some(true);

        let profile = apply_acp_session_change(
            &mut inst,
            "sess-1",
            Some(&AcpSessionChange::Assigned("forked-child-id".into())),
        );

        assert_eq!(inst.acp_session_id.as_deref(), Some("forked-child-id"));
        assert_eq!(
            inst.fork_pending, None,
            "fork_pending cleared once the forked id is assigned"
        );
        assert_eq!(
            inst.import_pending, None,
            "import_pending consumed alongside fork_pending so a restart does not re-seed the transcript into the forked store"
        );
        assert!(
            profile.is_some(),
            "must persist so the forked id survives restart"
        );
    }

    // A different-id assignment that is NOT consuming a fork (fork_pending is
    // None) must leave import_pending alone: that marker belongs to the import
    // flow, which lands on the same-id path, and clearing it here would block a
    // legitimate import retry from re-seeding the transcript.
    #[test]
    fn non_fork_assignment_preserves_import_pending() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.acp_session_id = None;
        inst.fork_pending = None;
        inst.import_pending = Some(true);

        let profile = apply_acp_session_change(
            &mut inst,
            "sess-1",
            Some(&AcpSessionChange::Assigned("some-new-id".into())),
        );

        assert_eq!(inst.acp_session_id.as_deref(), Some("some-new-id"));
        assert_eq!(
            inst.import_pending,
            Some(true),
            "a non-fork different-id assignment must not consume import_pending"
        );
        assert!(
            profile.is_some(),
            "a new id assignment must persist regardless of markers"
        );
    }

    // A SessionContextReset from a FAILED structured fork must clear the
    // one-shot fork marker (and its paired import marker) so neither the
    // reconciler nor the supervisor re-issues the same failing session/fork on
    // the next reattach. This is the reducer side of the fork-failure retry-loop
    // fix; the reset carries no new id, so acp_session_id is cleared too.
    #[test]
    fn reset_clears_fork_pending_and_import_pending() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = Some("stale-parent-id".into());
        inst.fork_pending = Some("parent-acp-id".into());
        inst.import_pending = Some(true);

        let profile = apply_acp_session_change(
            &mut inst,
            "sess-1",
            Some(&AcpSessionChange::Reset("fork_failed: boom".into())),
        );

        assert_eq!(inst.acp_session_id, None, "reset clears the stored id");
        assert_eq!(
            inst.fork_pending, None,
            "a failed fork's one-shot marker must clear so it is not retried"
        );
        assert_eq!(
            inst.import_pending, None,
            "import_pending is consumed alongside fork_pending on reset"
        );
        assert!(profile.is_some(), "the reset must persist");
    }

    // A SessionContextReset from a plain session/load failure (no fork pending)
    // must clear the dead id but leave import_pending untouched: that marker
    // belongs to the import flow, and clearing it here would block a legitimate
    // import retry. Mirrors the non-fork assignment guard.
    #[test]
    fn reset_without_fork_pending_preserves_import_pending() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.acp_session_id = Some("dead-id".into());
        inst.fork_pending = None;
        inst.import_pending = Some(true);

        let profile = apply_acp_session_change(
            &mut inst,
            "sess-1",
            Some(&AcpSessionChange::Reset("session/load failed: gone".into())),
        );

        assert_eq!(inst.acp_session_id, None, "reset clears the dead id");
        assert_eq!(
            inst.import_pending,
            Some(true),
            "a non-fork reset must not consume import_pending"
        );
        assert!(profile.is_some(), "the reset must persist");
    }

    #[test]
    fn acp_session_assigned_same_id_no_marker_is_noop() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.acp_session_id = Some("sid-1".to_string());
        inst.idle_dormant_since = None;
        // Same id, nothing stale to clear: must stay a no-op (no rewrite).
        let persist = apply_acp_session_change(
            &mut inst,
            "seed",
            Some(&AcpSessionChange::Assigned("sid-1".to_string())),
        );
        assert!(
            persist.is_none(),
            "unchanged id with no stale marker is a no-op"
        );
    }

    // #3080: a user /clear emits Event::SessionCleared. Before the fix this
    // derived no session change, so the stale ACP id survived on disk and the
    // next worker restart replayed the pre-clear conversation via session/load.
    // It must now derive a Cleared change so the stored id is dropped.
    #[test]
    fn session_cleared_derives_cleared_change() {
        assert_eq!(
            derive_acp_session_change(&crate::acp::Event::SessionCleared),
            Some(AcpSessionChange::Cleared),
            "a /clear must invalidate the persisted ACP resume id"
        );
    }

    // Applying Cleared must null the stored id and force a clean restart by
    // dropping the paired fork/import markers too: a /clear issued before a
    // pending fork/import resolves must still restart as session/new, not
    // re-session/fork the parent. See #3080.
    #[test]
    fn cleared_nulls_stored_id_and_pending_markers() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = Some("pre-clear-id".into());
        inst.fork_pending = Some("parent-acp-id".into());
        inst.import_pending = Some(true);

        let profile =
            apply_acp_session_change(&mut inst, "sess-1", Some(&AcpSessionChange::Cleared));

        assert_eq!(inst.acp_session_id, None, "clear drops the stored id");
        assert_eq!(
            inst.fork_pending, None,
            "clear drops fork_pending so restart does not re-fork the parent"
        );
        assert_eq!(
            inst.import_pending, None,
            "clear drops import_pending so restart is a clean session/new"
        );
        assert!(profile.is_some(), "the clear must persist");
    }

    // Regression for the event-ordering the listener sees: an id assigned at
    // connect followed by a later /clear must end with no stored id. See #3080.
    #[test]
    fn assign_then_clear_leaves_no_stored_id() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.view = crate::session::View::Structured;

        apply_acp_session_change(
            &mut inst,
            "sess-1",
            Some(&AcpSessionChange::Assigned("old-id".into())),
        );
        assert_eq!(inst.acp_session_id, Some("old-id".into()));

        apply_acp_session_change(&mut inst, "sess-1", Some(&AcpSessionChange::Cleared));
        assert_eq!(
            inst.acp_session_id, None,
            "a /clear after an assignment must not leave the old id on disk"
        );
    }

    #[test]
    fn derive_acp_status_maps_terminal_events() {
        use crate::acp::approvals::{ApprovalDecision, Nonce};
        use crate::acp::permissions::build_approval;
        use crate::acp::state::ToolCall;
        use crate::acp::Event;
        let tool_call = ToolCall {
            id: "t".into(),
            name: "shell".into(),
            kind: "execute".into(),
            args_preview: "{}".into(),
            started_at: chrono::Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        };
        assert_eq!(
            derive_acp_status(&Event::UserPromptSent {
                prompt_id: None,
                text: "hi".into(),
                attachments: Vec::new(),
            }),
            Some(StatusIntent::Set(Status::Running))
        );
        assert_eq!(
            derive_acp_status(&Event::ApprovalRequested {
                approval: build_approval(tool_call.clone()),
            }),
            Some(StatusIntent::Set(Status::Waiting))
        );
        assert_eq!(
            derive_acp_status(&Event::ApprovalResolved {
                nonce: Nonce("x".into()),
                decision: ApprovalDecision::Allow,
            }),
            Some(StatusIntent::Set(Status::Running))
        );
        // A pending elicitation blocks the turn on the user just like an
        // approval, so the sidebar dot must go yellow (Waiting) and recover
        // to Running on resolution.
        let elicitation = crate::acp::elicitations::Elicitation {
            nonce: Nonce("e-1".into()),
            message: "Pick".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions: Vec::new(),
            requested_at: chrono::Utc::now(),
            resolved: None,
        };
        assert_eq!(
            derive_acp_status(&Event::ElicitationRequested { elicitation }),
            Some(StatusIntent::Set(Status::Waiting))
        );
        assert_eq!(
            derive_acp_status(&Event::ElicitationResolved {
                nonce: Nonce("e-1".into()),
                outcome: crate::acp::elicitations::ElicitationOutcome::Accepted,
                answers: Vec::new(),
            }),
            Some(StatusIntent::Set(Status::Running))
        );
        assert_eq!(
            derive_acp_status(&Event::Stopped {
                reason: "prompt_complete".into()
            }),
            Some(StatusIntent::Set(Status::Idle))
        );
        // Rate-limit park: NOT an error; sidebar stays grey, the
        // dedicated RateLimit banner carries the reset time. See #1281.
        assert_eq!(
            derive_acp_status(&Event::Stopped {
                reason: "rate_limited".into()
            }),
            Some(StatusIntent::Set(Status::Idle))
        );
        assert_eq!(
            derive_acp_status(&Event::AgentStartupError {
                message: "boom".into()
            }),
            Some(StatusIntent::Set(Status::Error))
        );
        // AcpSessionAssigned heals an Error banner only — never
        // clobbers an in-progress Running/Waiting turn.
        assert_eq!(
            derive_acp_status(&Event::AcpSessionAssigned {
                acp_session_id: "uuid".into()
            }),
            Some(StatusIntent::HealError)
        );
        // Rate-limit auto-resume breadcrumb heals like AcpSessionAssigned:
        // the worker is coming back, so clear a sticky error without
        // clobbering an in-progress turn. See #1722.
        assert_eq!(
            derive_acp_status(&Event::RateLimitAutoResumed {
                resets_at: chrono::Utc::now(),
                manual: false,
            }),
            Some(StatusIntent::HealError)
        );
    }

    #[test]
    fn derive_acp_session_change_extracts_assigned_id() {
        use crate::acp::Event;
        let ev = Event::AcpSessionAssigned {
            acp_session_id: "uuid-1234".into(),
        };
        assert_eq!(
            derive_acp_session_change(&ev),
            Some(AcpSessionChange::Assigned("uuid-1234".into()))
        );
    }

    #[test]
    fn derive_acp_session_change_extracts_reset_reason() {
        use crate::acp::Event;
        let ev = Event::SessionContextReset {
            reason: "session/load failed: bad id".into(),
        };
        assert_eq!(
            derive_acp_session_change(&ev),
            Some(AcpSessionChange::Reset(
                "session/load failed: bad id".into()
            ))
        );
    }

    #[test]
    fn derive_acp_session_change_ignores_unrelated_events() {
        use crate::acp::Event;
        assert_eq!(
            derive_acp_session_change(&Event::AgentMessageChunk { text: "x".into() }),
            None
        );
        assert_eq!(
            derive_acp_session_change(&Event::Stopped {
                reason: "prompt_complete".into()
            }),
            None
        );
        assert_eq!(derive_acp_session_change(&Event::ThinkingStarted), None);
    }

    #[test]
    fn derive_acp_status_running_on_agent_activity() {
        use crate::acp::state::ToolCall;
        use crate::acp::Event;
        // A turn that resumes agent-side (fired ScheduleWakeup, background
        // TaskOutput notification) streams only these events, never a
        // UserPromptSent. They must drive Running so the sidebar dot recovers.
        assert_eq!(
            derive_acp_status(&Event::AgentMessageChunk { text: "x".into() }),
            Some(StatusIntent::Set(Status::Running))
        );
        assert_eq!(
            derive_acp_status(&Event::ThinkingStarted),
            Some(StatusIntent::Set(Status::Running))
        );
        assert_eq!(
            derive_acp_status(&Event::ToolCallStarted {
                tool_call: ToolCall {
                    id: "t".into(),
                    name: "shell".into(),
                    kind: "execute".into(),
                    args_preview: "{}".into(),
                    started_at: chrono::Utc::now(),
                    parent_tool_call_id: None,
                    memory_recall: None,
                    diffs: Vec::new(),
                },
            }),
            Some(StatusIntent::Set(Status::Running))
        );
        // ThinkingEnded is a sub-phase terminator, not a work signal; leaving
        // it None avoids needless intents (ThinkingStarted already set Running).
        assert_eq!(derive_acp_status(&Event::ThinkingEnded), None);
    }

    // --- #2248: a structured session must heal out of a stale Stopped ---

    fn stopped_structured_instance() -> Instance {
        let mut inst = Instance::new("s", "/tmp/s");
        inst.view = crate::session::View::Structured;
        inst.status = Status::Stopped;
        inst
    }

    fn apply(inst: &mut Instance, intent: StatusIntent) {
        let tx = broadcast::channel(8).0;
        apply_status_intent(inst, Some(intent), &tx);
    }

    #[test]
    fn heal_error_wakes_a_stopped_session() {
        // AcpSessionAssigned / RateLimitAutoResumed -> HealError: a fresh
        // worker attached, so a stale Stopped from idle-reap or a prior
        // manual stop must heal. This is the #2248 trap: pre-fix the guard
        // froze Stopped and the dot stayed grey through a live turn.
        let mut inst = stopped_structured_instance();
        apply(&mut inst, StatusIntent::HealError);
        assert_eq!(inst.status, Status::Idle);
        // The UserPromptSent that follows the respawn then drives Running.
        apply(&mut inst, StatusIntent::Set(Status::Running));
        assert_eq!(inst.status, Status::Running);
    }

    #[test]
    fn trailing_acp_event_cannot_change_a_trashed_session_status() {
        let mut inst = stopped_structured_instance();
        inst.status = Status::Running;
        inst.trash();

        apply(&mut inst, StatusIntent::Set(Status::Error));

        assert_eq!(
            inst.status,
            Status::Running,
            "trash teardown must not become a user-facing error transition"
        );
    }

    #[test]
    fn agent_activity_wakes_an_idle_session_after_a_fired_wakeup() {
        // A session that paused on ScheduleWakeup sits Idle. When the wake
        // fires the turn resumes agent-side with activity events (no
        // UserPromptSent), so the activity-derived Set(Running) must flip the
        // dot green instead of leaving it grey.
        let mut inst = stopped_structured_instance();
        inst.status = Status::Idle;
        apply(&mut inst, StatusIntent::Set(Status::Running));
        assert_eq!(inst.status, Status::Running);
        assert_eq!(inst.idle_entered_at, None);
    }

    #[test]
    fn heal_error_still_heals_a_sticky_error() {
        let mut inst = stopped_structured_instance();
        inst.status = Status::Error;
        apply(&mut inst, StatusIntent::HealError);
        assert_eq!(inst.status, Status::Idle);
    }

    #[test]
    fn status_intent_transitions_preserve_last_accessed_at() {
        // #3465 residual: the intent applier used to restamp
        // last_accessed_at on every transition. The value relays through
        // DaemonStatusPoller into TUI memory and save()'s merge_from_tui
        // monotone max persists it, so a phantom stamp here wiped
        // concurrent archives through merge_user_action_diff's touched
        // arm. Structured rows take real touches from user prompts
        // (touch_on_prompt_and_wake_if_sunk), so the field stays gesture-only.
        let mut inst = stopped_structured_instance();
        inst.status = Status::Idle;
        let user_touch = chrono::Utc::now() - chrono::Duration::seconds(60);
        inst.last_accessed_at = Some(user_touch);

        apply(&mut inst, StatusIntent::Set(Status::Running));
        assert_eq!(inst.status, Status::Running);
        assert_eq!(inst.idle_entered_at, None);
        assert_eq!(
            inst.last_accessed_at,
            Some(user_touch),
            "a worker-event transition must not fabricate a user-gesture stamp"
        );

        apply(&mut inst, StatusIntent::Set(Status::Idle));
        assert_eq!(inst.status, Status::Idle);
        assert!(inst.idle_entered_at.is_some());
        assert_eq!(
            inst.last_accessed_at,
            Some(user_touch),
            "entering Idle re-anchors idle bookkeeping, not the gesture stamp"
        );
    }

    #[test]
    fn relayed_intent_stamp_wipes_concurrent_archive() {
        // Full #3465 residual chain on structured rows:
        // apply_status_intent stamps daemon memory, save()'s
        // merge_from_tui folds that memory into disk with an ungated
        // monotone max, and a writer holding a pre snapshot from before
        // the stamp loses its archive to merge_user_action_diff's
        // touched arm. Dropping the intent stamp breaks this chain.
        let user_touch = chrono::Utc::now() - chrono::Duration::seconds(60);

        let mut daemon_row = stopped_structured_instance();
        daemon_row.status = Status::Idle;
        daemon_row.last_accessed_at = Some(user_touch);
        apply(&mut daemon_row, StatusIntent::Set(Status::Running));

        let mut disk = stopped_structured_instance();
        disk.status = Status::Idle;
        disk.last_accessed_at = Some(user_touch);
        let pre = disk.clone();

        disk.merge_from_tui(&daemon_row);

        let mut post = pre.clone();
        post.archive();
        disk.merge_user_action_diff(&pre, &post);

        assert!(
            disk.archived_at.is_some(),
            "relayed intent stamp must not wipe a concurrent archive (#3465)"
        );
    }

    #[test]
    fn trailing_set_intents_do_not_wake_a_stopped_session() {
        // A deliberate Stop, or a session mid-stop, keeps emitting acp
        // events for a few ticks. None of those Set intents may revive it,
        // or the chain Stopped -> Running -> Idle would strand a deliberate
        // Stop on Idle.
        for target in [Status::Running, Status::Waiting, Status::Idle] {
            let mut inst = stopped_structured_instance();
            apply(&mut inst, StatusIntent::Set(target));
            assert_eq!(
                inst.status,
                Status::Stopped,
                "target {target:?} woke Stopped"
            );
        }
    }

    #[test]
    fn deleting_and_creating_block_every_intent() {
        for terminal in [Status::Deleting, Status::Creating] {
            let mut inst = stopped_structured_instance();
            inst.status = terminal;
            apply(&mut inst, StatusIntent::Set(Status::Running));
            assert_eq!(inst.status, terminal);
            apply(&mut inst, StatusIntent::HealError);
            assert_eq!(inst.status, terminal);
        }
    }

    #[tokio::test]
    async fn seed_unblocks_a_stopped_session_with_an_in_flight_turn() {
        use crate::acp::Event;
        // Daemon restart: session persisted Stopped, but the last lifecycle
        // event was a UserPromptSent (a turn was in flight when the prior
        // daemon died). Seed must reflect the live turn, not the stale dot.
        let inst = stopped_structured_instance();
        let id = inst.id.clone();
        let state = test_support::build_test_app_state(vec![inst]);
        state
            .acp_event_store
            .record(
                &id,
                1,
                &Event::UserPromptSent {
                    prompt_id: None,
                    text: "go".into(),
                    attachments: Vec::new(),
                },
            )
            .expect("record");
        seed_acp_statuses(state.clone()).await;
        assert_eq!(state.instances.read().await[0].status, Status::Running);
    }

    #[tokio::test]
    async fn seed_preserves_a_deliberate_stop_across_restart() {
        use crate::acp::Event;
        // Latest event is a Stopped (clean / deliberate stop), so the seed
        // leaves the persisted Stopped intact rather than downgrading it.
        let inst = stopped_structured_instance();
        let id = inst.id.clone();
        let state = test_support::build_test_app_state(vec![inst]);
        state
            .acp_event_store
            .record(
                &id,
                1,
                &Event::Stopped {
                    reason: "prompt_complete".into(),
                },
            )
            .expect("record");
        seed_acp_statuses(state.clone()).await;
        assert_eq!(state.instances.read().await[0].status, Status::Stopped);
    }
}
