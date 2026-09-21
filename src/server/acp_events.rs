//! The ACP event listener.

use crate::server::push::StatusChange;
use crate::session::Instance;
use crate::session::Status;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use super::state::{instance_lock_in, AppState};
use crate::server::{acp_ws, api};

/// One task instead of two halves the broadcast clone count and locks `state.instances`
/// once per event instead of twice for the events (e.g.
pub(super) async fn acp_event_listener(state: Arc<AppState>) {
    let mut rx = state.acp_events_tx.subscribe();
    loop {
        let frame = match rx.recv().await {
            Ok(f) => f,
            // Lagged.
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
                    &state.acp_control_cache,
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

        // Detect wake-fire.
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

        // Approval push.
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

        // Clear push.
        if let crate::acp::state::Event::ApprovalResolved { decision, .. } = frame.event.as_ref() {
            record_approval_decision(&state, *decision);
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let seq = frame.seq;
            tokio::spawn(async move {
                acp_ws::trigger_approval_clear_push(&state_for_push, &session_id, seq).await;
            });
        }

        // Question push.
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

        // Clear push for an answered question, mirroring the approval clear above.
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

        // Recall cache.
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

        // Smart-rename defer.
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
                        // Automatic turn-end trigger.
                        false,
                    )
                    .await;
                });
            } else {
                // A `prompt_complete` Stopped without any persisted UserPromptSent is
                // unexpected.
                tracing::debug!(
                    target: "smart_rename",
                    session = %frame.session_id,
                    "trigger fired but event store has no first-turn context; skipping"
                );
            }
        }

        // Conversation-summary defer.
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

        // Gated on `reads_activity_flags`: a cold cache (nothing has
        // hydrated this session since the daemon started, e.g. right after a
        // restart with a reattached worker) must not cost every other arm a
        // full log replay on this serial listener task. The replay picks up
        // this frame's own event too, since it is persisted to the store
        // before being broadcast, and `fold_control_state` hydrates through
        // the cache's per-session lock, so a second frame for the same cold
        // session waits for the first rather than double-hydrating (#4001).
        if reads_activity_flags(frame.event.as_ref())
            && !state.acp_control_cache.is_hydrated(&frame.session_id)
        {
            state
                .session_service
                .fold_control_state(&frame.session_id)
                .await;
        }
        let turn_active_after = state.acp_control_cache.turn_active(&frame.session_id);
        let background_agent_active_after = state
            .acp_control_cache
            .has_active_background_agent(&frame.session_id);
        let status_intent = derive_acp_status(
            frame.event.as_ref(),
            turn_active_after,
            background_agent_active_after,
        );
        let acp_change = derive_acp_session_change(frame.event.as_ref());
        let load_session_capability = match (frame.event.as_ref(), frame.worker_generation) {
            (
                crate::acp::state::Event::PromptCapabilities {
                    load_session: Some(capable),
                    ..
                },
                Some(generation),
            ) => Some((*capable, generation)),
            _ => None,
        };
        if status_intent.is_none() && acp_change.is_none() && load_session_capability.is_none() {
            continue;
        }

        // Acquire `instances` once for both branches.
        let (profile_to_save, unread_profile) = {
            let mut instances = state.instances.write().await;
            let Some(inst) = instances.iter_mut().find(|i| i.id == frame.session_id) else {
                continue;
            };
            if !inst.is_structured() {
                continue;
            }
            // Check while holding the instance lock.
            if let Some((capable, generation)) = load_session_capability {
                if state
                    .acp_supervisor
                    .is_current_worker_generation(&frame.session_id, generation)
                    .await
                {
                    inst.acp_load_session_capable = Some(capable);
                }
            }

            // Snapshotting around the call is exactly "the transition `apply_status_intent`
            // actually applied".
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

/// Tally a resolved approval for the opt-in telemetry snapshot.
fn record_approval_decision(state: &AppState, decision: crate::acp::approvals::ApprovalDecision) {
    use crate::acp::approvals::ApprovalDecision;
    use std::sync::atomic::Ordering::Relaxed;
    let counter = match decision {
        ApprovalDecision::Allow => &state.telemetry_structured.approvals_allow,
        ApprovalDecision::AllowAlways => &state.telemetry_structured.approvals_allow_always,
        ApprovalDecision::Deny => &state.telemetry_structured.approvals_deny,
        ApprovalDecision::Cancelled => return,
    };
    counter.fetch_add(1, Relaxed);
}

/// Seed each acp-enabled session's `Instance.status` from the most recent lifecycle event
/// in the on-disk event log.
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
        // No live control-state fold to consult at boot (the cache is
        // cold and boot deliberately does not hydrate it), so a background
        // sub-agent outstanding across a restart reads as Idle here; the
        // reconciler and the next live event correct it once the tailer
        // resumes (#4001).
        let Some(intent) = derive_acp_status(&event, false, false) else {
            continue;
        };
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            // At startup the on-disk event log is the whole truth.
            if inst.status == Status::Stopped
                && matches!(intent, StatusIntent::Set(Status::Running | Status::Waiting))
            {
                inst.status = Status::Idle;
            }
            apply_status_intent(inst, Some(intent), &state.status_tx);
        }
    }
}

/// Fold a derived `StatusIntent` into an `Instance`.
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
            // A Stopped session must not be woken by a trailing worker event.
            if inst.status == Status::Stopped {
                return;
            }
            s
        }
        // Background sub-agent events must not speak for the main turn: they
        // preserve its Waiting (a pending approval/elicitation) and Error (a
        // dead connection the supervisor is still respawning), and Stopped
        // likewise stays until the main turn's own lifecycle moves it. The
        // main turn's own events (plain `Set`) resolve Waiting and Error;
        // Error also heals on a fresh worker attach.
        StatusIntent::SetUnlessHeld(s) => {
            if matches!(
                inst.status,
                Status::Stopped | Status::Waiting | Status::Error
            ) {
                return;
            }
            s
        }
        // HealError comes only from AcpSessionAssigned / RateLimitAuto Resumed, both
        // emitted when a fresh worker attaches and never as trailing post-stop events.
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
    // last_accessed_at is deliberately NOT stamped here (#3465 residual).
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

/// Whether a structured row whose ACP status just moved should take the automatic unread
/// mark, i.e. whether its turn just finished.
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

/// Write the automatic unread mark for `id` to its profile store, then mirror it into
/// daemon memory.
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

/// Re-derive every structured row's status from the durable event log after the ACP
/// broadcast dropped frames, marking any row whose turn ended while we were not listening.
pub(super) async fn recover_structured_unread_after_lag(
    instances: &RwLock<Vec<Instance>>,
    event_store: &crate::acp::event_store::EventStore,
    instance_locks: &RwLock<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    control_cache: &crate::acp::control_cache::ControlStateCache,
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
        // Unlike boot's cold `(false, false)` seeding, this reads the live
        // control cache (the boot-vs-lag contrast above): hydrated, it is
        // at-or-ahead of this event's fold, so its activity flags are the
        // best available verdict; a miss (never opened, evicted, or
        // forgotten) degrades to boot's conservative verdict. The seed query
        // excludes background events, so a `Stopped` under a still-running
        // sub-agent needs this (#4001).
        let turn_active_after = control_cache.turn_active(&id);
        let background_agent_active_after = control_cache.has_active_background_agent(&id);
        let Some(intent) =
            derive_acp_status(&event, turn_active_after, background_agent_active_after)
        else {
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

/// Fold a derived `AcpSessionChange` into an `Instance`.
pub(super) fn apply_acp_session_change(
    inst: &mut Instance,
    session_id: &str,
    change: Option<&AcpSessionChange>,
) -> Option<String> {
    match change? {
        AcpSessionChange::Assigned(new_id) => {
            // A worker just initialized (session/new or session/load), so the session is by
            // definition no longer idle-dormant.
            let cleared_stale_dormant = inst.idle_dormant_since.take().is_some();
            let same_acp_session = inst.acp_session_id.as_deref() == Some(new_id.as_str());
            // #2276.
            let cleared_import_pending = if same_acp_session {
                inst.import_pending.take().unwrap_or(false)
            } else {
                false
            };
            if same_acp_session {
                // Same id (a reattach / session/load reuses it).
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
            // A structured fork sets fork_pending + import_pending together at creation and
            // does not pre-pin acp_session_id, so the adapter's new forked id arrives on
            // THIS different-id path.
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
            // A structured fork that failed (or was refused by a resume-only agent) reaches
            // here via SessionContextReset.
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
            // For a profile that forwards its clear alias, AoE never learns the adapter's
            // post-clear conversation id, so the only way to stop a restart from
            // resurrecting the pre-clear conversation via session/load is to drop the
            // stored id now and force a fresh session/new.
            inst.acp_session_id = None;
            inst.fork_pending = None;
            inst.import_pending = None;
        }
    }
    Some(inst.source_profile.clone())
}

/// What an event tells the ACP-session-id listener to do.
#[derive(Debug, PartialEq, Eq, Clone)]
pub(super) enum AcpSessionChange {
    Assigned(String),
    Reset(String),
    /// A user `/clear`.
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

/// What an acp event implies for the sidebar status.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StatusIntent {
    Set(Status),
    /// Like `Set`, but a no-op while the sidebar sits on a status only the
    /// main turn may resolve: `Waiting` (a pending approval/elicitation),
    /// `Stopped` (a deliberate stop), or `Error` (a dead connection awaiting
    /// respawn). Background sub-agent lifecycle events must not speak for
    /// the main turn; its own events (plain `Set`) and `HealError` resolve
    /// those. Used only by the `BackgroundAgent*` arms (#4001).
    SetUnlessHeld(Status),
    HealError,
}

/// Whether `derive_acp_status` reads either activity flag for `event`,
/// i.e. whether a caller must hydrate a cold control cache before calling
/// it. Adjacent to the two flag-reading arms below, not structurally tied to
/// them: nothing enforces the pairing at compile time.
/// `derive_acp_status_ignores_the_flags_off_the_two_reading_arms` covers
/// every arm named today and fails if one of those starts reading the flags
/// without being added here; the two functions sit next to each other so a
/// genuinely new `Event` variant with a flag-reading arm is still on the
/// reviewer to catch in the diff, the test cannot see it.
pub(super) fn reads_activity_flags(event: &crate::acp::Event) -> bool {
    matches!(
        event,
        crate::acp::Event::Stopped { .. } | crate::acp::Event::BackgroundAgentCompleted { .. }
    )
}

/// `turn_active_after` and `background_agent_active_after` are the session's
/// post-event activity flags from the folded control state (`AcpState::
/// turn_active` / `has_active_background_agent()`); misses read `false` and
/// degrade to boot's conservative verdict. `turn_active` itself still gates
/// prompt dispatch and the queue drain and tracks only the main turn. Both
/// `Stopped` and `BackgroundAgentCompleted` resolve Idle only once neither
/// flag is set; the former needs the flags because the cache can be ahead of
/// a lagged frame (a newer turn already opened), the latter because a
/// sub-agent can outlive its own completion event's ordering.
/// `reads_activity_flags` above must name exactly these two arms. See #4001.
pub(crate) fn derive_acp_status(
    event: &crate::acp::Event,
    turn_active_after: bool,
    background_agent_active_after: bool,
) -> Option<StatusIntent> {
    use crate::acp::Event;
    match event {
        Event::UserPromptSent { .. }
        | Event::ApprovalResolved { .. }
        | Event::ElicitationResolved { .. } => Some(StatusIntent::Set(Status::Running)),
        // Agent transcript output means a turn is live even when no UserPromptSent preceded
        // it.
        Event::ThinkingStarted
        | Event::AgentMessageChunk { .. }
        | Event::ToolCallStarted { .. } => Some(StatusIntent::Set(Status::Running)),
        // A launched or still-working background sub-agent keeps the sidebar
        // dot lit even while the main turn is between its own events. Must
        // not override a pending approval/elicitation's Waiting dot, which
        // speaks to the main turn, not the sub-agent (#4001).
        Event::BackgroundAgentLaunched { .. } => Some(StatusIntent::SetUnlessHeld(Status::Running)),
        Event::BackgroundAgentProgress {
            status: crate::acp::state::BackgroundAgentStatus::Running,
            ..
        } => Some(StatusIntent::SetUnlessHeld(Status::Running)),
        // A pending approval or elicitation both block the turn on the
        // user, so the sidebar dot goes yellow either way.
        Event::ApprovalRequested { .. } | Event::ElicitationRequested { .. } => {
            Some(StatusIntent::Set(Status::Waiting))
        }
        // All Stopped reasons surface as Idle, including the rate-limit park.
        //
        // Unless something is still busy after this `Stopped`: a background
        // sub-agent keeps working past its parent (#4001), and the cache can
        // be ahead of a lagged frame (a newer turn already opened), so the
        // arm consults both activity flags. Live, the event itself folds
        // `turn_active` false, so ahead-ness is the only source of `true`.
        Event::Stopped { .. } => Some(StatusIntent::Set(
            if turn_active_after || background_agent_active_after {
                Status::Running
            } else {
                Status::Idle
            },
        )),
        // The last outstanding background agent finished. Only drops to Idle
        // once neither the main turn nor a sibling agent is still active
        // (`turn_active_after` covers a sub-agent launched mid-turn that
        // outlives its own completion event's ordering; the `Stopped` arm
        // consults the same flags). `SetUnlessHeld`
        // because a sibling agent finishing must not clobber a pending
        // approval/elicitation on the main turn (#4001).
        Event::BackgroundAgentCompleted { .. } => Some(StatusIntent::SetUnlessHeld(
            if turn_active_after || background_agent_active_after {
                Status::Running
            } else {
                Status::Idle
            },
        )),
        Event::AgentStartupError { .. } => Some(StatusIntent::Set(Status::Error)),
        // A successful session/new or session/load means the agent is alive.
        Event::AcpSessionAssigned { .. } => Some(StatusIntent::HealError),
        // Auto-resume after a rate-limit park.
        Event::RateLimitAutoResumed { .. } => Some(StatusIntent::HealError),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::AcpBroadcastFrame;
    use crate::server::test_support;

    /// A broadcast only reaches receivers that subscribed before the send, and a spawned
    /// listener subscribes as its first act.
    async fn await_subscribed(state: &AppState) {
        for _ in 0..500 {
            if state.acp_events_tx.receiver_count() > 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        panic!("listener never subscribed");
    }

    /// Poll the session row until `want` holds, bounded so a failure reports the reason.
    async fn await_row(
        state: &AppState,
        id: &str,
        want: fn(&Instance) -> bool,
        why: &str,
    ) -> Instance {
        for _ in 0..500 {
            let row = state
                .instances
                .read()
                .await
                .iter()
                .find(|i| i.id == id)
                .cloned();
            if let Some(row) = row.filter(want) {
                return row;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
        panic!("{why}");
    }

    /// #3741.
    #[test]
    fn approval_tally_counts_the_effective_decision_and_skips_cancellations() {
        use crate::acp::approvals::ApprovalDecision;
        use std::sync::atomic::Ordering::Relaxed;

        let state = test_support::build_test_app_state(vec![]);
        let counts = || {
            let c = &state.telemetry_structured;
            (
                c.approvals_allow.load(Relaxed),
                c.approvals_allow_always.load(Relaxed),
                c.approvals_deny.load(Relaxed),
            )
        };
        assert_eq!(counts(), (0, 0, 0));

        record_approval_decision(&state, ApprovalDecision::Allow);
        record_approval_decision(&state, ApprovalDecision::AllowAlways);
        record_approval_decision(&state, ApprovalDecision::Deny);
        record_approval_decision(&state, ApprovalDecision::Deny);
        assert_eq!(counts(), (1, 1, 2));

        record_approval_decision(&state, ApprovalDecision::Cancelled);
        assert_eq!(counts(), (1, 1, 2), "a cancellation is not a decision");
    }

    /// #3181: only a structured row's own Running -> Idle turn end marks unread.
    #[test]
    fn should_mark_acp_unread_only_on_a_structured_running_to_idle_turn_end() {
        // The helper reads the row *after* `apply_status_intent` ran, so `new` is its
        // current status and `old` the one it moved from.
        let mark = |structured: bool, old, new, enabled, already_unread| {
            let mut inst = Instance::new("row", "/tmp/test");
            if structured {
                inst.view = crate::session::View::Structured;
            }
            inst.status = new;
            inst.unread = already_unread;
            should_mark_acp_unread(&inst, old, enabled)
        };
        use Status::{Error, Idle, Running, Waiting};

        assert!(mark(true, Running, Idle, true, false), "turn finished");
        // The turn stopped while still blocked on the user, who is by construction
        // present for it; an answered approval comes back through Running first, so
        // this is not the answered-then-completed path.
        assert!(
            !mark(true, Waiting, Idle, true, false),
            "blocked on the user"
        );
        assert!(!mark(true, Running, Error, true, false), "crashed");
        assert!(!mark(true, Idle, Running, true, false), "turn starting");
        assert!(!mark(true, Running, Running, true, false), "no transition");
        assert!(!mark(true, Running, Idle, false, false), "feature off");
        // Re-marking would churn the flock once per turn, and would undo a read the
        // user has not been given a new turn to earn.
        assert!(!mark(true, Running, Idle, true, true), "already unread");
        // Terminal rows stay with the tmux poll loop's `decide_passive_transition`.
        assert!(!mark(false, Running, Idle, true, false), "terminal row");
    }

    /// Seed `profile`'s store with `rows`, so a persist closure has a matching id to mark.
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

    /// The commit-check half of `persist_and_mirror_unread`.
    #[tokio::test]
    #[serial_test::serial]
    async fn persist_and_mirror_unread_mirrors_only_a_committed_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());

        let owning = "acp-unread-owner";
        let mut inst = Instance::new("acp-session", "/tmp/acp");
        inst.view = crate::session::View::Structured;
        inst.source_profile = owning.to_string();
        let id = inst.id.clone();
        seed_profile_store(owning, vec![inst.clone()]);
        // The profile the row is *not* in.
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
    /// `flush_passive_transition_defers_unread_until_persist_ok` (in `status_poll.rs`)
    /// locks for the tmux poller.
    #[tokio::test]
    #[serial_test::serial]
    async fn persist_and_mirror_unread_skips_the_mirror_on_a_failed_write() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());

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

    /// Lag replay, the other finding on #3530.
    #[tokio::test]
    #[serial_test::serial]
    async fn recover_structured_unread_after_lag_marks_only_a_missed_turn_end() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::set_unread_enabled(true);

        let profile = "acp-unread-lag-replay";

        // The turn ended while the listener was lagged.
        let mut missed = Instance::new("acp-missed", "/tmp/acp");
        missed.view = crate::session::View::Structured;
        missed.source_profile = profile.to_string();
        missed.status = Status::Running;

        // Already reconciled.
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
        // A cold control cache for these sessions: lag recovery never
        // hydrates it, so the reads miss and degrade to boot's conservative
        // `(false, false)` verdict, the Idle+unread behavior this test pins
        // (#4001).
        let control_cache = crate::acp::control_cache::ControlStateCache::new();

        let instances = RwLock::new(rows);
        let locks = RwLock::new(std::collections::HashMap::new());
        let (status_tx, _rx) = broadcast::channel(16);

        let marked = recover_structured_unread_after_lag(
            &instances,
            &store,
            &locks,
            &control_cache,
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

    /// #4001: the lag-recovery replay must consult the live control cache.
    /// The seed query excludes background events, so the latest status event
    /// is the turn's `Stopped` even though a background sub-agent is still
    /// running; deriving from hardcoded inactivity would resolve Idle under
    /// the live `Running` memory status and mark an unfinished turn unread.
    #[tokio::test]
    #[serial_test::serial]
    async fn recover_structured_unread_after_lag_keeps_running_for_a_live_background_agent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::set_unread_enabled(true);

        let profile = "acp-unread-lag-background";

        // Same shape as the missed turn end: memory still says Running.
        let mut live = Instance::new("acp-live-bg", "/tmp/acp");
        live.view = crate::session::View::Structured;
        live.source_profile = profile.to_string();
        live.status = Status::Running;
        let live_id = live.id.clone();
        let rows = vec![live];
        seed_profile_store(profile, rows.clone());

        let db = temp.path().join("acp-events.db");
        let store = crate::acp::event_store::EventStore::open(&db, 1000).expect("event store");
        store
            .record(
                &live_id,
                1,
                &crate::acp::Event::UserPromptSent {
                    text: "spawn and go".into(),
                    attachments: Vec::new(),
                    prompt_id: None,
                    synthesized: false,
                },
            )
            .expect("record prompt");
        store
            .record(
                &live_id,
                2,
                &crate::acp::Event::BackgroundAgentLaunched {
                    agent_id: "bg-1".into(),
                    tool_call_id: "tc-1".into(),
                    description: "map backend".into(),
                    prompt: "do it".into(),
                    model: "claude-opus-4-8".into(),
                    output_file: "/tmp/bg-1.output".into(),
                    started_at: chrono::Utc::now(),
                },
            )
            .expect("record launch");
        store
            .record(
                &live_id,
                3,
                &crate::acp::Event::Stopped {
                    reason: "prompt_complete".into(),
                },
            )
            .expect("record stop");

        // The daemon's control cache folded the same log, so it still sees
        // the outstanding sub-agent even though the seed query does not.
        let control_cache = crate::acp::control_cache::ControlStateCache::new();
        control_cache.get_or_hydrate(&live_id, || {
            let mut reduced = crate::acp::state::AcpState::new(
                crate::acp::state::AcpSessionId(live_id.clone()),
                crate::acp::state::AgentName("claude".into()),
                None,
            );
            let mut last_seq = 0;
            for (seq, event) in store.replay_from(&live_id, 0) {
                let _ = reduced.apply_event(event);
                last_seq = seq;
            }
            (reduced, last_seq)
        });
        assert!(
            control_cache.has_active_background_agent(&live_id),
            "precondition: the folded cache still sees the sub-agent"
        );

        let instances = RwLock::new(rows);
        let locks = RwLock::new(std::collections::HashMap::new());
        let (status_tx, _rx) = broadcast::channel(16);

        let marked = recover_structured_unread_after_lag(
            &instances,
            &store,
            &locks,
            &control_cache,
            crate::file_watch::FileWatchService::noop(),
            &status_tx,
        )
        .await;

        assert_eq!(marked, 0, "a live background sub-agent is not a turn end");

        let guard = instances.read().await;
        let inst = guard.iter().find(|i| i.id == live_id).expect("row");
        assert_eq!(
            inst.status,
            Status::Running,
            "the Stopped must not resolve Idle under the still-running sub-agent"
        );
        assert!(!inst.unread, "an unfinished turn must not be marked unread");
    }

    /// #4001: a `UserDiffCommentsPrompt` opens a turn in the control state
    /// but is absent from the seed query, so during such a turn the latest
    /// seed event is the previous turn's `Stopped`. Lag recovery must let
    /// the cache's `turn_active` override that stale seed instead of
    /// resolving Idle under the live turn and marking it unread.
    #[tokio::test]
    #[serial_test::serial]
    async fn recover_structured_unread_after_lag_keeps_running_for_a_newer_turn_past_the_seed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::set_unread_enabled(true);

        let profile = "acp-unread-lag-newer-turn";

        let mut live = Instance::new("acp-live-newer-turn", "/tmp/acp");
        live.view = crate::session::View::Structured;
        live.source_profile = profile.to_string();
        live.status = Status::Running;
        let live_id = live.id.clone();
        let rows = vec![live];
        seed_profile_store(profile, rows.clone());

        let db = temp.path().join("acp-events.db");
        let store = crate::acp::event_store::EventStore::open(&db, 1000).expect("event store");
        store
            .record(
                &live_id,
                1,
                &crate::acp::Event::UserPromptSent {
                    text: "first turn".into(),
                    attachments: Vec::new(),
                    prompt_id: None,
                    synthesized: false,
                },
            )
            .expect("record prompt");
        store
            .record(
                &live_id,
                2,
                &crate::acp::Event::Stopped {
                    reason: "prompt_complete".into(),
                },
            )
            .expect("record stop");
        // The newer turn the seed query cannot see.
        store
            .record(
                &live_id,
                3,
                &crate::acp::Event::UserDiffCommentsPrompt {
                    intro: "intro".into(),
                    outro: "outro".into(),
                    is_multi_repo: false,
                    comments: Vec::new(),
                    assembled_markdown: "diff".into(),
                },
            )
            .expect("record diff prompt");

        let control_cache = crate::acp::control_cache::ControlStateCache::new();
        control_cache.get_or_hydrate(&live_id, || {
            let mut reduced = crate::acp::state::AcpState::new(
                crate::acp::state::AcpSessionId(live_id.clone()),
                crate::acp::state::AgentName("claude".into()),
                None,
            );
            let mut last_seq = 0;
            for (seq, event) in store.replay_from(&live_id, 0) {
                let _ = reduced.apply_event(event);
                last_seq = seq;
            }
            (reduced, last_seq)
        });
        assert!(
            control_cache.turn_active(&live_id),
            "precondition: the folded cache sees the newer turn"
        );

        let instances = RwLock::new(rows);
        let locks = RwLock::new(std::collections::HashMap::new());
        let (status_tx, _rx) = broadcast::channel(16);

        let marked = recover_structured_unread_after_lag(
            &instances,
            &store,
            &locks,
            &control_cache,
            crate::file_watch::FileWatchService::noop(),
            &status_tx,
        )
        .await;

        assert_eq!(marked, 0, "a live newer turn is not a turn end");

        let guard = instances.read().await;
        let inst = guard.iter().find(|i| i.id == live_id).expect("row");
        assert_eq!(
            inst.status,
            Status::Running,
            "the stale seed Stopped must not resolve Idle under the newer turn"
        );
        assert!(!inst.unread, "an unfinished turn must not be marked unread");
    }

    /// A capability frame is only applied when it names the worker generation that is
    /// still live, so an event queued by a replaced worker is dropped while the sentinel
    /// event behind it still lands.
    #[tokio::test]
    async fn acp_event_listener_tracks_load_session_capability_updates() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let mut inst = Instance::new("acp-session", "/tmp/acp");
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = Some("same-acp-id".to_string());
        let id = inst.id.clone();
        let state = test_support::build_test_app_state(vec![inst]);
        let first_generation = state.acp_supervisor.test_insert_worker(&id).await;
        let listener = tokio::spawn(acp_event_listener(state.clone()));

        await_subscribed(&state).await;

        let send = |seq, event, worker_generation| {
            state
                .acp_events_tx
                .send(AcpBroadcastFrame {
                    session_id: id.clone(),
                    seq,
                    event: Arc::new(event),
                    worker_generation,
                })
                .expect("listener is subscribed");
        };
        let capability = |load_session| crate::acp::Event::PromptCapabilities {
            image: false,
            audio: false,
            embedded_context: false,
            load_session: Some(load_session),
            steering: false,
        };

        send(1, capability(true), Some(first_generation));
        await_row(
            &state,
            &id,
            |i| i.acp_load_session_capable == Some(true),
            "the active worker capability was not applied",
        )
        .await;

        // Replace the worker, then publish a stale frame from the old generation with a
        // generation-less sentinel behind it.
        state.acp_supervisor.test_remove_worker(&id).await;
        state
            .instances
            .write()
            .await
            .iter_mut()
            .find(|inst| inst.id == id)
            .expect("instance")
            .acp_load_session_capable = None;
        let second_generation = state.acp_supervisor.test_insert_worker(&id).await;
        assert_ne!(first_generation, second_generation);

        send(2, capability(false), Some(first_generation));
        send(
            3,
            crate::acp::Event::AcpSessionAssigned {
                acp_session_id: "replacement-acp-id".to_string(),
            },
            None,
        );
        let row = await_row(
            &state,
            &id,
            |i| i.acp_session_id.as_deref() == Some("replacement-acp-id"),
            "the sentinel event behind the stale frame was not applied",
        )
        .await;
        assert_eq!(
            row.acp_load_session_capable, None,
            "a queued event from the replaced worker must be ignored"
        );

        send(4, capability(false), Some(second_generation));
        await_row(
            &state,
            &id,
            |i| i.acp_load_session_capable == Some(false),
            "the replacement worker capability was not applied",
        )
        .await;

        listener.abort();
        let _ = listener.await;
    }

    /// End to end over `acp_event_listener` itself, the path that actually closes #3181.
    #[tokio::test]
    #[serial_test::serial]
    async fn acp_event_listener_marks_a_finished_turn_unread_on_disk_and_in_memory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());
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

        await_subscribed(&state).await;

        // The turn ends.
        state
            .acp_events_tx
            .send(AcpBroadcastFrame {
                session_id: id.clone(),
                seq: 1,
                event: Arc::new(crate::acp::Event::Stopped {
                    reason: "prompt_complete".into(),
                }),
                worker_generation: None,
            })
            .expect("listener is subscribed");

        let row = await_row(
            &state,
            &id,
            |i| i.unread,
            "daemon memory must mirror the mark, so /api/sessions reports it",
        )
        .await;
        listener.abort();
        let _ = listener.await;

        assert_eq!(row.status, Status::Idle, "the Stopped applied");
        assert!(
            load_profile_row(profile, &id).is_some_and(|i| i.unread),
            "and the mark must be durable, which is the #3181 fix; a memory-only \
             mark is dropped by the next reload"
        );
    }

    /// #4001: after a daemon restart the control cache is cold, and the
    /// live listener's own reads of it never hydrate. A worker reattaching
    /// mid-turn then emits its background sub-agent's completion live, with
    /// no Stopped in between; without hydrating first, both activity flags
    /// read false and the completion wrongly derives Idle, marking the
    /// still-open turn unread. Hydrating must pick up the whole log,
    /// including the still-open `UserPromptSent`, and keep the row Running.
    #[tokio::test]
    #[serial_test::serial]
    async fn acp_event_listener_hydrates_a_cold_cache_for_a_reattached_mid_turn_completion() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::set_unread_enabled(true);

        let profile = "acp-listener-cold-reattach";
        let mut inst = Instance::new("acp-cold-reattach", "/tmp/acp");
        inst.view = crate::session::View::Structured;
        inst.source_profile = profile.to_string();
        // Seeded Idle, not the Running a real boot would leave: the poll
        // below must observe the listener drive Idle -> Running, or a
        // vacuous seed would pass whether or not the fix ran.
        inst.status = Status::Idle;
        let id = inst.id.clone();
        seed_profile_store(profile, vec![inst.clone()]);

        let state = test_support::build_test_app_state(vec![inst]);

        // The whole pre-restart log, persisted but never folded into this
        // (fresh, cold) process's cache: the turn is still open, a sub-agent
        // was launched, and it has now completed. No Stopped.
        state
            .acp_event_store
            .record(
                &id,
                1,
                &crate::acp::Event::UserPromptSent {
                    text: "spawn and go".into(),
                    attachments: Vec::new(),
                    prompt_id: None,
                    synthesized: false,
                },
            )
            .expect("record prompt");
        state
            .acp_event_store
            .record(
                &id,
                2,
                &crate::acp::Event::BackgroundAgentLaunched {
                    agent_id: "bg-1".into(),
                    tool_call_id: "tc-1".into(),
                    description: "map backend".into(),
                    prompt: "do it".into(),
                    model: "claude-opus-4-8".into(),
                    output_file: "/tmp/bg-1.output".into(),
                    started_at: chrono::Utc::now(),
                },
            )
            .expect("record launch");
        let completed = crate::acp::Event::BackgroundAgentCompleted {
            agent_id: "bg-1".into(),
            status: crate::acp::state::BackgroundAgentStatus::Completed,
            tools: Vec::new(),
            result: Some("done".into()),
            warning: None,
            ended_at: chrono::Utc::now(),
        };
        state
            .acp_event_store
            .record(&id, 3, &completed)
            .expect("record completion");

        assert!(
            !state.acp_control_cache.is_hydrated(&id),
            "precondition: a fresh process starts with a cold cache"
        );

        let listener = tokio::spawn(acp_event_listener(state.clone()));
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

        // The reattached worker's tailer resumes and emits the sub-agent's
        // completion as the first live frame this process sees.
        state
            .acp_events_tx
            .send(AcpBroadcastFrame {
                session_id: id.clone(),
                seq: 3,
                event: Arc::new(completed),
                worker_generation: None,
            })
            .expect("listener is subscribed");

        // Poll for the Idle -> Running transition, not the cache's hydrated
        // flag: a yield point (the instances write lock) sits between the
        // hydrating await and the status write landing, so a hydrated cache
        // does not imply the write happened yet. `status` is the whole
        // discriminating signal here: `should_mark_acp_unread` only marks a
        // row unread off an OLD status of Running, and the seed above is
        // Idle, so unread cannot flip regardless of the fix; the guard below
        // checks it anyway in case a future change adds a different path to
        // unread. Without the gated hydration `row.status` never turns
        // Running (a cold read derives Idle) and the loop times out.
        let mut resolved = false;
        for _ in 0..500 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let instances = state.instances.read().await;
            if let Some(row) = instances.iter().find(|i| i.id == id) {
                if row.status == Status::Running {
                    resolved = true;
                    break;
                }
            }
        }
        listener.abort();

        assert!(
            resolved,
            "a live turn must not drop to Idle off a cold-cache miss"
        );
        {
            let instances = state.instances.read().await;
            let row = instances.iter().find(|i| i.id == id).expect("row");
            assert!(
                !row.unread,
                "guard: a resolved live turn must not be unread"
            );
        }
        assert!(
            state.acp_control_cache.is_hydrated(&id),
            "the listener must have hydrated the cold session"
        );
        assert!(
            state.acp_control_cache.turn_active(&id),
            "the replay must pick up the still-open UserPromptSent"
        );
    }

    /// #4001: the `Stopped` arm reads the same activity flags as
    /// `BackgroundAgentCompleted` and needs the same cold-cache hydration.
    /// A worker reattaches with its sub-agent still running, the main turn
    /// ends, and the tailer's next line this process sees is a plain
    /// `Stopped` (no completion yet); without hydrating first, both flags
    /// read false and `Stopped` wrongly derives Idle, marking the still-open
    /// work unread. Hydrating must pick up the outstanding launch and keep
    /// the row Running.
    #[tokio::test]
    #[serial_test::serial]
    async fn acp_event_listener_hydrates_a_cold_cache_for_a_stopped_turn_with_an_outstanding_agent()
    {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::set_unread_enabled(true);

        let profile = "acp-listener-cold-stopped-outstanding";
        let mut inst = Instance::new("acp-cold-stopped-outstanding", "/tmp/acp");
        inst.view = crate::session::View::Structured;
        inst.source_profile = profile.to_string();
        // Seeded Idle, not the Running a real boot would leave: the poll
        // below must observe the listener drive Idle -> Running, or a
        // vacuous seed would pass whether or not the fix ran.
        inst.status = Status::Idle;
        let id = inst.id.clone();
        seed_profile_store(profile, vec![inst.clone()]);

        let state = test_support::build_test_app_state(vec![inst]);

        // The pre-restart log: a sub-agent was launched and is still going
        // (no completion recorded) when the main turn's Stopped arrives.
        state
            .acp_event_store
            .record(
                &id,
                1,
                &crate::acp::Event::UserPromptSent {
                    text: "spawn and go".into(),
                    attachments: Vec::new(),
                    prompt_id: None,
                    synthesized: false,
                },
            )
            .expect("record prompt");
        state
            .acp_event_store
            .record(
                &id,
                2,
                &crate::acp::Event::BackgroundAgentLaunched {
                    agent_id: "bg-1".into(),
                    tool_call_id: "tc-1".into(),
                    description: "map backend".into(),
                    prompt: "do it".into(),
                    model: "claude-opus-4-8".into(),
                    output_file: "/tmp/bg-1.output".into(),
                    started_at: chrono::Utc::now(),
                },
            )
            .expect("record launch");
        let stopped = crate::acp::Event::Stopped {
            reason: "prompt_complete".into(),
        };
        state
            .acp_event_store
            .record(&id, 3, &stopped)
            .expect("record stop");

        assert!(
            !state.acp_control_cache.is_hydrated(&id),
            "precondition: a fresh process starts with a cold cache"
        );

        let listener = tokio::spawn(acp_event_listener(state.clone()));
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

        // The reattached worker's tailer resumes and emits the main turn's
        // Stopped as the first live frame this process sees.
        state
            .acp_events_tx
            .send(AcpBroadcastFrame {
                session_id: id.clone(),
                seq: 3,
                event: Arc::new(stopped),
                worker_generation: None,
            })
            .expect("listener is subscribed");

        // Same discriminating signal as the completion test above: the
        // seeded Idle status means `should_mark_acp_unread` can never flip
        // unread here, so `status` alone is what the poll must watch for,
        // and the guard assertion below checks unread separately.
        let mut resolved = false;
        for _ in 0..500 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let instances = state.instances.read().await;
            if let Some(row) = instances.iter().find(|i| i.id == id) {
                if row.status == Status::Running {
                    resolved = true;
                    break;
                }
            }
        }
        listener.abort();

        assert!(
            resolved,
            "a sub-agent still running past its parent's Stopped must not drop to Idle off a cold-cache miss"
        );
        {
            let instances = state.instances.read().await;
            let row = instances.iter().find(|i| i.id == id).expect("row");
            assert!(
                !row.unread,
                "guard: a resolved live turn must not be unread"
            );
        }
        assert!(
            state.acp_control_cache.is_hydrated(&id),
            "the listener must have hydrated the cold session"
        );
        assert!(
            state.acp_control_cache.has_active_background_agent(&id),
            "the replay must pick up the still-outstanding launch"
        );
    }

    /// #2237 plus the one-shot fork/import markers: a reassignment clears a stale
    /// dormant marker even when the id is unchanged, a consumed fork drops both markers
    /// together so a restart neither re-forks nor re-seeds the transcript, and a change
    /// that is not consuming a fork leaves `import_pending` for the restart that needs it.
    #[test]
    fn apply_acp_session_change_matrix() {
        use AcpSessionChange::*;
        type Row = (Option<String>, Option<String>, Option<bool>, bool);

        fn applied(
            id: Option<&str>,
            fork: Option<&str>,
            import: Option<bool>,
            dormant: bool,
            change: AcpSessionChange,
        ) -> Row {
            let mut inst = Instance::new("seed", "/tmp/seed");
            inst.view = crate::session::View::Structured;
            inst.acp_session_id = id.map(str::to_string);
            inst.fork_pending = fork.map(str::to_string);
            inst.import_pending = import;
            inst.idle_dormant_since = dormant.then(chrono::Utc::now);
            let persist = apply_acp_session_change(&mut inst, "sess-1", Some(&change));
            (
                inst.acp_session_id.clone(),
                inst.fork_pending.clone(),
                inst.import_pending,
                persist.is_some(),
            )
        }
        fn want(id: Option<&str>, fork: Option<&str>, import: Option<bool>, persists: bool) -> Row {
            (
                id.map(str::to_string),
                fork.map(str::to_string),
                import,
                persists,
            )
        }

        assert_eq!(
            applied(Some("sid-1"), None, None, true, Assigned("sid-1".into())),
            want(Some("sid-1"), None, None, true),
            "a stale dormant marker must be cleared, and persisted, on an unchanged id"
        );
        assert_eq!(
            applied(Some("sid-1"), None, None, false, Assigned("sid-1".into())),
            want(Some("sid-1"), None, None, false),
            "an unchanged id with nothing stale is a no-op"
        );
        assert_eq!(
            applied(
                None,
                Some("parent"),
                Some(true),
                false,
                Assigned("child".into())
            ),
            want(Some("child"), None, None, true),
            "the forked id consumes both one-shot markers"
        );
        assert_eq!(
            applied(None, None, Some(true), false, Assigned("new-id".into())),
            want(Some("new-id"), None, Some(true), true),
            "a non-fork assignment keeps import_pending"
        );
        assert_eq!(
            applied(
                Some("stale"),
                Some("parent"),
                Some(true),
                false,
                Reset("fork_failed: boom".into())
            ),
            want(None, None, None, true),
            "a failed fork's markers must clear so the reconciler does not retry it"
        );
        assert_eq!(
            applied(
                Some("dead-id"),
                None,
                Some(true),
                false,
                Reset("session/load failed: gone".into())
            ),
            want(None, None, Some(true), true),
            "a plain load failure clears the dead id only"
        );
        assert_eq!(
            applied(
                Some("pre-clear"),
                Some("parent"),
                Some(true),
                false,
                Cleared
            ),
            want(None, None, None, true),
            "a clear forces a restart into a clean session/new"
        );
    }

    #[test]
    fn derive_acp_session_change_reads_only_session_lifecycle_events() {
        use crate::acp::Event;
        assert_eq!(
            derive_acp_session_change(&Event::AcpSessionAssigned {
                acp_session_id: "uuid-1234".into()
            }),
            Some(AcpSessionChange::Assigned("uuid-1234".into()))
        );
        assert_eq!(
            derive_acp_session_change(&Event::SessionContextReset {
                reason: "session/load failed: bad id".into()
            }),
            Some(AcpSessionChange::Reset(
                "session/load failed: bad id".into()
            ))
        );
        // #3080: a /clear must invalidate the persisted ACP resume id.
        assert_eq!(
            derive_acp_session_change(&Event::SessionCleared),
            Some(AcpSessionChange::Cleared)
        );
        for unrelated in [
            Event::AgentMessageChunk { text: "x".into() },
            Event::Stopped {
                reason: "prompt_complete".into(),
            },
            Event::ThinkingStarted,
        ] {
            assert_eq!(derive_acp_session_change(&unrelated), None);
        }
    }

    #[test]
    fn derive_acp_status_maps_events_to_status_intents() {
        use crate::acp::approvals::{ApprovalDecision, Nonce};
        use crate::acp::elicitations::{Elicitation, ElicitationOutcome};
        use crate::acp::permissions::build_approval;
        use crate::acp::state::{BackgroundAgentStatus, ToolCall};
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
        let elicitation = Elicitation {
            nonce: Nonce("e-1".into()),
            message: "Pick".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions: Vec::new(),
            requested_at: chrono::Utc::now(),
            resolved: None,
        };
        let set = |s: Status| Some(StatusIntent::Set(s));
        let held = |s: Status| Some(StatusIntent::SetUnlessHeld(s));

        // Arms that ignore both activity flags, asserted under both flag
        // settings so one that starts reading them without being added to
        // `reads_activity_flags` fails here. A brand-new `Event` variant with
        // its own flag-reading arm is in neither this table nor the predicate,
        // so extending both stays a reviewer-caught convention (#4001).
        let flag_blind = [
            // Agent-side activity drives Running on its own: a turn resumed by a fired
            // wakeup or a background TaskOutput never sends a UserPromptSent.
            (
                Event::UserPromptSent {
                    prompt_id: None,
                    text: "hi".into(),
                    attachments: Vec::new(),
                    synthesized: false,
                },
                set(Status::Running),
            ),
            (
                Event::AgentMessageChunk { text: "x".into() },
                set(Status::Running),
            ),
            (Event::ThinkingStarted, set(Status::Running)),
            (
                Event::ToolCallStarted {
                    tool_call: tool_call.clone(),
                },
                set(Status::Running),
            ),
            // A pending approval or elicitation blocks the turn on the user, and both
            // recover to Running once resolved.
            (
                Event::ApprovalRequested {
                    approval: build_approval(tool_call, Vec::new()),
                },
                set(Status::Waiting),
            ),
            (
                Event::ApprovalResolved {
                    nonce: Nonce("x".into()),
                    decision: ApprovalDecision::Allow,
                },
                set(Status::Running),
            ),
            (
                Event::ElicitationRequested { elicitation },
                set(Status::Waiting),
            ),
            (
                Event::ElicitationResolved {
                    nonce: Nonce("e-1".into()),
                    outcome: ElicitationOutcome::Accepted,
                    answers: Vec::new(),
                },
                set(Status::Running),
            ),
            // A launched or still-working sub-agent keeps the dot lit, but must
            // not speak for the main turn's Waiting/Stopped/Error.
            (
                Event::BackgroundAgentLaunched {
                    agent_id: "a-1".into(),
                    tool_call_id: "t".into(),
                    description: "desc".into(),
                    prompt: "p".into(),
                    model: "m".into(),
                    output_file: "f".into(),
                    started_at: chrono::Utc::now(),
                },
                held(Status::Running),
            ),
            (
                Event::BackgroundAgentProgress {
                    agent_id: "a-1".into(),
                    status: BackgroundAgentStatus::Running,
                    tool_count: 1,
                    tools: Vec::new(),
                    last_tool: None,
                    last_text: None,
                    at: chrono::Utc::now(),
                },
                held(Status::Running),
            ),
            (
                Event::AgentStartupError {
                    message: "boom".into(),
                },
                set(Status::Error),
            ),
            // A live session heals an Error banner but never an in-progress turn.
            (
                Event::AcpSessionAssigned {
                    acp_session_id: "uuid".into(),
                },
                Some(StatusIntent::HealError),
            ),
            (
                Event::RateLimitAutoResumed {
                    resets_at: chrono::Utc::now(),
                    manual: false,
                },
                Some(StatusIntent::HealError),
            ),
            // ThinkingEnded ends a sub-phase; ThinkingStarted already set Running.
            (Event::ThinkingEnded, None),
        ];
        for (event, want) in flag_blind {
            assert!(!reads_activity_flags(&event), "{event:?}");
            assert_eq!(derive_acp_status(&event, false, false), want, "{event:?}");
            assert_eq!(derive_acp_status(&event, true, true), want, "{event:?}");
        }

        // The two arms that do read the flags resolve Idle only once neither
        // the main turn nor a background sub-agent is still active. Live, a
        // `Stopped` folds `turn_active` false itself, so a true flag there
        // means the cache is ahead of a lagged frame. See #4001.
        let stopped = || Event::Stopped {
            reason: "prompt_complete".into(),
        };
        let completed = || Event::BackgroundAgentCompleted {
            agent_id: "a-1".into(),
            status: BackgroundAgentStatus::Completed,
            tools: Vec::new(),
            result: None,
            warning: None,
            ended_at: chrono::Utc::now(),
        };
        // Every Stopped reason surfaces as Idle, the rate-limit park included.
        let parked = || Event::Stopped {
            reason: "rate_limited".into(),
        };
        let flag_reading = [
            (stopped(), false, false, set(Status::Idle)),
            (stopped(), false, true, set(Status::Running)),
            (stopped(), true, false, set(Status::Running)),
            (parked(), false, false, set(Status::Idle)),
            (completed(), false, false, held(Status::Idle)),
            (completed(), false, true, held(Status::Running)),
            (completed(), true, false, held(Status::Running)),
        ];
        for (event, turn_active, background_active, want) in flag_reading {
            assert!(reads_activity_flags(&event), "{event:?}");
            assert_eq!(
                derive_acp_status(&event, turn_active, background_active),
                want,
                "{event:?} turn={turn_active} background={background_active}"
            );
        }
    }

    /// #4001: stopping a session mid-sub-agent must not leave it reading
    /// Running forever. `Supervisor::shutdown_with_reason`'s teardown path
    /// now publishes a synthetic `BackgroundAgentCompleted { Detached }` for
    /// every outstanding agent before its own `Stopped`; replay this exact
    /// sequence through a real `AcpState` (not hand-fed booleans) to prove
    /// the fold actually clears `has_active_background_agent`, and that the
    /// resumed session's terminating `Stopped` derives Idle off it.
    #[test]
    fn derive_acp_status_resolves_idle_once_teardown_detaches_the_last_agent() {
        use crate::acp::state::{AcpSessionId, AcpState, AgentName, BackgroundAgentStatus};
        use crate::acp::Event;

        let mut state = AcpState::new(AcpSessionId("s-1".into()), AgentName("claude".into()), None);
        state
            .apply_event(Event::UserPromptSent {
                prompt_id: None,
                text: "spawn and go".into(),
                attachments: Vec::new(),
                synthesized: false,
            })
            .unwrap();
        state
            .apply_event(Event::BackgroundAgentLaunched {
                agent_id: "bg-1".into(),
                tool_call_id: "tc-1".into(),
                description: "map backend".into(),
                prompt: "do it".into(),
                model: "claude-opus-4-8".into(),
                output_file: "/tmp/bg-1.output".into(),
                started_at: chrono::Utc::now(),
            })
            .unwrap();
        assert!(
            state.has_active_background_agent(),
            "precondition: the launch is still outstanding"
        );

        // Without this event the fold below would still see the agent as
        // outstanding and the final Stopped would derive Running: this is
        // the bug (#4001), not a hypothetical.
        state
            .apply_event(Event::BackgroundAgentCompleted {
                agent_id: "bg-1".into(),
                status: BackgroundAgentStatus::Detached,
                tools: Vec::new(),
                result: None,
                warning: None,
                ended_at: chrono::Utc::now(),
            })
            .unwrap();
        assert!(
            !state.has_active_background_agent(),
            "a Detached completion closes the record like any other terminal status"
        );

        state
            .apply_event(Event::Stopped {
                reason: "user_stopped".into(),
            })
            .unwrap();
        assert_eq!(
            derive_acp_status(
                &Event::Stopped {
                    reason: "user_stopped".into()
                },
                state.turn_active,
                state.has_active_background_agent(),
            ),
            Some(StatusIntent::Set(Status::Idle)),
            "the resumed session's turn-end must not read Running forever"
        );
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
        // AcpSessionAssigned / RateLimitAutoResumed -> HealError.
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
        // A session that paused on ScheduleWakeup sits Idle.
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

    /// #4001: a background sub-agent's progress must not clobber a pending
    /// approval/elicitation's Waiting dot; the main turn's own
    /// `ApprovalResolved`/`ElicitationResolved` (a plain `Set`) still
    /// recovers it normally.
    #[test]
    fn set_unless_waiting_is_a_noop_while_waiting_but_plain_set_still_recovers() {
        let mut inst = stopped_structured_instance();
        inst.status = Status::Waiting;
        apply(&mut inst, StatusIntent::SetUnlessHeld(Status::Running));
        assert_eq!(
            inst.status,
            Status::Waiting,
            "a background agent's own progress does not resolve the main turn's approval"
        );

        apply(&mut inst, StatusIntent::Set(Status::Running));
        assert_eq!(
            inst.status,
            Status::Running,
            "the main turn's own ApprovalResolved/ElicitationResolved still recovers Waiting"
        );
    }

    #[test]
    fn set_unless_waiting_behaves_like_set_when_not_waiting() {
        let mut inst = stopped_structured_instance();
        inst.status = Status::Idle;
        apply(&mut inst, StatusIntent::SetUnlessHeld(Status::Running));
        assert_eq!(inst.status, Status::Running);
    }

    /// A background sub-agent's lifecycle must not clear the main
    /// connection's Error banner: the tailer keeps draining on a cloned
    /// sender while the supervisor evaluates a respawn. The main turn's
    /// own events (plain `Set`) and `HealError` from a fresh worker attach
    /// both still resolve Error; only `SetUnlessHeld` preserves it.
    #[test]
    fn set_unless_waiting_never_clears_a_main_agent_error() {
        let mut inst = stopped_structured_instance();
        inst.status = Status::Error;
        apply(&mut inst, StatusIntent::SetUnlessHeld(Status::Running));
        assert_eq!(
            inst.status,
            Status::Error,
            "a background sub-agent starting must not clear the main connection's error"
        );

        apply(&mut inst, StatusIntent::SetUnlessHeld(Status::Idle));
        assert_eq!(
            inst.status,
            Status::Error,
            "a background sub-agent finishing must not clear it either"
        );

        apply(&mut inst, StatusIntent::HealError);
        assert_eq!(
            inst.status,
            Status::Idle,
            "a fresh worker attach still resolves it"
        );
    }

    /// #4001: a background agent finishing must not clobber a pending
    /// approval/elicitation on the main turn either.
    #[test]
    fn background_agent_completed_does_not_clobber_waiting() {
        use crate::acp::Event;
        let mut inst = stopped_structured_instance();
        inst.status = Status::Waiting;
        let intent = derive_acp_status(
            &Event::BackgroundAgentCompleted {
                agent_id: "a-1".into(),
                status: crate::acp::state::BackgroundAgentStatus::Completed,
                tools: Vec::new(),
                result: None,
                warning: None,
                ended_at: chrono::Utc::now(),
            },
            false,
            false,
        )
        .expect("BackgroundAgentCompleted derives an intent");
        apply(&mut inst, intent);
        assert_eq!(inst.status, Status::Waiting);
    }

    /// #4001: a sub-agent launched mid-turn can complete before its parent
    /// turn's own `Stopped`. That completion must not drop the sidebar to
    /// Idle (and, via `should_mark_acp_unread`'s Running->Idle edge, must not
    /// mark the still-unfinished turn unread) just because it was the last
    /// background agent outstanding.
    #[test]
    fn background_agent_completed_mid_turn_keeps_running_and_does_not_mark_unread() {
        use crate::acp::Event;
        let mut inst = stopped_structured_instance();
        inst.status = Status::Idle;

        // UserPromptSent: turn starts.
        let intent = derive_acp_status(
            &Event::UserPromptSent {
                prompt_id: None,
                text: "hi".into(),
                attachments: Vec::new(),
                synthesized: false,
            },
            true,
            false,
        )
        .unwrap();
        apply(&mut inst, intent);
        assert_eq!(inst.status, Status::Running);

        // BackgroundAgentLaunched: still running, no other agents yet.
        let intent = derive_acp_status(
            &Event::BackgroundAgentLaunched {
                agent_id: "a-1".into(),
                tool_call_id: "t".into(),
                description: "desc".into(),
                prompt: "p".into(),
                model: "m".into(),
                output_file: "f".into(),
                started_at: chrono::Utc::now(),
            },
            true,
            true,
        )
        .unwrap();
        apply(&mut inst, intent);
        assert_eq!(inst.status, Status::Running);

        // BackgroundAgentCompleted lands before the main turn's Stopped: the
        // main turn is still active and no sibling agent remains, so this
        // must stay Running rather than derive Idle from
        // background_agent_active_after alone.
        let old_status = inst.status;
        let intent = derive_acp_status(
            &Event::BackgroundAgentCompleted {
                agent_id: "a-1".into(),
                status: crate::acp::state::BackgroundAgentStatus::Completed,
                tools: Vec::new(),
                result: None,
                warning: None,
                ended_at: chrono::Utc::now(),
            },
            true,
            false,
        )
        .unwrap();
        apply(&mut inst, intent);
        assert_eq!(
            inst.status,
            Status::Running,
            "the main turn is still active"
        );
        assert!(
            !should_mark_acp_unread(&inst, old_status, true),
            "a mid-turn completion must not mark an unfinished turn unread"
        );

        // The main turn's own Stopped, no agents left: this is the one real
        // turn-end edge, and must still mark unread as before.
        let old_status = inst.status;
        let intent = derive_acp_status(
            &Event::Stopped {
                reason: "prompt_complete".into(),
            },
            false,
            false,
        )
        .unwrap();
        apply(&mut inst, intent);
        assert_eq!(inst.status, Status::Idle);
        assert!(should_mark_acp_unread(&inst, old_status, true));
    }

    /// #4001: `BackgroundAgentCompleted` arriving after the main turn's own
    /// `Stopped` (the ordinary case) still resolves Idle once it is the last
    /// agent outstanding, and must not regress from the mid-turn fix above.
    #[test]
    fn background_agent_completed_after_stopped_still_resolves_idle() {
        use crate::acp::Event;
        let mut inst = stopped_structured_instance();
        inst.status = Status::Running;

        let intent = derive_acp_status(
            &Event::Stopped {
                reason: "prompt_complete".into(),
            },
            false,
            true,
        )
        .unwrap();
        apply(&mut inst, intent);
        assert_eq!(
            inst.status,
            Status::Running,
            "a background agent outlives Stopped"
        );

        let old_status = inst.status;
        let intent = derive_acp_status(
            &Event::BackgroundAgentCompleted {
                agent_id: "a-1".into(),
                status: crate::acp::state::BackgroundAgentStatus::Completed,
                tools: Vec::new(),
                result: None,
                warning: None,
                ended_at: chrono::Utc::now(),
            },
            false,
            false,
        )
        .unwrap();
        apply(&mut inst, intent);
        assert_eq!(inst.status, Status::Idle);
        assert!(should_mark_acp_unread(&inst, old_status, true));
    }

    #[test]
    fn status_intent_transitions_preserve_last_accessed_at() {
        // #3465 residual.
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
        // Full #3465 residual chain on structured rows.
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
        // A deliberate Stop, or a session mid-stop, keeps emitting acp events for a few
        // ticks.
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
        // Daemon restart.
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
                    synthesized: false,
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
