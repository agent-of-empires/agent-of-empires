//! The ACP event listener: turning agent events into session status,
//! unread marks, and stored session ids.

use crate::acp::state::AcpSessionChange;
use crate::server::push::{publish_status_change, StatusChange};
use crate::session::Instance;
use crate::session::Status;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

use super::state::{instance_lock_in, AppState};
use crate::acp::state::Event;
use crate::server::{acp_ws, api};

#[cfg(test)]
struct AssignmentPause {
    session_id: String,
    seq: Option<u64>,
    arrived: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
    applied: tokio::sync::oneshot::Sender<()>,
}

#[cfg(test)]
static ASSIGNMENT_PAUSE: std::sync::Mutex<Option<AssignmentPause>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) struct AssignmentListenerGate {
    pub arrived: tokio::sync::oneshot::Receiver<()>,
    pub release: tokio::sync::oneshot::Sender<()>,
    pub applied: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
pub(crate) fn pause_assignment_listener_for_test(
    session_id: &str,
    seq: u64,
) -> AssignmentListenerGate {
    assignment_listener_gate_for_test(session_id, Some(seq))
}

#[cfg(test)]
fn assignment_listener_gate_for_test(session_id: &str, seq: Option<u64>) -> AssignmentListenerGate {
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let (applied, application) = tokio::sync::oneshot::channel();
    let mut pending = ASSIGNMENT_PAUSE.lock().unwrap();
    assert!(pending.is_none(), "another assignment gate is installed");
    *pending = Some(AssignmentPause {
        session_id: session_id.to_owned(),
        seq,
        arrived,
        release: released,
        applied,
    });
    AssignmentListenerGate {
        arrived: arrival,
        release,
        applied: application,
    }
}

#[cfg(test)]
struct AssignmentApplicationAck(Option<tokio::sync::oneshot::Sender<()>>);

#[cfg(test)]
impl Drop for AssignmentApplicationAck {
    fn drop(&mut self) {
        if let Some(applied) = self.0.take() {
            let _ = applied.send(());
        }
    }
}

#[cfg(test)]
async fn pause_assignment_for_test(
    frame: &crate::acp::protocol::AcpBroadcastFrame,
) -> AssignmentApplicationAck {
    let pause = {
        let mut pending = ASSIGNMENT_PAUSE.lock().unwrap();
        if pending.as_ref().is_some_and(|pause| {
            pause.session_id == frame.session_id
                && pause.seq.is_none_or(|seq| seq == frame.seq)
                && matches!(
                    frame.event.as_ref(),
                    crate::acp::Event::AcpSessionAssigned { .. }
                )
        }) {
            pending.take()
        } else {
            None
        }
    };
    let Some(pause) = pause else {
        return AssignmentApplicationAck(None);
    };
    let ack = AssignmentApplicationAck(Some(pause.applied));
    let _ = pause.arrived.send(());
    let _ = pause.release.await;
    ack
}

/// Applies live status, unread and session-id observations.
/// One line per lifecycle frame. Approval lines carry the call id, never the
/// title: for Bash-style tools the title is the full command.
/// `Failed` carries the one-shot's last output line, which the one-shot already logged at debug;
/// the warning names only the outcome so child text never reaches the default level.
fn log_smart_rename_outcome(
    session: &str,
    result: &Result<(), crate::session::smart_rename::SmartRenameError>,
) {
    use crate::session::smart_rename::SmartRenameError;
    match result {
        Ok(()) => {}
        Err(SmartRenameError::Skipped(reason)) => {
            tracing::debug!(target: "smart_rename", session = %session, reason = %reason, "smart rename skipped")
        }
        Err(SmartRenameError::TimedOut) => {
            tracing::warn!(target: "smart_rename", session = %session, "smart rename timed out")
        }
        Err(SmartRenameError::Failed(_)) => {
            tracing::warn!(target: "smart_rename", session = %session, "smart rename failed")
        }
    }
}

fn log_frame(frame: &crate::acp::protocol::AcpBroadcastFrame) {
    match frame.event.as_ref() {
        Event::ApprovalRequested { approval } => tracing::info!(
            target: "acp.event_listener",
            session = %frame.session_id,
            call = %approval.tool_call.id,
            kind = %approval.tool_call.kind,
            destructive = approval.destructive,
            nonce = %approval.nonce.0,
            "approval requested"
        ),
        Event::ApprovalResolved { nonce, decision } => tracing::info!(
            target: "acp.event_listener",
            session = %frame.session_id,
            nonce = %nonce.0,
            ?decision,
            "approval resolved"
        ),
        Event::ElicitationRequested { elicitation } => tracing::info!(
            target: "acp.event_listener",
            session = %frame.session_id,
            nonce = %elicitation.nonce.0,
            questions = elicitation.questions.len(),
            "question asked"
        ),
        Event::ElicitationResolved { nonce, outcome, .. } => tracing::info!(
            target: "acp.event_listener",
            session = %frame.session_id,
            nonce = %nonce.0,
            ?outcome,
            "question answered"
        ),
        Event::ConversationCompactionStarted => tracing::info!(
            target: "acp.event_listener",
            session = %frame.session_id,
            "compaction started"
        ),
        Event::ConversationCompacted => tracing::info!(
            target: "acp.event_listener",
            session = %frame.session_id,
            "compaction completed"
        ),
        Event::ConversationCompactionFailed => tracing::warn!(
            target: "acp.event_listener",
            session = %frame.session_id,
            "compaction failed"
        ),
        Event::ConversationCompactionSummary {
            compaction_id,
            text,
        } => tracing::debug!(
            target: "acp.event_listener",
            session = %frame.session_id,
            compaction_id,
            summary_len = text.len(),
            "compaction summary recorded"
        ),
        Event::PromptRejected { reason, .. } => tracing::warn!(
            target: "acp.event_listener",
            session = %frame.session_id,
            reason,
            "prompt rejected"
        ),
        Event::CancelRequested { escalates_at } => tracing::debug!(
            target: "acp.event_listener",
            session = %frame.session_id,
            escalates_at = %escalates_at,
            "turn cancel requested"
        ),
        Event::ModeSwitchFailed { mode_id, reason } => tracing::warn!(
            target: "acp.event_listener",
            session = %frame.session_id,
            mode_id,
            reason,
            "mode switch failed"
        ),
        Event::ConfigOptionSwitchFailed {
            config_id,
            value,
            reason,
        } => tracing::warn!(
            target: "acp.event_listener",
            session = %frame.session_id,
            config_id,
            value,
            reason,
            "config option switch failed"
        ),
        _ => {}
    }
}

pub(super) async fn acp_event_listener(state: Arc<AppState>) {
    let mut rx = state.acp_events_tx.subscribe();
    'frames: loop {
        let received = tokio::select! {
            biased;
            _ = state.shutdown.cancelled() => return,
            frame = rx.recv() => frame,
        };
        let frame = match received {
            Ok(f) => f,
            // Unread is edge-triggered; replay missed durable turn ends.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    target: "acp.event_listener",
                    skipped,
                    "broadcast lagged; status and acp_session_id may briefly desync"
                );
                let recovered = recover_structured_unread_after_lag(
                    &state.instances,
                    &state.session_service,
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
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::debug!(
                    target: "acp.event_listener",
                    "broadcast channel closed; listener exiting"
                );
                return;
            }
        };
        log_frame(&frame);
        #[cfg(test)]
        let _assignment_applied = pause_assignment_for_test(&frame).await;
        let current = frame.scope != crate::acp::protocol::AcpEventScope::HistoryOnly;
        let original_terminal = frame
            .operation
            .is_some_and(|operation| operation.outcome.is_some())
            && matches!(
                frame.event.as_ref(),
                Event::Stopped { .. } | Event::PromptRuntimeError { .. }
            );
        // Resource custody has its own exact allocation identity. It may update
        // background status without promoting its historical scope to Current.
        let resource_effect = frame.resource_ref.is_some()
            && (frame.native_origin.is_some() || frame.host_action)
            && status_policy(frame.event.as_ref()) == Some(StatusPolicy::Background);
        let resolved_callback = matches!(
            frame.event.as_ref(),
            Event::ApprovalResolved { .. } | Event::ElicitationResolved { .. }
        );
        let original_request = if !current
            && matches!(
                frame.event.as_ref(),
                Event::ApprovalRequested { .. } | Event::ElicitationRequested { .. }
            ) {
            match state
                .session_service
                .acp_control_cache
                .is_live_original_request(&frame.session_id, frame.seq, &frame.event)
                .await
            {
                Ok(live) => live,
                Err(error) => {
                    tracing::warn!(target: "acp.event_listener", %error, "read Original attention request");
                    false
                }
            }
        } else {
            false
        };
        if !current
            && !frame.host_action
            && !original_terminal
            && !resolved_callback
            && !original_request
            && !resource_effect
        {
            continue;
        }

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
                    state
                        .runtime
                        .work
                        .spawn("server.acp_event_effect", async move {
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

        // Approval pushes bypass active-session suppression.
        if let crate::acp::state::Event::ApprovalRequested { approval } = frame.event.as_ref() {
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let approval_title = approval.tool_call.name.clone();
            let destructive = approval.destructive;
            let seq = frame.seq;
            state
                .runtime
                .work
                .spawn("server.acp_event_effect", async move {
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

        // Retract resolved approvals on every device.
        if let crate::acp::state::Event::ApprovalResolved { decision, .. } = frame.event.as_ref() {
            record_approval_decision(&state, *decision);
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let seq = frame.seq;
            state
                .runtime
                .work
                .spawn("server.acp_event_effect", async move {
                    acp_ws::trigger_approval_clear_push(&state_for_push, &session_id, seq).await;
                });
        }

        // Questions use the same push policy as approvals.
        if let crate::acp::state::Event::ElicitationRequested { elicitation } = frame.event.as_ref()
        {
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let question = elicitation.message.clone();
            let seq = frame.seq;
            state
                .runtime
                .work
                .spawn("server.acp_event_effect", async move {
                    acp_ws::trigger_question_push(&state_for_push, &session_id, &question, seq)
                        .await;
                });
        }

        // Retract answered questions.
        if matches!(
            frame.event.as_ref(),
            crate::acp::state::Event::ElicitationResolved { .. }
        ) {
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let seq = frame.seq;
            state
                .runtime
                .work
                .spawn("server.acp_event_effect", async move {
                    acp_ws::trigger_question_clear_push(&state_for_push, &session_id, seq).await;
                });
        }
        if !current && !frame.host_action && !original_terminal && !resource_effect {
            continue;
        }

        // Retain advertised options for settings without a live session.
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
                    let recorded = tokio::task::spawn_blocking(move || {
                        if let Err(e) = crate::acp::option_catalog::record(&agent, &options, now) {
                            tracing::warn!(
                                target: "acp.event_listener",
                                agent = %agent,
                                error = %e,
                                "failed to record acp option catalog"
                            );
                        }
                    })
                    .await;
                    if let Err(error) = recorded {
                        tracing::warn!(target: "acp.event_listener", %error, "option catalog task failed");
                    }
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
                state
                    .runtime
                    .work
                    .spawn("server.acp_event_effect", async move {
                        let result = crate::session::smart_rename::try_smart_rename(
                            state_for_rename,
                            session_id.clone(),
                            crate::session::smart_rename::SmartRenameInput {
                                first_user_prompt,
                                context,
                            },
                            false,
                        )
                        .await;
                        log_smart_rename_outcome(&session_id, &result);
                    });
            } else {
                // The first prompt must be durable before its turn completes.
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
            state
                .runtime
                .work
                .spawn("server.acp_event_effect", async move {
                    crate::session::conversation_summary::try_conversation_summary(
                        state_for_summary,
                        session_id,
                        crate::session::conversation_summary::SummaryTrigger::Auto,
                    )
                    .await;
                });
        }

        // The sink folds before broadcasting, so a frame published to a warm
        // cache carries its own status and no later reader depends on how far
        // the fold has run. A frame published cold (right after a restart with
        // a reattached worker) carries none and is rebuilt from the log.
        // Failed persistence is a history-only notice; qualified failed native
        // observations remain private in the cache's failure lane.
        let native_subject = frame
            .native_subject
            .or_else(|| frame.event.native_subject());
        let status_policy = if !current && !original_terminal && !resource_effect {
            None
        } else {
            match native_subject.map(|subject| subject.kind) {
                Some(
                    crate::acp::state::NativeSubjectEvent::ChannelUnavailable(_)
                    | crate::acp::state::NativeSubjectEvent::AdmissionRefusal(_),
                ) => Some(StatusPolicy::Authoritative),
                Some(crate::acp::state::NativeSubjectEvent::ChannelReady) => {
                    Some(StatusPolicy::Heal)
                }
                _ => status_policy(frame.event.as_ref()),
            }
        };
        let status_update = match (status_policy, frame.event.as_ref()) {
            (Some(policy), _) if native_subject.is_some() => {
                let Ok(mut fold) = state
                    .session_service
                    .fold_control_state(&frame.session_id)
                    .await
                else {
                    continue;
                };
                if fold.last_seq < frame.seq {
                    if let Err(error) =
                        fold.apply_scoped_event(frame.seq, &frame.event, &frame.view_metadata())
                    {
                        tracing::warn!(target: "acp.event_listener", session = %frame.session_id, %error, "control fold failed");
                        continue;
                    }
                }
                Some((policy, fold.sidebar_status()))
            }
            (Some(policy), crate::acp::state::Event::AgentStartupError { .. }) => {
                Some((policy, Status::Error))
            }
            (Some(policy), _) => match frame.sidebar_status {
                Some(projected) => Some((policy, projected)),
                None => {
                    // Hydrates once; frames published after it carry a status.
                    let Ok(fold) = state
                        .session_service
                        .fold_control_state(&frame.session_id)
                        .await
                    else {
                        continue;
                    };
                    if fold.last_seq == frame.seq {
                        Some(fold.sidebar_status())
                    } else {
                        state
                            .session_service
                            .sidebar_status_at(&frame.session_id, frame.seq)
                            .await
                    }
                    .map(|projected| (policy, projected))
                }
            },
            (None, _) => None,
        };
        let change = current
            .then(|| derive_acp_session_change(frame.event.as_ref()))
            .flatten();
        let acp_change = match change {
            Some(change)
                if native_subject.is_some() || matches!(&change, AcpSessionChange::Assigned(_)) =>
            {
                persist_identity_observation(
                    &state,
                    &frame,
                    change,
                    status_update.map(|(_, status)| status),
                )
                .await;
                continue;
            }
            change => change,
        };
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
        if status_update.is_none() && acp_change.is_none() && load_session_capability.is_none() {
            continue;
        }
        let resource_subject = resource_effect
            .then_some(frame.native_origin)
            .flatten()
            .map(|origin| crate::acp::state::NativeEventSubject {
                context: origin.context,
                owner: origin.owner,
                kind: crate::acp::state::NativeSubjectEvent::RuntimeObservation,
            });
        if let Some(subject) = native_subject.or(resource_subject) {
            persist_native_runtime_observation(
                &state,
                &frame,
                subject,
                status_update,
                load_session_capability,
            )
            .await;
            continue;
        }
        // Identity events share the per-session mutation lock with pollers and
        // lifecycle callers. Hold it through the durable write so a concurrent
        // SID publication cannot be overwritten by a stale ACP observation.
        let identity_lock = if acp_change.is_some() {
            Some(state.instance_lock(&frame.session_id).await)
        } else {
            None
        };
        let _identity_guard = match &identity_lock {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };

        let mut canonical_changed = false;
        // Acquire `instances` once for both branches. Releases before
        // the (potentially blocking) sessions.json save.
        let (profile_to_save, unread_profile) = loop {
            let mut instances = state.instances.write().await;
            let Some(inst) = instances.iter_mut().find(|i| i.id == frame.session_id) else {
                continue 'frames;
            };
            if !inst.is_structured() {
                continue 'frames;
            }
            // Legacy unscoped capability observations still need the exact
            // current lease at mutation. Never await `workers` under `instances`,
            // or block on lifecycle while a native publisher owns its fence.
            if let Some((capable, generation)) = load_session_capability {
                match state.acp_supervisor.commit_native_observation(
                    &frame.session_id,
                    Some(generation),
                    || {
                        let changed = inst.acp_load_session_capable != Some(capable);
                        inst.acp_load_session_capable = Some(capable);
                        Ok(changed)
                    },
                ) {
                    Ok(changed) => canonical_changed |= changed,
                    Err(error) if error.is::<crate::acp::supervisor::NativeObservationBusy>() => {
                        drop(instances);
                        tokio::task::yield_now().await;
                        continue;
                    }
                    Err(_) => continue 'frames,
                }
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
            if let Some((policy, projected)) = status_update {
                apply_status_intent(inst, policy, projected, &state.status_tx);
            }
            canonical_changed |= old_status != inst.status;
            let unread_profile =
                should_mark_acp_unread(inst, old_status, crate::session::unread_enabled())
                    .then(|| inst.source_profile.clone());
            let profile_to_save =
                apply_acp_session_change(inst, &frame.session_id, acp_change.as_ref(), false);
            canonical_changed |= profile_to_save.is_some();
            break (profile_to_save, unread_profile);
        };
        if canonical_changed {
            state
                .mutation_epoch
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        // Identity and turn-end are disjoint ACP events. If a future event ever
        // combines them, release the identity guard before the unread helper takes it.
        if unread_profile.is_some() {
            drop(_identity_guard);
        }

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
            let profile_for_save = profile;
            let acp_change_for_save = acp_change;
            let file_watch = state.file_watch.clone();
            let save_result = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
                let storage = crate::session::Storage::new(&profile_for_save, file_watch)?;
                storage.update(|all, _groups| {
                    let found = all.iter().any(|i| i.id == session_id_for_save);
                    if let Some(inst) = all.iter_mut().find(|i| i.id == session_id_for_save) {
                        apply_acp_session_change(
                            inst,
                            &session_id_for_save,
                            acp_change_for_save.as_ref(),
                            false,
                        );
                    }
                    Ok(found)
                })
            })
            .await;
            match save_result {
                Ok(Ok(_)) => {}
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
        if canonical_changed {
            state.runtime.request_publish();
        }
    }
}

async fn persist_native_runtime_observation(
    state: &Arc<AppState>,
    frame: &crate::acp::protocol::AcpBroadcastFrame,
    subject: crate::acp::state::NativeEventSubject,
    status_update: Option<(StatusPolicy, Status)>,
    load_session_capability: Option<(bool, u64)>,
) {
    let _namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(&frame.session_id).await;
    let _identity = lock.lock().await;
    let identity = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == frame.session_id && row.is_structured())
        .map(|row| {
            (
                row.source_profile.clone(),
                row.created_at,
                row.lifecycle_generation,
            )
        });
    let Some((profile, created_at, generation)) = identity else {
        return;
    };
    let owner = Arc::clone(state);
    let id = frame.session_id.clone();
    let epoch = frame.worker_generation;
    let historical_origin = (frame.scope == crate::acp::protocol::AcpEventScope::HistoryOnly
        && frame
            .operation
            .is_some_and(|operation| operation.outcome.is_some())
        && matches!(
            frame.event.as_ref(),
            Event::Stopped { .. } | Event::PromptRuntimeError { .. }
        ))
    .then_some(frame.native_origin)
    .flatten();
    let resource_origin = (frame.resource_ref.is_some()
        && status_policy(frame.event.as_ref()) == Some(StatusPolicy::Background))
    .then_some(frame.native_origin)
    .flatten();
    let denial = frame.availability_denial;
    let saved = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let storage = crate::session::Storage::open_reference(&profile)?;
        anyhow::ensure!(
            historical_origin.is_some()
                || crate::acp::state::NativeEventContext::capture_identity(
                    &id, created_at, generation, &storage,
                )? == subject.context,
            crate::session::LifecycleReservationError::Superseded
        );
        let fence = crate::session::NativeInputFence::capture_route(
            storage.profile(),
            &id,
            created_at,
            generation,
            false,
            Some(storage.bound_profile_identity()?),
        )?;
        let applicable = |row: &Instance, resource_owner_guarded: bool| -> anyhow::Result<bool> {
            let identity_matches = match historical_origin {
                Some(origin) => origin.same_physical_row(row, &storage)?,
                None => {
                    crate::acp::state::NativeEventContext::capture(row, &storage)?
                        == subject.context
                }
            };
            let owner_matches = if resource_origin.is_some() {
                resource_owner_guarded
            } else if denial {
                subject.unavailability_still_applies(&id)
            } else if historical_origin.is_some() {
                subject.owner_not_replaced(&id)
            } else {
                subject.owner_is_current(&id)
            };
            let attempt_matches = resource_origin.is_none_or(|origin| {
                origin.context == subject.context
                    && origin.owner == subject.owner
                    && row.acp_establishment
                        == Some(crate::acp::establishment::AcpEstablishment::Applied {
                            attempt_id: origin.attempt_id,
                        })
            });
            Ok(identity_matches && owner_matches && attempt_matches)
        };
        loop {
            let native_guard = if let Some(origin) = resource_origin {
                fence.lock_observation_matching(|row| {
                    row.acp_establishment
                        == Some(crate::acp::establishment::AcpEstablishment::Applied {
                            attempt_id: origin.attempt_id,
                        })
                })?
            } else {
                fence.lock_observation()?
            };
            let resource_guard = if let Some(origin) = resource_origin {
                let Some(registry) = crate::process::worker_registry::try_registry_lock(&id)?
                else {
                    drop(native_guard);
                    std::thread::yield_now();
                    continue;
                };
                let record = registry.record()?.ok_or(crate::session::SessionGone)?;
                let physical = origin
                    .owner
                    .filter(|physical| physical.birth_fingerprint.is_some())
                    .ok_or(crate::session::LifecycleReservationError::Superseded)?;
                anyhow::ensure!(
                    registry.stop()?.is_none()
                        && physical.owns_record(&record)
                        && record.source_profile.as_deref() == Some(storage.profile())
                        && crate::process::process_birth_identity(physical.pid)?.is_some_and(
                            |birth| Some(birth.fingerprint()) == physical.birth_fingerprint
                        ),
                    crate::session::LifecycleReservationError::Superseded
                );
                Some(registry)
            } else {
                None
            };
            let publication = owner.publication.blocking_write();
            let mut rows = owner.instances.blocking_write();
            let applied = owner
                .acp_supervisor
                .commit_native_observation(&id, epoch, || {
                    let row = rows
                        .iter_mut()
                        .find(|row| row.id == id)
                        .ok_or(crate::session::SessionGone)?;
                    anyhow::ensure!(
                        row.is_structured() && applicable(row, resource_guard.is_some())?,
                        crate::session::LifecycleReservationError::Superseded
                    );
                    let old = row.status;
                    if let Some((policy, projected)) = status_update {
                        apply_status_intent(row, policy, projected, &owner.status_tx);
                    }
                    let mut changed = row.status != old;
                    if let Some((capable, issued)) = load_session_capability {
                        if epoch == Some(issued) && row.acp_load_session_capable != Some(capable) {
                            row.acp_load_session_capable = Some(capable);
                            changed = true;
                        }
                    }
                    Ok((
                        changed,
                        should_mark_acp_unread(row, old, crate::session::unread_enabled()),
                    ))
                });
            drop(rows);
            drop(publication);
            let (changed, unread) = match applied {
                Err(error) if error.is::<crate::acp::supervisor::NativeObservationBusy>() => {
                    drop(resource_guard);
                    drop(native_guard);
                    std::thread::yield_now();
                    continue;
                }
                result => result?,
            };
            if changed {
                owner
                    .mutation_epoch
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                owner.runtime.request_publish();
            }
            if unread {
                storage.update(|rows, _| {
                    owner
                        .acp_supervisor
                        .commit_native_observation(&id, epoch, || {
                            let row = rows
                                .iter_mut()
                                .find(|row| row.id == id)
                                .ok_or(crate::session::SessionGone)?;
                            anyhow::ensure!(
                                applicable(row, resource_guard.is_some())?,
                                crate::session::LifecycleReservationError::Superseded
                            );
                            row.mark_unread();
                            Ok(())
                        })
                })?;
                let _publication = owner.publication.blocking_write();
                let mut rows = owner.instances.blocking_write();
                owner
                    .acp_supervisor
                    .commit_native_observation(&id, epoch, || {
                        if let Some(row) = rows.iter_mut().find(|row| row.id == id) {
                            anyhow::ensure!(
                                applicable(row, resource_guard.is_some())?,
                                crate::session::LifecycleReservationError::Superseded
                            );
                            row.mark_unread();
                        }
                        Ok(())
                    })?;
                owner.runtime.request_publish();
            }
            return Ok(());
        }
    })
    .await;
    match saved {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.is::<crate::session::LifecycleReservationError>() => {}
        Ok(Err(error)) => tracing::warn!(target: "acp.event_listener",
            session = %frame.session_id, %error, "native observation was not applied"),
        Err(error) => tracing::warn!(target: "acp.event_listener",
            session = %frame.session_id, %error, "native observation task failed"),
    }
}

/// Native controllers own this checkpoint; the actor supplies only its SDK receipt.
pub(crate) struct BindingRetirement {
    pub target: crate::acp::supervisor::RetirementTarget,
    pub checkpoint: Option<crate::acp::supervisor::NativeBindingCheckpoint>,
    supervisor:
        std::sync::Weak<crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>>,
}

impl BindingRetirement {
    pub(crate) fn captured(
        state: &AppState,
        mut target: crate::acp::supervisor::RetirementTarget,
        checkpoint: Option<crate::acp::supervisor::NativeBindingCheckpoint>,
    ) -> Self {
        target.journalled |= target.lease.is_some();
        Self {
            target,
            checkpoint,
            supervisor: Arc::downgrade(&state.acp_supervisor),
        }
    }

    pub(crate) fn prepare(
        state: &AppState,
        row: &Instance,
        storage: &crate::session::Storage,
    ) -> anyhow::Result<Self> {
        let target = state.acp_supervisor.capture_retirement(&row.id)?;
        Self::prepare_captured(state, target, row, storage)
    }

    pub(crate) fn prepare_captured(
        state: &AppState,
        target: crate::acp::supervisor::RetirementTarget,
        row: &Instance,
        storage: &crate::session::Storage,
    ) -> anyhow::Result<Self> {
        let checkpoint = if let Some(saved) = target
            .lease
            .as_ref()
            .and_then(|lease| lease.retirement().binding_checkpoint())
        {
            (crate::acp::state::NativeEventContext::capture(row, storage)?
                == saved.destination.context
                && saved
                    .destination
                    .choice
                    .as_ref()
                    .is_some_and(|choice| choice.matches(row)))
            .then_some(saved)
        } else {
            target.prepare_binding_checkpoint(row, storage)?
        };
        Ok(Self::captured(state, target, checkpoint))
    }

    pub(crate) fn promote(
        &mut self,
        row: &Instance,
        storage: &crate::session::Storage,
    ) -> anyhow::Result<()> {
        if let Some(checkpoint) = &mut self.checkpoint {
            checkpoint.destination = checkpoint.destination.retarget(row, storage)?;
        }
        Ok(())
    }

    pub(crate) fn activate(&self) {
        if let Some(lease) = &self.target.lease {
            lease.retirement().delegate_binding_checkpoint(true);
        }
        if let Some(checkpoint) = &self.checkpoint {
            self.target.record_binding_checkpoint(checkpoint.clone());
        }
    }

    /// Physical/current-producer completion, not proof that every SDK result
    /// reached an installed worker's drain before its existing retirement abort.
    fn ensure_settled(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.target.lease.is_none()
                || self.supervisor.upgrade().is_some_and(|supervisor| {
                    supervisor.binding_producer_is_current(&self.target)
                }),
            crate::session::LifecycleReservationError::Superseded
        );
        Ok(())
    }

    fn has_captured_binding(&self) -> bool {
        self.target
            .lease
            .as_ref()
            .and_then(|lease| lease.retirement().establishment_projection())
            .is_some_and(|projection| {
                projection.retirement_checkpoint_pending() || projection.has_unsettled_completion()
            })
    }

    /// Caller holds native lifecycle exclusion after original retirement.
    /// Preserve captured receipts and the deferred original-receiver drain;
    /// lossless ingestion of installed SDK results is a separate requirement.
    pub(crate) fn commit_under_native_lock(
        &self,
        native: &dyn crate::session::SessionStore,
    ) -> anyhow::Result<()> {
        self.ensure_settled()?;
        if let Some(projection) = self
            .target
            .lease
            .as_ref()
            .and_then(|lease| lease.retirement().establishment_projection())
        {
            if !projection.retirement_checkpoint_pending() {
                return Ok(());
            }
            let checkpoint = self
                .checkpoint
                .as_ref()
                .ok_or(crate::acp::establishment::EstablishmentRefusal::Unconfirmed)?;
            let supervisor = self
                .supervisor
                .upgrade()
                .ok_or(crate::acp::establishment::EstablishmentRefusal::Superseded)?;
            if let Some(destination) =
                supervisor.checkpoint_retired_establishment(native, &self.target, checkpoint)?
            {
                let mut saved = checkpoint.clone();
                saved.destination = destination;
                self.target.record_binding_checkpoint(saved);
            }
            return Ok(());
        }
        Ok(())
    }
}

pub(crate) fn commit_retired_binding(
    native: &dyn crate::session::SessionStore,
    id: &str,
    retirement: &BindingRetirement,
) -> anyhow::Result<()> {
    let _lifecycle = native.storage().acquire_instance_lifecycle_lock(id)?;
    retirement.commit_under_native_lock(native)
}

/// Caller retains the captured target, namespace and operation exclusion, never a native flock.
pub(crate) async fn retire_native_worker(
    state: &Arc<AppState>,
    id: &str,
    target: crate::acp::supervisor::RetirementTarget,
    deadline: std::time::Duration,
) -> anyhow::Result<()> {
    use crate::session::SessionStore as _;
    let retirement = BindingRetirement::captured(state, target, None);
    retirement.activate();
    let captured = state.clone();
    let id_owned = id.to_owned();
    let target = retirement.target.clone();
    let prepared = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let profile = captured
            .instances
            .blocking_read()
            .iter()
            .find(|row| row.id == id_owned)
            .map(|row| row.source_profile.clone())
            .ok_or(crate::session::SessionGone)?;
        let native = crate::server::session_store::NativeSessionStore::open(
            captured.clone(),
            &profile,
            None,
        )?;
        let lifecycle = native
            .storage()
            .acquire_instance_lifecycle_lock(&id_owned)?;
        let row = native
            .load()?
            .into_iter()
            .find(|row| row.id == id_owned)
            .ok_or(crate::session::SessionGone)?;
        let binding =
            BindingRetirement::prepare_captured(&captured, target, &row, native.storage())?;
        binding.activate();
        drop(lifecycle);
        Ok((native, binding))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    // Failed checkpoints retain debt but cannot leave the captured actor running.
    state
        .acp_supervisor
        .retire_captured_for_resume_and_wait(id, &retirement.target, deadline)
        .await?;
    retirement.ensure_settled()?;
    if !retirement.has_captured_binding() {
        // No captured binding obligation remains after owned retirement.
        // This does not establish lossless ingestion of installed SDK results.
        state
            .acp_supervisor
            .acknowledge_retirement(&retirement.target)?;
        return Ok(());
    }
    let (native, binding) = prepared?;
    let captured = state.clone();
    let id_owned = id.to_owned();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let _lifecycle = native
            .storage()
            .acquire_instance_lifecycle_lock(&id_owned)?;
        binding.commit_under_native_lock(&native)?;
        captured
            .acp_supervisor
            .acknowledge_retirement(&binding.target)?;
        Ok(())
    })
    .await?
}

pub(crate) async fn commit_background_bindings(state: &Arc<AppState>) {
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return;
    }
    for (id, target) in state.acp_supervisor.retired_binding_targets() {
        let _namespace = state.profile_namespace.read().await;
        let instance = state.instance_lock(&id).await;
        let _instance = instance.lock().await;
        let owner = state.clone();
        let captured = target.clone();
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            use crate::session::SessionStore as _;
            let Some(row) = owner
                .instances
                .blocking_read()
                .iter()
                .find(|row| row.id == id)
                .cloned()
            else {
                // Only its explicit deletion journal may acknowledge an absent row.
                return Ok(());
            };
            if row.terminal_handoff.is_some() {
                return Ok(());
            }
            let native = crate::server::session_store::NativeSessionStore::open(
                owner.clone(),
                &row.source_profile,
                None,
            )?;
            let _lifecycle = native.storage().acquire_instance_lifecycle_lock(&id)?;
            if captured
                .lease
                .as_ref()
                .is_none_or(|lease| !lease.retirement().binding_checkpoint_delegated())
            {
                return Ok(());
            }
            let Some(current) = native.load()?.into_iter().find(|row| row.id == id) else {
                return Ok(());
            };
            if current.terminal_handoff.is_some() {
                return Ok(());
            }
            let checkpoint = captured
                .lease
                .as_ref()
                .and_then(|lease| lease.retirement().binding_checkpoint());
            let retirement = if let Some(checkpoint) = checkpoint {
                BindingRetirement::captured(&owner, captured, Some(checkpoint))
            } else {
                BindingRetirement::prepare_captured(&owner, captured, &current, native.storage())?
            };
            let committed = retirement.commit_under_native_lock(&native);
            if let Err(error) = committed {
                if !matches!(
                    error.downcast_ref::<crate::session::LifecycleReservationError>(),
                    Some(crate::session::LifecycleReservationError::Superseded)
                ) {
                    return Err(error);
                }
            }
            // The native exclusion serializes this ACK with a journal's claim.
            if retirement
                .target
                .lease
                .as_ref()
                .is_some_and(|lease| lease.retirement().binding_checkpoint_delegated())
            {
                owner
                    .acp_supervisor
                    .acknowledge_retirement(&retirement.target)?;
            }
            Ok(())
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            tracing::warn!(target: "acp.reconciler", ?result, "native binding checkpoint remains unconfirmed");
        }
    }
}

async fn persist_identity_observation(
    state: &Arc<AppState>,
    frame: &crate::acp::protocol::AcpBroadcastFrame,
    change: AcpSessionChange,
    projected_status: Option<Status>,
) {
    use crate::session::SessionStore;

    let _namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(&frame.session_id).await;
    let _identity = lock.lock().await;
    let (profile, created_at, generation, conversation, previous_id, cached_status) = {
        let rows = state.instances.read().await;
        let Some(row) = rows
            .iter()
            .find(|row| row.id == frame.session_id && row.is_structured())
        else {
            return;
        };
        (
            row.source_profile.clone(),
            row.created_at,
            row.lifecycle_generation,
            row.conversation_state(),
            row.acp_session_id.clone(),
            row.status,
        )
    };
    // Capture before taking a native lock; producers acquire lifecycle first.
    let admission = state
        .acp_supervisor
        .current_lease(&frame.session_id)
        .filter(|lease| frame.worker_generation == Some(lease.epoch()))
        .and_then(|lease| lease.admission())
        .filter(|admission| {
            admission.generation == generation && admission.store.storage().profile() == profile
        });
    let owner = Arc::clone(state);
    let id = frame.session_id.clone();
    let epoch = frame.worker_generation;
    let subject = frame
        .native_subject
        .or_else(|| frame.event.native_subject());
    let ready_subject = subject
        .is_some_and(|subject| subject.kind == crate::acp::state::NativeSubjectEvent::ChannelReady);
    let current_ready = if ready_subject {
        state
            .acp_supervisor
            .current_native_availability(&id)
            .await
            .filter(|current| {
                Some(current.lease.epoch()) == epoch
                    && subject.is_some_and(|subject| {
                        current.scope.context == subject.context && current.owner == subject.owner
                    })
            })
    } else {
        None
    };
    let Some(postimage) = current_ready
        .as_ref()
        .and_then(|current| current.client.establishment_projection())
        .and_then(|projection| projection.applied())
    else {
        return;
    };
    let saved = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let native = crate::server::session_store::NativeSessionStore::open(
            Arc::clone(&owner), &profile, None,
        )?;
        let context = crate::acp::state::NativeEventContext::capture_identity(
            &id, created_at, generation, native.storage(),
        )?;
        anyhow::ensure!(
            subject.is_none_or(|subject| subject.context == context),
            crate::session::LifecycleReservationError::Superseded
        );
        let admitted_scope = if let Some(admission) = admission {
            let admitted = admission.store.storage();
            admitted.verify_bound_profile()?;
            anyhow::ensure!(
                admitted.bound_profile_identity()? == native.storage().bound_profile_identity()?,
                crate::session::LifecycleReservationError::Superseded
            );
            true
        } else {
            false
        };
        let repair_status = loop {
            let guard = native.storage().acquire_instance_lifecycle_lock(&id)?;
            let mut repair_status = None;
            let result = native.commit(&mut |rows, _| {
                owner.acp_supervisor.commit_native_observation(&id, epoch, || {
                    let row = rows.iter_mut().find(|row| row.id == id)
                        .ok_or(crate::session::SessionGone)?;
                    anyhow::ensure!(
                        row.created_at == created_at
                            && row.lifecycle_generation == generation
                            && conversation.matches(row)
                            && postimage.matches(row, native.storage())?
                            && row.acp_session_id == previous_id
                            && row.is_structured()
                            && row.terminal_handoff.as_ref().is_none_or(|intent| {
                                intent.stage == crate::session::TerminalHandoffStage::RetirementPending
                            })
                            && crate::acp::state::NativeEventContext::capture(row, native.storage())?
                                == context
                            && subject.is_none_or(|subject| subject.owner_is_current(&id)),
                        crate::session::LifecycleReservationError::Superseded
                    );
                    let correlated = matches!(&change, AcpSessionChange::Assigned(_))
                        && admitted_scope && row.ensure_startable().is_ok()
                        && row.terminal_handoff.is_none() && row.launch_is_finalized();
                    if correlated && !row.is_idle_dormant() {
                        if ready_subject {
                            if matches!(cached_status, Status::Error | Status::Stopped)
                                && current_ready.as_ref().is_some_and(|current| {
                                    current.scope.choice.as_ref().is_some_and(|choice| choice.matches(row))
                                        && current.client.connection_ready()
                                        && current.client.connection_unavailable().is_none()
                                })
                            {
                                repair_status = projected_status;
                            }
                        } else if cached_status == Status::Stopped
                            && matches!(row.status,
                                Status::Starting | Status::Running | Status::Idle | Status::Waiting)
                        {
                            repair_status = Some(row.status);
                        }
                    }
                    Ok(())
                })
            });
            drop(guard);
            match result {
                Err(error) if error.is::<crate::acp::supervisor::NativeObservationBusy>() => {
                    std::thread::yield_now();
                }
                result => { result?; break repair_status; }
            }
        };
        if let Some(status) = repair_status {
            let fence = crate::session::NativeInputFence::capture_route(
                native.storage().profile(), &id, created_at, generation, false,
                Some(native.storage().bound_profile_identity()?),
            )?;
            loop {
                let guard = fence.lock_observation()?;
                let publication = owner.publication.blocking_write();
                let mut rows = owner.instances.blocking_write();
                let applied = owner.acp_supervisor.commit_native_observation(&id, epoch, || {
                    if let Some(row) = rows.iter_mut().find(|row| {
                        row.id == id && !row.is_idle_dormant()
                            && (row.status == Status::Stopped || (ready_subject && row.status == Status::Error))
                    }) {
                        anyhow::ensure!(
                            crate::acp::state::NativeEventContext::capture(row, native.storage())?
                                == context
                                && subject.is_none_or(|subject| subject.owner_is_current(&id)),
                            crate::session::LifecycleReservationError::Superseded
                        );
                        if ready_subject && !current_ready.as_ref().is_some_and(|current| {
                            current.scope.choice.as_ref().is_some_and(|choice| choice.matches(row))
                                && current.client.connection_ready()
                                && current.client.connection_unavailable().is_none()
                        }) {
                            return Ok(());
                        }
                        let old = row.status;
                        apply_status_intent(row, StatusPolicy::Heal, status, &owner.status_tx);
                        if row.status != old {
                            owner.mutation_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            owner.runtime.request_publish();
                        }
                    }
                    Ok(())
                });
                drop(rows);
                drop(publication);
                drop(guard);
                match applied {
                    Err(error) if error.is::<crate::acp::supervisor::NativeObservationBusy>() =>
                        std::thread::yield_now(),
                    result => { result?; break; }
                }
            }
        }
        Ok(())
    }).await;
    match saved {
        Ok(Ok(())) => {}
        Ok(Err(error)) if error.is::<crate::session::LifecycleReservationError>() => {}
        Ok(Err(error)) => tracing::warn!(target: "acp.event_listener",
            session = %frame.session_id, %error, "ACP identity observation was not applied"),
        Err(error) => tracing::warn!(target: "acp.event_listener",
            session = %frame.session_id, %error, "ACP identity observation task failed"),
    }
}

/// Tally a resolved approval for the opt-in telemetry snapshot.
///
/// Counted here rather than at the HTTP endpoint because only the
/// permission handler knows which option the user's answer resolved to:
/// an answered option list posts an allow-shaped decision whatever the
/// option means, and a stale option id cancels instead of resolving.
/// This event carries the decision that actually reached the agent.
///
/// `Cancelled` is not a user decision (the daemon-restart sweep and the
/// stale-option path both emit it), so it counts as nothing; it is
/// matched explicitly so a new variant is a compile error here.
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
    for id in acp_ids {
        let Some(mut observation) = folded_sidebar_observation(&state.session_service, &id).await
        else {
            continue;
        };
        let namespace = Arc::clone(&state.profile_namespace);
        let _namespace = namespace.read().await;
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            observation.try_apply(&id, inst, |inst, _, observation| {
                apply_status_seed(inst, observation.status, &state.status_tx);
                Ok(())
            });
        }
    }
}

/// A semantic sidebar observation, never the default Idle of a history-only fold.
pub(crate) async fn folded_sidebar_status(
    service: &crate::server::session_service::SessionService,
    id: &str,
) -> Option<Status> {
    let mut observation = folded_sidebar_observation(service, id).await?;
    let namespace = Arc::clone(&observation.owner.profile_namespace);
    let _namespace = namespace.read().await;
    let mut rows = service.instances.write().await;
    let row = rows.iter_mut().find(|row| row.id == id)?;
    observation.try_apply(id, row, |_, _, observation| Ok(observation.status))
}

struct SidebarObservation {
    owner: Arc<AppState>,
    origin: crate::acp::state::NativeEventOrigin,
    current: crate::acp::state::CurrentNativeAvailability,
    postimage: Arc<crate::acp::establishment::EstablishmentPostimage>,
    frontier: u64,
    status: Status,
    ready: bool,
    unavailable: Option<crate::acp::state::AcpConnectionUnavailability>,
}

impl SidebarObservation {
    fn matches_row(
        &self,
        row: &Instance,
        storage: &crate::session::Storage,
    ) -> anyhow::Result<bool> {
        Ok(row.is_structured()
            && row.source_profile == storage.profile()
            && row.terminal_handoff.is_none()
            && self.origin.context == crate::acp::state::NativeEventContext::capture(row, storage)?
            && self.postimage.matches(row, storage)?
            && self
                .current
                .scope
                .choice
                .as_ref()
                .is_some_and(|choice| choice.matches(row)))
    }

    /// Every guard lives only inside this synchronous mutation. Contention is unknown.
    fn try_apply<T>(
        &mut self,
        id: &str,
        row: &mut Instance,
        apply: impl FnOnce(&mut Instance, &crate::session::Storage, &Self) -> anyhow::Result<T>,
    ) -> Option<T> {
        let fence = Arc::clone(&self.current.scope.fence);
        let (_, result) = fence
            .lock_observation_project(
                |disk, storage| {
                    if !self.matches_row(disk, storage)? || !self.matches_row(row, storage)? {
                        return Ok(None);
                    }
                    let Some(registry) = crate::process::worker_registry::try_registry_lock(id)?
                    else {
                        return Ok(None);
                    };
                    let Some(record) = registry.record()? else {
                        return Ok(None);
                    };
                    let Some(owner) = self
                        .origin
                        .owner
                        .filter(|owner| owner.birth_fingerprint.is_some())
                    else {
                        return Ok(None);
                    };
                    if registry.stop()?.is_some()
                        || !owner.owns_record(&record)
                        || record.source_profile.as_deref() != Some(storage.profile())
                        || !crate::process::process_birth_identity(owner.pid)?.is_some_and(
                            |birth| Some(birth.fingerprint()) == owner.birth_fingerprint,
                        )
                    {
                        return Ok(None);
                    }
                    if !self.current.refresh(id, true).unwrap_or(false)
                        || self.current.ready != self.ready
                        || self.current.unavailable != self.unavailable
                    {
                        return Ok(None);
                    }
                    let Some(applied) =
                        self.owner
                            .acp_control_cache
                            .with_frontier(id, self.frontier, || {
                                self.owner.acp_supervisor.commit_native_observation(
                                    id,
                                    Some(self.current.lease.epoch()),
                                    || apply(row, storage, self),
                                )
                            })
                    else {
                        return Ok(None);
                    };
                    match applied {
                        Ok(value) => Ok(Some(value)),
                        Err(error)
                            if error.is::<crate::acp::supervisor::NativeObservationBusy>() =>
                        {
                            Ok(None)
                        }
                        Err(error) => Err(error),
                    }
                },
                true,
            )
            .ok()??;
        result
    }
}

async fn folded_sidebar_observation(
    service: &crate::server::session_service::SessionService,
    id: &str,
) -> Option<SidebarObservation> {
    let fold = service.fold_control_state(id).await.ok()?;
    if fold.sidebar_observation_seq == 0 {
        return None;
    }
    let origin = fold.sidebar_observation_origin?;
    let owner = service.native_state().ok()?;
    let current = service
        .acp_supervisor
        .current_native_availability(id)
        .await?;
    let postimage = current.client.establishment_projection()?.applied()?;
    if origin.context != current.scope.context
        || origin.owner != current.owner
        || origin.attempt_id != postimage.attempt_id
        || origin.context != postimage.context
    {
        return None;
    }
    Some(SidebarObservation {
        owner,
        origin,
        current,
        postimage,
        frontier: fold.last_seq,
        status: fold.sidebar_status(),
        ready: fold.connection_ready,
        unavailable: fold.connection_unavailable,
    })
}

pub(crate) fn apply_status_seed(
    inst: &mut Instance,
    projected: Status,
    status_tx: &broadcast::Sender<StatusChange>,
) {
    if inst.status == Status::Stopped && matches!(projected, Status::Running | Status::Waiting) {
        inst.status = Status::Idle;
    }
    apply_status_intent(inst, StatusPolicy::Authoritative, projected, status_tx);
}

/// Whether reaching `status` should release a plugin's `sessions.turn.send` pending mark:
/// either it is now genuinely counted (`Running`/`Waiting`/`Starting`/`Creating`), or it is a
/// terminal outcome for this revival attempt (`Error`/`Stopped`/`Deleting`). `Idle` and
/// `Unknown` are deliberately excluded: a `Stopped`/`Error` session's `HealError` heal passes
/// through `Idle` before `Running`, and treating that hop as settled would release the mark
/// before the session is actually counted.
fn revival_pending_clears_on(status: Status) -> bool {
    matches!(
        status,
        Status::Running
            | Status::Waiting
            | Status::Starting
            | Status::Creating
            | Status::Error
            | Status::Stopped
            | Status::Deleting
    )
}

/// Fold a projected status into an `Instance` under `policy`. Pure mutation;
/// callers hold the write lock. Sends a `StatusChange` on
/// `status_tx` so push notifications and the dashboard see the
/// transition like any tmux-driven one.
pub(crate) fn apply_status_intent(
    inst: &mut Instance,
    policy: StatusPolicy,
    target: Status,
    status_tx: &broadcast::Sender<StatusChange>,
) {
    // Genuine in-flight terminal states: never fight them.
    if inst.is_trashed() || matches!(inst.status, Status::Deleting | Status::Creating) {
        return;
    }
    match policy {
        // A Stopped session must not be woken by a trailing worker event.
        StatusPolicy::Authoritative if inst.status == Status::Stopped => return,
        StatusPolicy::Authoritative => {}
        // Background work cannot resolve main-turn Waiting, Error or Stopped.
        // Error describes a failed operation or connection, not necessarily death.
        StatusPolicy::Background => {
            if matches!(
                inst.status,
                Status::Stopped | Status::Waiting | Status::Error
            ) {
                return;
            }
        }
        // Lifecycle authority can repair a stale Error/Stopped projection.
        StatusPolicy::Heal => {
            if !matches!(inst.status, Status::Error | Status::Stopped) {
                return;
            }
        }
    }
    // A plugin's sessions.turn.send marks a resting session pending right before waking it, so
    // the active-session cap counts it before this, its first real status report, lands.
    // Checked against `target` ahead of the no-op return below (not gated by it): a redundant
    // intent that resolves to a status the row is already in must still release a stale mark.
    if revival_pending_clears_on(target) {
        inst.plugin_revival_pending = false;
    }
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
    publish_status_change(status_tx, inst, prev);
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
/// user, who is by construction present for it. Successful and parked stops
/// project `Idle`; `prompt_failed` projects `Error`, not a successful unread edge.
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

/// Serialize the durable unread mark and its mirror with explicit unread edits.
/// A missing disk row must never produce a live mark.
pub(super) async fn persist_and_mirror_unread(
    instances: &RwLock<Vec<Instance>>,
    instance_lock: &tokio::sync::Mutex<()>,
    file_watch: Arc<crate::file_watch::FileWatchService>,
    id: &str,
    profile: String,
) -> bool {
    let _guard = instance_lock.lock().await;
    let persist_id = id.to_string();
    let persisted = api::persist_session_update(
        profile.clone(),
        "acp turn-end unread",
        file_watch,
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                inst.mark_unread();
                true
            } else {
                false
            }
        },
    )
    .await;
    let Ok((marked, _, _)) = persisted else {
        return false;
    };
    if !marked {
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
/// the recovery source: it hydrates the same fold `seed_acp_statuses` reads at
/// boot.
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
    service: &crate::server::session_service::SessionService,
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
        let Some(mut observation) = folded_sidebar_observation(service, &id).await else {
            continue;
        };
        let namespace = Arc::clone(&observation.owner.profile_namespace);
        let _namespace = namespace.read().await;
        let lock = instance_lock_in(instance_locks, &id).await;
        let _mutation = lock.lock().await;
        let mut rows = instances.write().await;
        let Some(inst) = rows.iter_mut().find(|row| row.id == id) else {
            continue;
        };
        let applied = observation.try_apply(&id, inst, |inst, storage, observation| {
            let old_status = inst.status;
            apply_status_intent(
                inst,
                StatusPolicy::Authoritative,
                observation.status,
                status_tx,
            );
            let unread = should_mark_acp_unread(inst, old_status, unread_enabled);
            if unread {
                storage.update(|disk, _| {
                    let row = disk
                        .iter_mut()
                        .find(|row| row.id == id)
                        .ok_or(crate::session::SessionGone)?;
                    anyhow::ensure!(
                        observation.matches_row(row, storage)?,
                        crate::session::LifecycleReservationError::Superseded
                    );
                    row.mark_unread();
                    Ok(())
                })?;
                inst.mark_unread();
                file_watch.notify_local_change(storage.sessions_path());
            }
            if old_status != inst.status || unread {
                service
                    .mutation_epoch
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                observation.owner.runtime.request_publish();
            }
            Ok(unread)
        });
        if applied == Some(true) {
            marked += 1;
        }
    }
    marked
}

/// Fold a derived `AcpSessionChange` into an `Instance`. Returns the
/// owning profile when sessions.json needs to be re-saved (so the new
/// `acp_session_id` survives daemon restart), or `None` if the
/// change was a no-op or no change was emitted.
/// `assignment_scoped` requires native launch admission or the matching retained
/// handoff. An identity observation alone cannot complete pending work or alter a pin.
pub(super) fn apply_acp_session_change(
    inst: &mut Instance,
    session_id: &str,
    change: Option<&AcpSessionChange>,
    assignment_scoped: bool,
) -> Option<String> {
    if !inst.is_structured()
        || inst.terminal_handoff.as_ref().is_some_and(|intent| {
            intent.stage != crate::session::TerminalHandoffStage::RetirementPending
        })
    {
        return None;
    }
    let AcpSessionChange::Assigned(new_id) = change?;
    let same_acp_session = inst.acp_session_id.as_deref() == Some(new_id.as_str());
    if !assignment_scoped {
        let pin_matches = matches!(&inst.resume_intent,
                    crate::session::ResumeIntent::Use(target) if target == new_id);
        let contradicts_choice = match &inst.resume_intent {
            crate::session::ResumeIntent::Default => false,
            crate::session::ResumeIntent::Use(target) => target != new_id,
            crate::session::ResumeIntent::Cleared | crate::session::ResumeIntent::Fork { .. } => {
                true
            }
        };
        let contradicts_binding = inst.resume_binding.as_ref().is_some_and(|binding| {
            binding.provenance == crate::session::ConversationProvenance::Asserted
                && binding.session_id != *new_id
        });
        let pending_flow = inst.fork_pending.is_some() || inst.import_pending == Some(true);
        if contradicts_choice
            || contradicts_binding
            || (pending_flow && !same_acp_session && !pin_matches)
        {
            return None;
        }
    }
    let cleared_import_pending = if assignment_scoped && same_acp_session {
        inst.import_pending.take().unwrap_or(false)
    } else {
        false
    };
    if same_acp_session {
        if cleared_import_pending {
            tracing::info!(
                target: "acp.event_listener",
                session = %session_id,
                cleared_import_pending,
                "cleared import marker on ACP reassignment"
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
    if assignment_scoped && inst.fork_pending.take().is_some() {
        inst.import_pending = None;
    }
    if !assignment_scoped || !matches!(inst.resume_intent, crate::session::ResumeIntent::Default) {
        return Some(inst.source_profile.clone());
    }
    if let Some(stale) = inst.resume_binding.as_ref().filter(|binding| {
        binding.provenance == crate::session::ConversationProvenance::Asserted
            && inst.acp_session_id.as_deref() != Some(binding.session_id.as_str())
    }) {
        tracing::info!(
            target: "acp.event_listener",
            session = %session_id,
            stale_id = %stale.session_id,
            "dropping asserted conversation binding that no longer matches the ACP id"
        );
        inst.resume_binding = None;
    }
    Some(inst.source_profile.clone())
}

pub(super) fn derive_acp_session_change(event: &crate::acp::Event) -> Option<AcpSessionChange> {
    AcpSessionChange::from_event(event)
}

/// How an event's projected sidebar status may overwrite the row's current
/// one. The value itself always comes from the folded `AcpState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatusPolicy {
    Authoritative,
    /// A no-op while the sidebar sits on a status only the main turn may
    /// resolve: `Waiting`, `Stopped` or `Error`. Used only by the
    /// `Background*` arms (#4001).
    Background,
    /// Only takes effect from `Error` or `Stopped`: a fresh worker attached.
    Heal,
}

/// The events that move the sidebar status, and how.
pub(crate) fn status_policy(event: &crate::acp::Event) -> Option<StatusPolicy> {
    use crate::acp::Event;
    match event {
        Event::UserPromptSent { .. }
        | Event::ApprovalRequested { .. }
        | Event::ApprovalResolved { .. }
        | Event::ElicitationRequested { .. }
        | Event::ElicitationResolved { .. }
        | Event::ThinkingStarted
        | Event::AgentMessageChunk { .. }
        | Event::ToolCallStarted { .. }
        | Event::Stopped { .. }
        | Event::PromptRuntimeError { .. }
        | Event::AgentStartupError { .. }
        | Event::IncompatibleAgent { .. } => Some(StatusPolicy::Authoritative),
        Event::BackgroundAgentLaunched { .. }
        | Event::BackgroundTaskStarted { .. }
        | Event::BackgroundAgentProgress {
            status: crate::acp::state::BackgroundAgentStatus::Running,
            ..
        }
        | Event::BackgroundAgentCompleted { .. }
        | Event::BackgroundTaskFinished { .. } => Some(StatusPolicy::Background),
        Event::RateLimitAutoResumed { .. } => Some(StatusPolicy::Heal),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::protocol::AcpBroadcastFrame;
    use crate::server::test_support;

    /// #3741: the tally follows the decision that reached the agent, and
    /// a cancellation is not a user decision, so it counts as nothing.
    /// The endpoint cannot do this itself: an answered option list posts
    /// an allow-shaped decision whatever the option turns out to mean.
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
    /// `flush_passive_transition_defers_unread_until_persist_ok` (in
    /// `status_poll.rs`) locks for the tmux poller. Separate test rather than a
    /// row in the one above because it needs the store deliberately broken.
    #[tokio::test]
    #[serial_test::serial]
    async fn persist_and_mirror_unread_skips_the_mirror_on_a_failed_write() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());

        // A failed write must not strand a memory-only mark (#2755), the rule
        // `flush_passive_transition_defers_unread_until_persist_ok` (in
        // `status_poll.rs`) locks for the tmux poller.
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

    /// Lag recovery marks a missed actual terminal, not an already observed turn or a terminal row.
    #[tokio::test]
    #[serial_test::serial]
    async fn recover_structured_unread_after_lag_marks_only_a_missed_turn_end() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                crate::session::set_unread_enabled(true);
                let mut missed = test_support::managed_control_session(scope, "lag-missed").await;
                let missed_id = missed.id().to_owned();
                let request = missed.start_prompt("actual missed terminal").await;
                missed.finish_prompt(&request, None).await;
                let mut already =
                    test_support::managed_control_session(scope, "lag-observed").await;
                let already_id = already.id().to_owned();
                let request = already.start_prompt("already observed terminal").await;
                already.finish_prompt(&request, None).await;
                let state = missed.state.clone();
                let mut terminal = Instance::new("tmux-row", "/tmp/tmux");
                terminal.source_profile = "test".into();
                terminal.status = Status::Running;
                let terminal_id = terminal.id.clone();
                let persisted = terminal.clone();
                tokio::task::spawn_blocking(move || {
                    crate::session::Storage::open_reference("test")
                        .unwrap()
                        .update(|rows, _| {
                            rows.push(persisted);
                            Ok(())
                        })
                })
                .await
                .unwrap()
                .unwrap();
                {
                    let mut rows = state.instances.write().await;
                    rows.iter_mut()
                        .find(|row| row.id == missed_id)
                        .unwrap()
                        .status = Status::Running;
                    rows.iter_mut()
                        .find(|row| row.id == already_id)
                        .unwrap()
                        .status = Status::Idle;
                    rows.push(terminal);
                }
                state.acp_control_cache.forget(&missed_id);
                state.acp_control_cache.forget(&already_id);
                let marked = recover_structured_unread_after_lag(
                    &state.instances,
                    &state.session_service,
                    &state.instance_locks,
                    state.file_watch.clone(),
                    &state.status_tx,
                )
                .await;
                assert_eq!(
                    marked, 1,
                    "only the actual missed terminal creates an unread edge"
                );
                let rows = state.instances.read().await;
                let missed = rows.iter().find(|row| row.id == missed_id).unwrap();
                assert_eq!(missed.status, Status::Idle);
                assert!(missed.unread);
                assert!(load_profile_row("test", &missed_id).unwrap().unread);
                assert!(!rows.iter().find(|row| row.id == already_id).unwrap().unread);
                let terminal = rows.iter().find(|row| row.id == terminal_id).unwrap();
                assert_eq!(terminal.status, Status::Running);
                assert!(!terminal.unread);
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn current_channel_ready_and_initialize_metadata_do_not_prove_a_finished_turn() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                crate::session::set_unread_enabled(true);
                let live =
                    test_support::managed_control_session(scope, "ready-is-not-terminal").await;
                let state = live.state.clone();
                let id = live.id().to_owned();
                let fold = state.session_service.fold_control_state(&id).await.unwrap();
                assert!(
                    fold.connection_ready,
                    "this is a genuinely qualified current Native source"
                );
                assert_eq!(
                    fold.sidebar_observation_seq, 0,
                    "initialize and readiness are not a turn outcome"
                );
                assert_eq!(
                    folded_sidebar_status(&state.session_service, &id).await,
                    None
                );
                state
                    .instances
                    .write()
                    .await
                    .iter_mut()
                    .find(|row| row.id == id)
                    .unwrap()
                    .status = Status::Running;
                seed_acp_statuses(state.clone()).await;
                let marked = recover_structured_unread_after_lag(
                    &state.instances,
                    &state.session_service,
                    &state.instance_locks,
                    state.file_watch.clone(),
                    &state.status_tx,
                )
                .await;
                assert_eq!(marked, 0);
                let rows = state.instances.read().await;
                let row = rows.iter().find(|row| row.id == id).unwrap();
                assert_eq!(row.status, Status::Running);
                assert!(!row.unread);
                assert!(!load_profile_row("test", &id).unwrap().unread);
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn qualified_background_task_end_is_sidebar_evidence_without_a_main_turn() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                crate::session::set_unread_enabled(true);
                let mut live =
                    test_support::managed_control_session(scope, "background-only-status").await;
                let state = live.state.clone();
                let id = live.id().to_owned();
                let path = start_projection_resource(&mut live, false).await;
                let started = state.session_service.fold_control_state(&id).await.unwrap();
                assert_eq!(started.native_control_seq, 0, "no main turn was admitted");
                assert!(started.sidebar_observation_seq > 0);
                assert_eq!(started.sidebar_status(), Status::Running);
                let mut unmatched = started.clone();
                unmatched
                    .apply_scoped_event(
                        started.last_seq + 1,
                        &Event::BackgroundTaskFinished {
                            task_id: "projection-task".into(),
                            ended_at: None,
                        },
                        &crate::acp::event_sender::EventMetadata::history(),
                    )
                    .unwrap();
                assert_eq!(
                    unmatched.sidebar_observation_seq, started.sidebar_observation_seq,
                    "an unqualified end cannot become evidence for the actual allocation"
                );
                assert_eq!(unmatched.sidebar_status(), Status::Running);
                state
                    .instances
                    .write()
                    .await
                    .iter_mut()
                    .find(|row| row.id == id)
                    .unwrap()
                    .status = Status::Running;
                finish_projection_resource(&mut live, &path, false).await;
                let ended = state.session_service.fold_control_state(&id).await.unwrap();
                assert_eq!(ended.native_control_seq, 0);
                assert!(ended.sidebar_observation_seq > started.sidebar_observation_seq);
                assert_eq!(ended.sidebar_status(), Status::Idle);
                let marked = recover_structured_unread_after_lag(
                    &state.instances,
                    &state.session_service,
                    &state.instance_locks,
                    state.file_watch.clone(),
                    &state.status_tx,
                )
                .await;
                assert_eq!(marked, 1, "the exact allocated task really finished");
                assert!(load_profile_row("test", &id).unwrap().unread);
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn historical_lifecycle_events_cannot_repaint_status_or_mark_a_turn_unread() {
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::set_unread_enabled(true);
        let mut row = Instance::new("history-only status", "/tmp/history-only");
        row.view = crate::session::View::Structured;
        row.source_profile = "history-only-status".into();
        row.status = Status::Running;
        let id = row.id.clone();
        seed_profile_store(&row.source_profile, vec![row.clone()]);
        let state = test_support::build_test_app_state(vec![row]);
        for (index, event) in [
            crate::acp::state::test_support::prompt("retained prompt"),
            Event::Stopped {
                reason: "prompt_complete".into(),
            },
            Event::AgentStartupError {
                message: "retained failure".into(),
                native_subject: None,
            },
            Event::AcpSessionAssigned {
                acp_session_id: "retained identity".into(),
                connection_ready: true,
                native_subject: None,
            },
            Event::BackgroundTaskStarted {
                task_id: "historical-task".into(),
                tool_call_id: None,
                description: "retained task".into(),
                workflow: None,
            },
        ]
        .iter()
        .enumerate()
        {
            state
                .acp_event_store
                .record(
                    &id,
                    index as u64 + 1,
                    event,
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
        }
        assert_eq!(
            folded_sidebar_status(&state.session_service, &id).await,
            None
        );
        seed_acp_statuses(state.clone()).await;
        let marked = recover_structured_unread_after_lag(
            &state.instances,
            &state.session_service,
            &state.instance_locks,
            state.file_watch.clone(),
            &state.status_tx,
        )
        .await;
        assert_eq!(
            marked, 0,
            "history is not evidence that the current turn completed"
        );
        let rows = state.instances.read().await;
        let row = rows.iter().find(|row| row.id == id).unwrap();
        assert_eq!(row.status, Status::Running);
        assert!(!row.unread);
        assert!(!load_profile_row("history-only-status", &id).unwrap().unread);
    }

    async fn observed_frame(
        publications: &mut broadcast::Receiver<AcpBroadcastFrame>,
        id: &str,
        matches: impl Fn(&Event) -> bool,
    ) -> AcpBroadcastFrame {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let frame = publications.recv().await.unwrap();
                if frame.session_id == id && matches(&frame.event) {
                    return frame;
                }
            }
        })
        .await
        .expect("actual source publication arrives")
    }

    async fn start_projection_resource(
        live: &mut crate::server::test_support::ManagedControlSession,
        agent: bool,
    ) -> std::path::PathBuf {
        let actual = live.client.actual();
        let store = &actual.native_store.as_ref().unwrap().stores[0];
        let project = store.join("projects/-managed");
        std::fs::create_dir_all(project.join("subagents")).unwrap();
        let parent = project.join(format!("{}.jsonl", live.sid));
        std::fs::write(&parent, "").unwrap();
        let transcript = project.join("subagents/projection-agent.jsonl");
        std::fs::write(&transcript, "").unwrap();
        let response = if agent {
            serde_json::json!({"toolName":"Agent","toolResponse":{
                "agentId":"projection-agent","description":"keep working",
                "prompt":"continue","resolvedModel":"test-model",
                "outputFile":transcript,"status":"async_launched"
            }})
        } else {
            serde_json::json!({"toolName":"Bash","toolResponse":{"backgroundTaskId":"projection-task"}})
        };
        live.native
            .send(serde_json::json!({
                "jsonrpc":"2.0","method":"session/update","params":{"sessionId":live.sid,
                    "update":{"sessionUpdate":"tool_call_update","toolCallId":"projection-tool",
                        "_meta":{"claudeCode":response}}}
            }))
            .await;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let event = live.client.next_event().await.unwrap();
                if matches!(event, Event::BackgroundAgentLaunched { .. }) && agent
                    || matches!(event, Event::BackgroundTaskStarted { .. }) && !agent
                {
                    break;
                }
            }
        })
        .await
        .expect("actual Native resource starts");
        let history = live
            .state
            .acp_event_store
            .replay_from(live.id(), 0)
            .unwrap();
        let launch = history
            .iter()
            .find(|stored| {
                matches!(stored.event.as_ref(), Event::BackgroundAgentLaunched { .. }) && agent
                    || matches!(stored.event.as_ref(), Event::BackgroundTaskStarted { .. })
                        && !agent
            })
            .unwrap();
        assert!(
            launch.metadata.resource_witness.is_some(),
            "actual first-I/O resource custody"
        );
        if agent {
            transcript
        } else {
            parent
        }
    }

    async fn finish_projection_resource(
        live: &mut crate::server::test_support::ManagedControlSession,
        path: &std::path::Path,
        agent: bool,
    ) {
        let line = if agent {
            serde_json::json!({"type":"assistant","message":{"stop_reason":"end_turn",
                "content":[{"type":"text","text":"done"}]}})
        } else {
            serde_json::json!({"type":"user","message":{"content":
                "<task-notification>\n<task-id>projection-task</task-id>\n<tool-use-id>projection-tool</tool-use-id>\n<status>completed</status>\n</task-notification>"}})
        };
        std::fs::write(path, format!("{line}\n")).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let event = live.client.next_event().await.unwrap();
                if matches!(event, Event::BackgroundAgentCompleted { .. }) && agent
                    || matches!(event, Event::BackgroundTaskFinished { .. }) && !agent
                {
                    break;
                }
            }
        })
        .await
        .expect("actual resource transcript completes");
    }

    /// Lag recovery keeps genuinely owned background work and newer turns running.
    #[tokio::test]
    #[serial_test::serial]
    async fn recover_structured_unread_after_lag_keeps_running_for_work_past_the_stopped() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        for background in [false, true] {
            with_managed(|scope| {
                Box::pin(async move {
                    crate::session::set_unread_enabled(true);
                    let mut live =
                        test_support::managed_control_session(scope, "lag-live-work").await;
                    let id = live.id().to_owned();
                    let request = live.start_prompt("go").await;
                    if background {
                        start_projection_resource(&mut live, true).await;
                    }
                    live.finish_prompt(&request, None).await;
                    if !background {
                        live.start_prompt("newer turn").await;
                    }
                    live.state.acp_control_cache.forget(&id);
                    live.state
                        .instances
                        .write()
                        .await
                        .iter_mut()
                        .find(|row| row.id == id)
                        .unwrap()
                        .status = Status::Running;
                    let marked = recover_structured_unread_after_lag(
                        &live.state.instances,
                        &live.state.session_service,
                        &live.state.instance_locks,
                        live.state.file_watch.clone(),
                        &live.state.status_tx,
                    )
                    .await;
                    assert_eq!(marked, 0, "live work is not a turn end");
                    let rows = live.state.instances.read().await;
                    let row = rows.iter().find(|row| row.id == id).unwrap();
                    assert_eq!(row.status, Status::Running);
                    assert!(!row.unread, "unfinished work must not become unread");
                })
            })
            .await;
        }
    }

    #[tokio::test]
    async fn acp_event_listener_tracks_only_actual_current_load_session_capability() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                let state = scope.state.clone();
                let listener = start_managed_listener(scope, &state).await;
                let live = crate::server::test_support::managed_control_session(
                    scope,
                    "listener-capability",
                )
                .await;
                let id = live.id().to_owned();
                let row = await_row(
                    &state,
                    &id,
                    |row| row.acp_load_session_capable == Some(true),
                    "actual initialize capability reaches the row",
                )
                .await;
                assert_eq!(row.acp_load_session_capable, Some(true));
                let before = state.acp_event_store.highest_seq(&id);
                let history = crate::acp::Event::PromptCapabilities {
                    image: false,
                    audio: false,
                    embedded_context: false,
                    load_session: Some(false),
                    steering: false,
                };
                let metadata = crate::acp::event_sender::EventMetadata::history();
                state
                    .acp_event_store
                    .record(&id, before + 1, &history, &metadata)
                    .unwrap();
                state
                    .acp_events_tx
                    .send(AcpBroadcastFrame::from_qualified(
                        id.clone(),
                        before + 1,
                        Arc::new(history),
                        &metadata,
                        Some(live.client.lease.epoch()),
                        None,
                        None,
                    ))
                    .unwrap();
                state
                    .acp_supervisor
                    .hydrate_seqs(state.acp_event_store.all_session_seqs().unwrap());
                let request_count = live.native.requests().len();
                tokio::task::yield_now().await;
                assert_eq!(
                    state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .acp_load_session_capable,
                    Some(true)
                );
                assert_eq!(live.native.requests().len(), request_count);
                listener.abort();
                let _ = listener.await;
            })
        })
        .await;
    }
    /// The real original terminal durably marks unread and mirrors it into daemon memory.
    #[tokio::test]
    #[serial_test::serial]
    async fn acp_event_listener_marks_a_finished_turn_unread_on_disk_and_in_memory() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                crate::session::set_unread_enabled(true);
                let mut live =
                    test_support::managed_control_session(scope, "listener-turn-end").await;
                let state = live.state.clone();
                let id = live.id().to_owned();
                let listener = start_managed_listener(scope, &state).await;
                let request = live.start_prompt("finish this turn").await;
                await_row(
                    &state,
                    &id,
                    |row| row.status == Status::Running,
                    "actual prompt opens the turn",
                )
                .await;
                live.finish_prompt(&request, None).await;
                let row = await_row(
                    &state,
                    &id,
                    |row| row.unread,
                    "durable unread mark reaches memory",
                )
                .await;
                assert_eq!(row.status, Status::Idle);
                assert!(load_profile_row("test", &id).unwrap().unread);
                listener.abort();
                let _ = listener.await;
            })
        })
        .await;
    }

    /// A frame far behind the fold keeps its own transition: ApprovalResolved
    /// at N is consumed after the content frames and the stop are already
    /// folded, and must not read a later level. The status rides on the frame,
    /// so it holds even when a finite retention cap has pruned the answer out
    /// of the journal.
    #[tokio::test]
    #[serial_test::serial]
    async fn acp_event_listener_keeps_the_edge_for_a_frame_far_behind_the_fold() {
        use crate::acp::supervisor::test_support::managed::with_managed_retention;
        for (retained, content_frames) in [(2000_usize, 8_usize), (2000, 24), (8, 24)] {
            with_managed_retention(retained, |scope| Box::pin(async move {
                    crate::session::set_unread_enabled(true);
                    let mut live = crate::server::test_support::managed_control_session(scope, "listener-fold-window").await;
                    let id = live.id().to_owned();
                    let listener = start_managed_listener(scope, &live.state).await;
                    let request = live.start_prompt("go").await;
                    let locked = live.state.clone();
                    let mirror = locked.instances.write().await;
                    for index in 0..content_frames {
                        live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                            "sessionId":live.sid,"update":{"sessionUpdate":"agent_message_chunk",
                                "content":{"type":"text","text":format!("out {index}")}}
                        }})).await;
                    }
                    live.finish_prompt(&request, None).await;
                    let frontier = live.state.acp_event_store.highest_seq(&id);
                    assert!(frontier > content_frames as u64);
                    drop(mirror);
                    let row = await_row(&live.state, &id, |row| row.unread,
                        "queued genuine running-to-idle transition marks unread").await;
                    assert_eq!(row.status, Status::Idle);
                    let durable = crate::session::Storage::open_reference("test").unwrap().load().unwrap();
                    assert!(durable.iter().find(|row| row.id == id).unwrap().unread);
                    listener.abort();
                    let _ = listener.await;
                })).await;
        }
    }

    /// Replay-only diagnostics cannot overwrite current runtime status.
    #[tokio::test]
    #[serial_test::serial]
    async fn acp_event_listener_ignores_a_historical_startup_error_outside_the_log() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let mut inst = Instance::new("acp-unpersisted", "/tmp/acp");
        inst.view = crate::session::View::Structured;
        inst.status = Status::Idle;
        let id = inst.id.clone();
        let state = test_support::build_test_app_state(vec![inst]);
        let listener = tokio::spawn(acp_event_listener(state.clone()));
        await_subscribed(&state).await;
        let event = Arc::new(Event::AgentStartupError {
            message: "retained startup diagnostic".into(),
            native_subject: None,
        });
        let gate = assignment_listener_gate_for_test(&id, Some(8));
        for (seq, event) in [
            (7, event),
            (
                8,
                Arc::new(Event::AcpSessionAssigned {
                    acp_session_id: "history-only".into(),
                    connection_ready: false,
                    native_subject: None,
                }),
            ),
        ] {
            state
                .acp_events_tx
                .send(AcpBroadcastFrame::from_qualified(
                    id.clone(),
                    seq,
                    event,
                    &crate::acp::event_sender::EventMetadata::history(),
                    None,
                    None,
                    None,
                ))
                .unwrap();
        }
        gate.arrived.await.unwrap();
        assert_eq!(state.instances.read().await[0].status, Status::Idle);
        gate.release.send(()).unwrap();
        gate.applied.await.unwrap();
        assert_eq!(state.acp_event_store.highest_seq(&id), 0);
        listener.abort();
        let _ = listener.await;
    }

    /// A cold fold reuses actual Native provenance, including live resource custody.
    #[tokio::test]
    #[serial_test::serial]
    async fn acp_event_listener_rebuilds_a_frame_published_to_a_cold_cache() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        for main_running in [false, true] {
            with_managed(|scope| {
                Box::pin(async move {
                    crate::session::set_unread_enabled(true);
                    let mut live =
                        test_support::managed_control_session(scope, "listener-cold").await;
                    let state = live.state.clone();
                    let id = live.id().to_owned();
                    let mut publications = state.acp_events_tx.subscribe();
                    let request = live.start_prompt("spawn and go").await;
                    let path = start_projection_resource(&mut live, true).await;
                    state.acp_control_cache.forget(&id);
                    assert!(!state.acp_control_cache.is_hydrated(&id));
                    if main_running {
                        finish_projection_resource(&mut live, &path, true).await;
                    } else {
                        live.finish_prompt(&request, None).await;
                    }
                    let frame = observed_frame(&mut publications, &id, |event| {
                        if main_running {
                            matches!(event, Event::BackgroundAgentCompleted { .. })
                        } else {
                            matches!(event, Event::Stopped { .. })
                        }
                    })
                    .await;
                    assert!(
                        frame.sidebar_status.is_none(),
                        "the source really published to a cold cache"
                    );
                    state
                        .instances
                        .write()
                        .await
                        .iter_mut()
                        .find(|row| row.id == id)
                        .unwrap()
                        .status = Status::Idle;
                    let listener = start_managed_listener(scope, &state).await;
                    state.acp_events_tx.send(frame).unwrap();
                    let row = await_row(
                        &state,
                        &id,
                        |row| row.status == Status::Running,
                        "cold fold must preserve genuinely open work",
                    )
                    .await;
                    assert!(!row.unread);
                    assert!(state.acp_control_cache.is_hydrated(&id));
                    assert_eq!(
                        state
                            .session_service
                            .fold_control_state(&id)
                            .await
                            .unwrap()
                            .sidebar_status(),
                        Status::Running
                    );
                    listener.abort();
                    let _ = listener.await;
                })
            })
            .await;
        }
    }

    #[test]
    fn acp_session_assigned_preserves_dormancy_on_first_and_same_id() {
        for previous in [None, Some("sid-1")] {
            let mut inst = Instance::new("seed", "/tmp/seed");
            inst.view = crate::session::View::Structured;
            inst.acp_session_id = previous.map(str::to_owned);
            let dormant = chrono::Utc::now();
            inst.idle_dormant_since = Some(dormant);
            let persist = apply_acp_session_change(
                &mut inst,
                "seed",
                Some(&AcpSessionChange::Assigned("sid-1".to_string())),
                false,
            );
            assert_eq!(inst.idle_dormant_since, Some(dormant));
            assert_eq!(inst.acp_session_id.as_deref(), Some("sid-1"));
            assert_eq!(persist.is_some(), previous.is_none());
        }
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
            true,
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
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = None;
        inst.fork_pending = None;
        inst.import_pending = Some(true);

        let profile = apply_acp_session_change(
            &mut inst,
            "sess-1",
            Some(&AcpSessionChange::Assigned("some-new-id".into())),
            true,
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
            false,
        );
        assert!(
            persist.is_none(),
            "unchanged id with no stale marker is a no-op"
        );
    }

    #[test]
    fn a_new_acp_id_drops_only_a_stale_asserted_binding() {
        let binding = |provenance| crate::session::ConversationBinding {
            session_id: "old-id".into(),
            execution: None,
            provenance,
            transcript_path: None,
        };
        let new_id = AcpSessionChange::Assigned("new-id".into());
        for (provenance, pinned, kept) in [
            (
                crate::session::ConversationProvenance::Asserted,
                false,
                false,
            ),
            (
                crate::session::ConversationProvenance::Observed,
                false,
                true,
            ),
            (crate::session::ConversationProvenance::Asserted, true, true),
        ] {
            let mut inst = Instance::new("seed", "/tmp/seed");
            inst.view = crate::session::View::Structured;
            inst.acp_session_id = Some("old-id".into());
            inst.resume_binding = Some(binding(provenance));
            if pinned {
                inst.resume_intent = crate::session::ResumeIntent::Use("old-id".into());
            }
            apply_acp_session_change(&mut inst, "sess-1", Some(&new_id), true);
            assert_eq!(inst.resume_binding.is_some(), kept);
            assert_eq!(inst.acp_session_id.as_deref(), Some("new-id"));
            if pinned {
                assert_eq!(
                    inst.resume_intent,
                    crate::session::ResumeIntent::Use("old-id".into())
                );
            }
        }
    }

    #[test]
    fn identity_observation_cannot_mutate_terminal_conversation() {
        let mut inst = Instance::new("terminal", "/tmp/terminal");
        inst.view = crate::session::View::Terminal;
        inst.resume_intent = crate::session::ResumeIntent::Use("native-id".into());
        let expected = inst.conversation_state();

        assert_eq!(
            apply_acp_session_change(
                &mut inst,
                "terminal",
                Some(&AcpSessionChange::Assigned("acp-id".into())),
                false,
            ),
            None
        );
        assert!(expected.matches(&inst));
        assert_eq!(inst.acp_session_id, None);
    }

    #[test]
    fn derive_acp_session_change_extracts_assigned_id() {
        use crate::acp::Event;
        let ev = Event::AcpSessionAssigned {
            acp_session_id: "uuid-1234".into(),
            connection_ready: false,
            native_subject: None,
        };
        assert_eq!(
            derive_acp_session_change(&ev),
            Some(AcpSessionChange::Assigned("uuid-1234".into()))
        );
    }

    #[test]
    fn derive_acp_session_change_ignores_unrelated_events() {
        use crate::acp::Event;
        assert_eq!(
            derive_acp_session_change(&Event::AgentMessageChunk {
                text: "x".into(),
                message_id: None
            }),
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
    fn status_policy_classifies_the_events_that_move_the_sidebar() {
        use crate::acp::approvals::{ApprovalDecision, Nonce};
        use crate::acp::state::test_support::{chunk, prompt, stopped};
        use crate::acp::state::BackgroundAgentStatus;
        use crate::acp::Event;
        use StatusPolicy::{Authoritative, Background, Heal};
        let completed = Event::BackgroundAgentCompleted {
            agent_id: "bg-1".into(),
            status: BackgroundAgentStatus::Completed,
            tools: Vec::new(),
            result: None,
            warning: None,
            ended_at: chrono::Utc::now(),
        };
        let progress = |status| Event::BackgroundAgentProgress {
            agent_id: "bg-1".into(),
            status,
            tool_count: 0,
            tools: Vec::new(),
            last_tool: None,
            last_text: None,
            at: chrono::Utc::now(),
        };
        let cases = [
            (prompt("hi"), Some(Authoritative)),
            (chunk("x"), Some(Authoritative)),
            (Event::ThinkingStarted, Some(Authoritative)),
            (stopped("prompt_complete"), Some(Authoritative)),
            (stopped("prompt_failed"), Some(Authoritative)),
            (
                Event::PromptRuntimeError {
                    message: "request failed".into(),
                    evidence: None,
                    native_subject: None,
                },
                Some(Authoritative),
            ),
            (
                Event::AgentStartupError {
                    message: "boom".into(),
                    native_subject: None,
                },
                Some(Authoritative),
            ),
            (
                Event::ApprovalResolved {
                    nonce: Nonce("x".into()),
                    decision: ApprovalDecision::Allow,
                },
                Some(Authoritative),
            ),
            (completed, Some(Background)),
            (
                Event::BackgroundTaskFinished {
                    task_id: "t".into(),
                    ended_at: None,
                },
                Some(Background),
            ),
            (progress(BackgroundAgentStatus::Running), Some(Background)),
            (progress(BackgroundAgentStatus::Completed), None),
            (
                Event::AcpSessionAssigned {
                    acp_session_id: "a".into(),
                    connection_ready: false,
                    native_subject: None,
                },
                None,
            ),
            (
                Event::RateLimitAutoResumed {
                    resets_at: chrono::Utc::now(),
                    manual: false,
                },
                Some(Heal),
            ),
            (Event::ThinkingEnded, None),
        ];
        for (event, want) in cases {
            assert_eq!(status_policy(&event), want, "{event:?}");
        }
    }

    // --- #2248: a structured session must heal out of a stale Stopped ---

    fn stopped_structured_instance() -> Instance {
        let mut inst = Instance::new("s", "/tmp/s");
        inst.view = crate::session::View::Structured;
        inst.status = Status::Stopped;
        inst
    }

    fn apply(inst: &mut Instance, policy: StatusPolicy, target: Status) {
        let tx = broadcast::channel(8).0;
        apply_status_intent(inst, policy, target, &tx);
    }

    /// Consumers scope a change by profile after the row may be gone, so the published change
    /// carries the profile it was made under.
    #[test]
    fn apply_status_intent_publishes_the_effective_profile() {
        let mut inst = stopped_structured_instance();
        inst.source_profile = "work".to_string();
        inst.status = Status::Idle;
        let (tx, mut rx) = broadcast::channel(8);
        apply_status_intent(&mut inst, StatusPolicy::Authoritative, Status::Running, &tx);
        assert_eq!(rx.try_recv().unwrap().effective_profile, "work");
    }

    /// A `sessions.turn.send` revival marks the row pending before the wake; `HealError`'s
    /// `Idle` hop must not release that mark; only the `UserPromptSent` that actually drives
    /// it to `Running` may.
    #[test]
    fn heal_error_does_not_clear_a_pending_plugin_revival() {
        let mut inst = stopped_structured_instance();
        inst.plugin_revival_pending = true;

        apply(&mut inst, StatusPolicy::Heal, Status::Idle);
        assert_eq!(inst.status, Status::Idle);
        assert!(
            inst.plugin_revival_pending,
            "the heal's Idle hop must not release the mark early"
        );

        apply(&mut inst, StatusPolicy::Authoritative, Status::Running);
        assert_eq!(inst.status, Status::Running);
        assert!(
            !inst.plugin_revival_pending,
            "reaching a counted status must release the mark"
        );
    }

    /// A redundant intent that resolves to the row's current status is a no-op for `status`
    /// itself, but must still release a stale pending mark rather than being silently skipped
    /// by the same early return.
    #[test]
    fn a_noop_transition_still_clears_a_pending_plugin_revival() {
        let mut inst = Instance::new("s", "/tmp/s");
        inst.view = crate::session::View::Structured;
        inst.status = Status::Running;
        inst.plugin_revival_pending = true;

        apply(&mut inst, StatusPolicy::Authoritative, Status::Running);

        assert_eq!(inst.status, Status::Running);
        assert!(
            !inst.plugin_revival_pending,
            "a no-op transition to an already-counted status must still release a stale mark"
        );
    }

    #[test]
    fn relayed_intent_stamp_wipes_concurrent_archive() {
        // Full #3465 residual chain on structured rows.
        let user_touch = chrono::Utc::now() - chrono::Duration::seconds(60);

        let mut daemon_row = stopped_structured_instance();
        daemon_row.status = Status::Idle;
        daemon_row.last_accessed_at = Some(user_touch);
        apply(
            &mut daemon_row,
            StatusPolicy::Authoritative,
            Status::Running,
        );

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

    /// Which policies may move which statuses. Background sub-agent events
    /// (`Background`) must not speak for the main turn's Waiting, Error, or
    /// Stopped (#4001); trailing events never wake a Stopped or trashed row;
    /// and no transition stamps `last_accessed_at` (#3465).
    #[test]
    fn apply_status_intent_matrix() {
        use Status::{Creating, Deleting, Error, Idle, Running, Stopped, Waiting};
        use StatusPolicy::{Authoritative, Background, Heal};
        let user_touch = chrono::Utc::now() - chrono::Duration::seconds(60);
        // (from, trashed, policy, target, want)
        let cases = [
            (Stopped, false, Heal, Idle, Idle),
            (Error, false, Heal, Idle, Idle),
            (Running, false, Heal, Idle, Running),
            (Idle, false, Authoritative, Running, Running),
            (Running, false, Authoritative, Idle, Idle),
            (Waiting, false, Authoritative, Running, Running),
            (Stopped, false, Authoritative, Running, Stopped),
            (Stopped, false, Authoritative, Waiting, Stopped),
            (Stopped, false, Authoritative, Idle, Stopped),
            (Running, true, Authoritative, Error, Running),
            (Idle, false, Background, Running, Running),
            (Running, false, Background, Idle, Idle),
            (Waiting, false, Background, Running, Waiting),
            (Waiting, false, Background, Idle, Waiting),
            (Error, false, Background, Running, Error),
            (Error, false, Background, Idle, Error),
            (Stopped, false, Background, Running, Stopped),
            (Deleting, false, Authoritative, Running, Deleting),
            (Deleting, false, Heal, Idle, Deleting),
            (Creating, false, Authoritative, Running, Creating),
            (Creating, false, Heal, Idle, Creating),
        ];
        for (from, trashed, policy, target, want) in cases {
            let case = format!("{from:?} trashed={trashed} {policy:?} {target:?}");
            let mut inst = stopped_structured_instance();
            inst.status = from;
            inst.last_accessed_at = Some(user_touch);
            if trashed {
                inst.trash();
            }
            apply(&mut inst, policy, target);
            assert_eq!(inst.status, want, "{case}");
            assert_eq!(inst.last_accessed_at, Some(user_touch), "{case}");
            if want != from {
                assert_eq!(inst.idle_entered_at.is_some(), want == Idle, "{case}");
            }
        }
    }

    /// Daemon restart uses the original request's real admission and terminal.
    #[tokio::test]
    #[serial_test::serial]
    async fn seed_acp_statuses_follows_the_folded_status() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        for running in [false, true] {
            with_managed(|scope| {
                Box::pin(async move {
                    let mut live =
                        test_support::managed_control_session(scope, "seed-status").await;
                    let id = live.id().to_owned();
                    let request = live.start_prompt("go").await;
                    if !running {
                        live.finish_prompt(&request, None).await;
                    }
                    live.state.acp_control_cache.forget(&id);
                    live.state
                        .instances
                        .write()
                        .await
                        .iter_mut()
                        .find(|row| row.id == id)
                        .unwrap()
                        .status = Status::Stopped;
                    seed_acp_statuses(live.state.clone()).await;
                    assert_eq!(
                        live.state
                            .instances
                            .read()
                            .await
                            .iter()
                            .find(|row| row.id == id)
                            .unwrap()
                            .status,
                        if running {
                            Status::Running
                        } else {
                            Status::Stopped
                        },
                    );
                })
            })
            .await;
        }
        let mut fresh = stopped_structured_instance();
        fresh.status = Status::Starting;
        let state = test_support::build_test_app_state(vec![fresh]);
        seed_acp_statuses(state.clone()).await;
        assert_eq!(state.instances.read().await[0].status, Status::Starting);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn seeded_task_activity_preserves_an_open_main_turn() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        for main_running in [false, true] {
            with_managed(|scope| {
                Box::pin(async move {
                    let mut live = test_support::managed_control_session(scope, "seed-task").await;
                    let id = live.id().to_owned();
                    let request = live.start_prompt("go").await;
                    let path = start_projection_resource(&mut live, false).await;
                    if !main_running {
                        live.finish_prompt(&request, None).await;
                    }
                    live.state.acp_control_cache.forget(&id);
                    live.state
                        .instances
                        .write()
                        .await
                        .iter_mut()
                        .find(|row| row.id == id)
                        .unwrap()
                        .status = Status::Stopped;
                    seed_acp_statuses(live.state.clone()).await;
                    assert_eq!(
                        live.state
                            .instances
                            .read()
                            .await
                            .iter()
                            .find(|row| row.id == id)
                            .unwrap()
                            .status,
                        Status::Running,
                    );
                    finish_projection_resource(&mut live, &path, false).await;
                    live.state.acp_control_cache.forget(&id);
                    seed_acp_statuses(live.state.clone()).await;
                    assert_eq!(
                        live.state
                            .instances
                            .read()
                            .await
                            .iter()
                            .find(|row| row.id == id)
                            .unwrap()
                            .status,
                        if main_running {
                            Status::Running
                        } else {
                            Status::Idle
                        },
                    );
                })
            })
            .await;
        }
    }

    /// Reattachment preserves the route attested by the actual original establishment.
    #[tokio::test]
    #[serial_test::serial]
    async fn acp_event_listener_attests_the_observed_store_on_a_reused_acp_session() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                let live = test_support::managed_control_session(scope, "attested-reuse").await;
                let observed_store = live
                    .client
                    .establishment_projection()
                    .unwrap()
                    .receipt_snapshot()
                    .unwrap()
                    .store
                    .clone()
                    .expect("actual Native store observation");
                let id = live.id().to_owned();
                let sid = live.sid.clone();
                let original = load_profile_row("test", &id).unwrap();
                assert_eq!(
                    original
                        .agent_session_binding
                        .as_ref()
                        .and_then(|binding| binding.execution.as_ref()),
                    Some(&observed_store)
                );
                let binding = original.agent_session_binding.clone();
                let (listener, handoff) = prepare_managed_listener(scope);
                let (attached_state, reconciled_state) = tokio::sync::oneshot::channel();
                let attached = scope
                    .reconcile_after(live.client, |state, id| async move {
                        activate_managed_listener(handoff, &state).await;
                        attached_state
                            .send(state.clone())
                            .unwrap_or_else(|_| panic!("reconciled state receiver remains open"));
                        let project = state
                            .instances
                            .read()
                            .await
                            .iter()
                            .find(|row| row.id == id)
                            .unwrap()
                            .project_path
                            .clone();
                        state
                            .acp_supervisor
                            .attach(id.clone(), project.into(), Vec::new(), false, None)
                            .await
                            .unwrap();
                    })
                    .await;
                let state = reconciled_state.await.unwrap();
                assert_eq!(attached.selected_native_session(), sid);
                let row = state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .clone();
                let durable = load_profile_row("test", &id).unwrap();
                for row in [&row, &durable] {
                    assert_eq!(row.acp_session_id.as_deref(), Some(sid.as_str()));
                    assert_eq!(row.agent_session_binding, binding);
                    assert_eq!(
                        row.agent_session_binding
                            .as_ref()
                            .and_then(|binding| binding.execution.as_ref()),
                        Some(&observed_store),
                        "reuse cannot replace the observed store with configuration"
                    );
                }
                listener.abort();
                let _ = listener.await;
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn delayed_assignment_preserves_newer_choice_and_uncompleted_flow() {
        use crate::acp::establishment::{
            EstablishmentExpected, EstablishmentRecovery, EstablishmentRecoveryAction,
        };
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::session::{ConversationProvenance, ResumeIntent};
        for case in [
            "use_other",
            "use_same",
            "cleared",
            "fork",
            "import_new",
            "import_same",
        ] {
            with_managed(|scope| {
                Box::pin(async move {
                    let mut publications = scope.state.acp_events_tx.subscribe();
                    let live =
                        test_support::managed_control_session(scope, "delayed-assignment").await;
                    let state = live.state.clone();
                    let id = live.id().to_owned();
                    await_row(
                        &state,
                        &id,
                        Instance::launch_is_finalized,
                        "actual original creation finalizes before a newer user choice",
                    )
                    .await;
                    let submission = state.session_service.prompt_submission(&id).await;
                    drop(submission);
                    let original = load_profile_row("test", &id).unwrap();
                    let frame = observed_frame(&mut publications, &id, |event| {
                        matches!(
                            event,
                            Event::AcpSessionAssigned {
                                connection_ready: true,
                                ..
                            }
                        )
                    })
                    .await;
                    let original_epoch = live.client.lease.epoch();
                    assert_ne!(
                        frame.scope,
                        crate::acp::protocol::AcpEventScope::HistoryOnly
                    );
                    let gate = pause_assignment_listener_for_test(&id, frame.seq);
                    let listener = start_managed_listener(scope, &state).await;
                    state.acp_events_tx.send(frame).unwrap();
                    tokio::time::timeout(std::time::Duration::from_secs(5), gate.arrived)
                        .await
                        .unwrap()
                        .unwrap();
                    let storage =
                        Arc::new(crate::session::Storage::open_reference("test").unwrap());
                    let mut chosen = original.clone();
                    chosen.lifecycle_generation += 1;
                    match case {
                        "use_other" | "use_same" => {
                            let target = if case == "use_same" {
                                live.sid.clone()
                            } else {
                                "33333333-3333-4333-8333-333333333333".into()
                            };
                            chosen.resume_intent = ResumeIntent::Use(target.clone());
                            let mut pin = original.agent_session_binding.clone().unwrap();
                            pin.session_id = target;
                            pin.provenance = ConversationProvenance::Asserted;
                            chosen.resume_binding = Some(pin);
                            chosen.import_pending = Some(true);
                        }
                        "cleared" => {
                            let recovery = EstablishmentRecovery {
                                expected: EstablishmentExpected::capture(&chosen, &storage)
                                    .unwrap(),
                                action: EstablishmentRecoveryAction::Fresh,
                            };
                            chosen
                                .arm_acp_establishment(&storage, Some(&recovery))
                                .unwrap();
                        }
                        "fork" => {
                            let parent = "44444444-4444-4444-8444-444444444444".to_owned();
                            chosen.resume_intent = ResumeIntent::Fork {
                                from: parent.clone(),
                            };
                            chosen.fork_pending = Some(parent);
                            chosen.import_pending = Some(true);
                        }
                        "import_new" => {
                            chosen.acp_session_id =
                                Some("55555555-5555-4555-8555-555555555555".into());
                            chosen.import_pending = Some(true);
                        }
                        "import_same" => chosen.import_pending = Some(true),
                        _ => unreachable!(),
                    }
                    if case != "cleared" {
                        chosen.arm_acp_establishment(&storage, None).unwrap();
                    }
                    chosen.validate_acp_establishment(&storage).unwrap();
                    let disk = storage.clone();
                    let replacement = chosen.clone();
                    let disk_id = id.clone();
                    tokio::task::spawn_blocking(move || {
                        let _lifecycle = disk.acquire_instance_lifecycle_lock(&disk_id)?;
                        disk.update(|rows, _| {
                            *rows.iter_mut().find(|row| row.id == disk_id).unwrap() = replacement;
                            Ok(())
                        })
                    })
                    .await
                    .unwrap()
                    .unwrap();
                    let mut cached = chosen.clone();
                    cached.status = Status::Error;
                    cached.last_error = Some("operation A failed".into());
                    *state
                        .instances
                        .write()
                        .await
                        .iter_mut()
                        .find(|row| row.id == id)
                        .unwrap() = cached;
                    let before_effect = state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .clone();
                    gate.release.send(()).unwrap();
                    tokio::time::timeout(std::time::Duration::from_secs(5), gate.applied)
                        .await
                        .unwrap()
                        .unwrap();
                    listener.abort();
                    let _ = listener.await;
                    let cached = state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .clone();
                    let durable = load_profile_row("test", &id).unwrap();
                    let observed_epoch = state
                        .acp_supervisor
                        .current_lease(&id)
                        .map(|lease| lease.epoch());
                    let restore = original.clone();
                    let restore_id = id.clone();
                    tokio::task::spawn_blocking(move || {
                        storage.update(|rows, _| {
                            *rows.iter_mut().find(|row| row.id == restore_id).unwrap() = restore;
                            Ok(())
                        })
                    })
                    .await
                    .unwrap()
                    .unwrap();
                    *state
                        .instances
                        .write()
                        .await
                        .iter_mut()
                        .find(|row| row.id == id)
                        .unwrap() = original;
                    for candidate in [&cached, &durable] {
                        assert_eq!(
                            candidate.acp_session_id, chosen.acp_session_id,
                            "{case}: no stale SID grant"
                        );
                        assert!(
                            chosen.conversation_state().matches(candidate),
                            "{case}: preserve newer choice"
                        );
                        assert_eq!(candidate.fork_pending, chosen.fork_pending, "{case}");
                        assert_eq!(candidate.import_pending, chosen.import_pending, "{case}");
                        assert_eq!(
                            candidate.lifecycle_generation, chosen.lifecycle_generation,
                            "{case}"
                        );
                        assert_eq!(
                            candidate.acp_establishment, chosen.acp_establishment,
                            "{case}"
                        );
                    }
                    assert_eq!(cached.status, Status::Error);
                    assert_eq!(cached.last_error, before_effect.last_error, "{case}");
                    assert_eq!(
                        observed_epoch,
                        Some(original_epoch),
                        "same original epoch does not authorize a different physical context"
                    );
                })
            })
            .await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn real_attach_commits_pending_fork_and_same_sid_import_without_reseeding_history() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        for fork in [false, true] {
            with_managed(|scope| Box::pin(async move {
                let project = scope.tempdir();
                let native_store = project.path().join("native-store");
                std::fs::create_dir(&native_store).unwrap();
                scope.save_environment(&["CLAUDE_CONFIG_DIR"]);
                std::env::set_var("CLAUDE_CONFIG_DIR", &native_store);
                let parent = uuid::Uuid::new_v4().to_string();
                let child = uuid::Uuid::new_v4().to_string();
                let source_sid = parent.clone();
                let source_cwd = project.path().to_path_buf();
                let source_transcripts = native_store.join("projects").join("managed-native");
                std::fs::create_dir_all(&source_transcripts).unwrap();
                let (source_config, _source_native) = scope.native_peer(
                    project.path().into(), "claude", "claude", move |request| {
                        Some(match request["method"].as_str().unwrap() {
                            "initialize" => serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                                "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}),
                            "session/new" => {
                                std::fs::write(
                                    source_transcripts.join(format!("{source_sid}.jsonl")),
                                    format!("{}\n", serde_json::json!({
                                        "type":"system","subtype":"init","sessionId":source_sid,"cwd":source_cwd,
                                    })),
                                ).unwrap();
                                serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                                    "result":{"sessionId":source_sid}})
                            }
                            "session/cancel" => return None,
                            method => panic!("unexpected original Native method: {method}"),
                        })
                    },
                );
                let original = scope.spawn(source_config, "attach-source").await.unwrap();
                original.ready().await;
                let selected = if fork { child.clone() } else { parent.clone() };
                let (sent, mut prompts) = tokio::sync::mpsc::unbounded_channel();
                let (config, native) = scope.native_peer(
                    project.path().into(), "claude", "claude", move |request| {
                        Some(match request["method"].as_str().unwrap() {
                            "initialize" => serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                                "result":{"protocolVersion":1,"agentCapabilities":{
                                    "loadSession":true,"sessionCapabilities":{"fork":{}}}}}),
                            "session/load" => serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{}}),
                            "session/fork" => serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                                "result":{"sessionId":child}}),
                            "session/prompt" => { sent.send(request.clone()).unwrap(); return None; }
                            "session/cancel" => return None,
                            method => panic!("unexpected Native method: {method}"),
                        })
                    },
                );
                let mut client = if fork {
                    scope.fork_after(original, config, "attach-pending-context").await
                } else {
                    scope.import_after(original, config, "attach-pending-context").await
                };
                client.ready().await;
                let id = client.id.clone();
                let owner = client.connected_runner();
                assert_eq!(client.selected_native_session(), selected);
                use axum::response::IntoResponse;
                let state = client.app_state();
                let response = crate::server::api::acp_prompt(
                    axum::extract::State(state.clone()), axum::extract::Path(id.clone()),
                    Ok(axum::Json(crate::acp::protocol::PromptRequest {
                        text: "original explicit request".into(), attachments: Vec::new(),
                        prompt_id: None, no_revive: false, queued_prompt_id: None,
                    })),
                ).await.into_response();
                assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
                let request = tokio::time::timeout(std::time::Duration::from_secs(10), prompts.recv())
                    .await.unwrap().unwrap();
                native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":selected,"update":{"sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text","text":"history before daemon restart"}}}})).await;
                native.send(serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                    "result":{"stopReason":"end_turn"}})).await;
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while !matches!(client.next_event().await.unwrap(), Event::Stopped { .. }) {}
                }).await.unwrap();
                let original_inputs = native.requests();
                for round in 0..2 {
                    client = scope.adopt(client, false).await;
                    client.ready().await;
                    let state = client.app_state();
                    let cached = state.instances.read().await.iter().find(|row| row.id == id).unwrap().clone();
                    let durable = load_profile_row("test", &id).unwrap();
                    for row in [&cached, &durable] {
                        assert_eq!(row.acp_session_id.as_deref(), Some(selected.as_str()), "round {round}");
                        assert!(row.fork_pending.is_none(), "never fork the parent twice");
                        assert!(row.import_pending.is_none(), "never seed imported history twice");
                    }
                    let history = state.acp_event_store.replay_from(&id, 0).unwrap();
                    assert_eq!(history.iter().filter(|stored| matches!(stored.event.as_ref(),
                        Event::UserPromptSent { text, .. } if text == "original explicit request"
                    )).count(), 1, "round {round}: retain actual user request");
                    assert_eq!(history.iter().filter(|stored| matches!(stored.event.as_ref(),
                        Event::AgentMessageChunk { text, .. } if text == "history before daemon restart"
                    )).count(), 1, "round {round}: retain actual output without replay duplicates");
                    assert_eq!(client.connected_runner(), owner);
                    for method in ["session/new", "session/load", "session/fork", "session/prompt"] {
                        assert_eq!(native.requests().iter().filter(|seen| seen.as_str() == method).count(),
                            original_inputs.iter().filter(|seen| seen.as_str() == method).count(),
                            "round {round}: attach does not repeat {method}");
                    }
                }
            })).await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn stale_sidebar_observation_cannot_replace_a_later_native_failure() {
        use crate::acp::supervisor::test_support::managed::with_managed;

        with_managed(|scope| {
            Box::pin(async move {
                let state = scope.state.clone();
                let listener = start_managed_listener(scope, &state).await;
                let mut live = test_support::managed_control_session(scope, "stale-sidebar").await;
                let id = live.id().to_owned();
                let first = live.start_prompt("the original completed input").await;
                live.finish_prompt(&first, None).await;
                await_row(
                    &state,
                    &id,
                    |row| row.status == Status::Idle,
                    "the first actual Native result reaches the sidebar",
                )
                .await;
                let mut observation = folded_sidebar_observation(&state.session_service, &id)
                    .await
                    .expect("actual qualified completed Native observation");
                assert_eq!(observation.status, Status::Idle);
                let frontier = state
                    .session_service
                    .fold_control_state(&id)
                    .await
                    .unwrap()
                    .last_seq;

                let failed = live.start_prompt("a distinct later input fails").await;
                live.finish_prompt(&failed, Some("internal_error")).await;
                let failure = await_row(
                    &state,
                    &id,
                    |row| row.status == Status::Error,
                    "the later real Native failure reaches the sidebar before the old commit",
                )
                .await;
                assert!(
                    state
                        .session_service
                        .fold_control_state(&id)
                        .await
                        .unwrap()
                        .last_seq
                        > frontier
                );
                assert!(live.client.connection_ready());
                assert!(live.client.connection_unavailable().is_none());
                let namespace = state.profile_namespace.read().await;
                let mut rows = state.instances.write().await;
                let row = rows.iter_mut().find(|row| row.id == id).unwrap();
                let applied = observation.try_apply(&id, row, |row, _, captured| {
                    apply_status_seed(row, captured.status, &state.status_tx);
                    Ok(())
                });
                assert_eq!(
                    row.status,
                    Status::Error,
                    "the old Idle observation cannot overwrite a later same-owner Native failure"
                );
                assert_eq!(row.last_error, failure.last_error);
                assert!(
                    applied.is_none(),
                    "a changed control frontier refuses the old observation"
                );
                drop(rows);
                drop(namespace);
                listener.abort();
                let _ = listener.await;
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn owned_channel_ready_heals_only_the_current_identity_fenced_connection_error() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        const TEST_NAME: &str = "server::acp_events::tests::owned_channel_ready_heals_only_the_current_identity_fenced_connection_error";
        for condition in ["ready", "sticky_loss", "superseded", "operation_error"] {
            with_managed(|scope| {
                Box::pin(async move {
                    let inspection = scope.inspect_next_runner(TEST_NAME);
                    let initial_state = scope.state.clone();
                    let initial_listener = start_managed_listener(scope, &initial_state).await;
                    let mut live =
                        test_support::managed_control_session(scope, "ready-repair").await;
                    let (attached_state, reconciled_state) = tokio::sync::oneshot::channel();
                    let id = live.id().to_owned();
                    let original = load_profile_row("test", &id).unwrap();
                    let owner = live.client.connected_runner();
                    let original_receipt = live
                        .client
                        .establishment_projection()
                        .unwrap()
                        .receipt_snapshot()
                        .unwrap();
                    let request = live
                        .start_prompt("original operation before control loss")
                        .await;
                    await_row(
                        &live.state,
                        &id,
                        |row| row.status == Status::Running,
                        "the original Native input reaches the sidebar before its result",
                    )
                    .await;
                    live.finish_prompt(
                        &request,
                        (condition == "operation_error").then_some("internal_error"),
                    )
                    .await;
                    let terminal = await_row(
                        &live.state,
                        &id,
                        |row| matches!(row.status, Status::Idle | Status::Error),
                        "the original Native result reaches the sidebar before control loss",
                    )
                    .await;
                    assert_eq!(
                        terminal.status,
                        if condition == "operation_error" {
                            Status::Error
                        } else {
                            Status::Idle
                        },
                        "{condition}"
                    );
                    assert_eq!(
                        folded_sidebar_status(&live.state.session_service, &id).await,
                        Some(if condition == "operation_error" {
                            Status::Error
                        } else {
                            Status::Idle
                        }),
                        "the original Native turn, not establishment metadata, owns the sidebar"
                    );
                    let native_requests = live.native.requests();
                    initial_listener.abort();
                    let _ = initial_listener.await;
                    let mut publications = live.client.subscribe_publications();
                    inspection.query("close-control").await;
                    tokio::time::timeout(std::time::Duration::from_secs(10), async {
                        while live.client.connection_unavailable().is_none() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("actual daemon control EOF becomes observable");
                    assert_eq!(
                        live.client.connection_unavailable().unwrap().source,
                        crate::acp::state::AcpConnectionSource::Control
                    );
                    let loss = observed_frame(&mut publications, &id, |event| {
                        matches!(
                            event,
                            Event::AcpConnectionUnavailable {
                                source: crate::acp::state::AcpConnectionSource::Control,
                                ..
                            }
                        )
                    })
                    .await;
                    assert!(loss.availability_denial);
                    assert_eq!(loss.native_origin.unwrap().owner, owner);
                    assert_eq!(loss.native_subject.unwrap().owner, owner);
                    assert_eq!(loss.worker_generation, Some(live.client.lease.epoch()));
                    let original_drain = live.client.pause_drain().await;
                    seed_acp_statuses(live.state.clone()).await;
                    let failed = await_row(
                        &live.state,
                        &id,
                        |row| row.status == Status::Error,
                        "qualified actual control loss seeds the connection Error",
                    )
                    .await;
                    let error_baseline = failed.last_error.clone();
                    drop(original_drain);
                    let gate = assignment_listener_gate_for_test(&id, None);
                    let (listener, handoff) = prepare_managed_listener(scope);
                    let attached = scope
                        .reconcile_after(live.client, |state, id| async move {
                            activate_managed_listener(handoff, &state).await;
                            attached_state
                                .send((state.clone(), state.acp_events_tx.subscribe()))
                                .unwrap_or_else(|_| {
                                    panic!("reconciled state receiver remains open")
                                });
                            {
                                let mut rows = state.instances.write().await;
                                let row = rows.iter_mut().find(|row| row.id == id).unwrap();
                                assert_eq!(row.source_profile, failed.source_profile);
                                assert_eq!(row.created_at, failed.created_at);
                                assert_eq!(row.lifecycle_generation, failed.lifecycle_generation);
                                assert!(row.conversation_state().matches(&failed));
                                assert_eq!(row.acp_establishment, failed.acp_establishment);
                                row.merge_runtime_from_reload(&failed);
                            }
                            let project = state
                                .instances
                                .read()
                                .await
                                .iter()
                                .find(|row| row.id == id)
                                .unwrap()
                                .project_path
                                .clone();
                            state
                                .acp_supervisor
                                .attach(id, project.into(), Vec::new(), false, None)
                                .await
                                .unwrap();
                        })
                        .await;
                    let (state, mut attached_publications) = reconciled_state.await.unwrap();
                    let attached_receipt = attached
                        .establishment_projection()
                        .unwrap()
                        .receipt_snapshot()
                        .unwrap();
                    assert_eq!(attached_receipt.owner, original_receipt.owner);
                    assert_eq!(attached_receipt.attempt, original_receipt.attempt);
                    assert_eq!(attached.connected_runner(), owner);
                    let timeout = std::time::Duration::from_secs(10);
                    tokio::time::timeout(timeout, gate.arrived)
                        .await
                        .unwrap()
                        .unwrap();
                    let assignment = observed_frame(&mut attached_publications, &id, |event| {
                        matches!(
                            event,
                            Event::AcpSessionAssigned {
                                connection_ready: true,
                                ..
                            }
                        )
                    })
                    .await;
                    assert!(assignment.seq > loss.seq);
                    assert_eq!(assignment.worker_generation, Some(attached.lease.epoch()));
                    assert_eq!(
                        assignment.scope,
                        crate::acp::protocol::AcpEventScope::HistoryOnly,
                        "read-only attach does not create an Original constructor observation"
                    );
                    assert!(attached.connection_ready());
                    assert!(attached.connection_unavailable().is_none());
                    assert_eq!(
                        state
                            .instances
                            .read()
                            .await
                            .iter()
                            .find(|row| row.id == id)
                            .unwrap()
                            .status,
                        Status::Error
                    );
                    let drain_pause = attached.pause_drain().await;
                    if condition == "sticky_loss" {
                        live.native.close_stdout().await;
                        tokio::time::timeout(timeout, async {
                            while attached.connection_unavailable().is_none() {
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .expect("real Native EOF precedes the current-availability refresh");
                    } else if condition == "superseded" {
                        let disk_id = id.clone();
                        tokio::task::spawn_blocking(move || {
                            crate::session::Storage::open_reference("test")
                                .unwrap()
                                .update(|rows, _| {
                                    rows.iter_mut()
                                        .find(|row| row.id == disk_id)
                                        .unwrap()
                                        .lifecycle_generation += 1;
                                    Ok(())
                                })
                        })
                        .await
                        .unwrap()
                        .unwrap();
                        state
                            .instances
                            .write()
                            .await
                            .iter_mut()
                            .find(|row| row.id == id)
                            .unwrap()
                            .lifecycle_generation += 1;
                    }
                    gate.release.send(()).unwrap();
                    tokio::time::timeout(timeout, gate.applied)
                        .await
                        .unwrap()
                        .unwrap();
                    // The delayed assignment is history, not fresh readiness authority.
                    // Refresh through the actual current actor at this receive frontier.
                    seed_acp_statuses(state.clone()).await;
                    let row = state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .clone();
                    listener.abort();
                    let _ = listener.await;
                    if condition == "superseded" {
                        let restore = original.clone();
                        let restore_id = id.clone();
                        tokio::task::spawn_blocking(move || {
                            crate::session::Storage::open_reference("test")
                                .unwrap()
                                .update(|rows, _| {
                                    *rows.iter_mut().find(|row| row.id == restore_id).unwrap() =
                                        restore;
                                    Ok(())
                                })
                        })
                        .await
                        .unwrap()
                        .unwrap();
                        *state
                            .instances
                            .write()
                            .await
                            .iter_mut()
                            .find(|row| row.id == id)
                            .unwrap() = original;
                    }
                    assert_eq!(
                        row.status,
                        if condition == "ready" {
                            Status::Idle
                        } else {
                            Status::Error
                        },
                        "{condition}"
                    );
                    assert_eq!(row.last_error, error_baseline, "{condition}");
                    assert_eq!(
                        live.native.requests(),
                        native_requests,
                        "same-owner reattachment must not repeat Native input"
                    );
                    if condition == "ready" {
                        let folded = state.session_service.fold_control_state(&id).await.unwrap();
                        assert!(folded.connection_ready && folded.connection_unavailable.is_none());
                        assert_eq!(folded.sidebar_status(), Status::Idle);
                        assert_eq!(folded.native_availability.unwrap().0, loss.seq);
                        assert_eq!(folded.sidebar_observation_seq, loss.seq);
                        assert_eq!(state.acp_supervisor.running_identity(&id), owner);
                    }
                    drop(drain_pause);
                })
            })
            .await;
        }
    }
    fn prepare_managed_listener(
        scope: &mut crate::acp::supervisor::test_support::managed::ManagedScope,
    ) -> (
        crate::acp::supervisor::test_support::managed::ManagedTask,
        tokio::sync::oneshot::Sender<Arc<AppState>>,
    ) {
        let (handoff, state) = tokio::sync::oneshot::channel();
        let listener = scope.spawn_helper(async move {
            if let Ok(state) = state.await {
                acp_event_listener(state).await;
            }
        });
        (listener, handoff)
    }

    async fn activate_managed_listener(
        handoff: tokio::sync::oneshot::Sender<Arc<AppState>>,
        state: &Arc<AppState>,
    ) {
        let before = state.acp_events_tx.receiver_count();
        handoff
            .send(state.clone())
            .unwrap_or_else(|_| panic!("managed listener awaits its actual AppState"));
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while state.acp_events_tx.receiver_count() <= before {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the new listener subscribes, not an existing managed observer");
    }

    async fn start_managed_listener(
        scope: &mut crate::acp::supervisor::test_support::managed::ManagedScope,
        state: &Arc<AppState>,
    ) -> crate::acp::supervisor::test_support::managed::ManagedTask {
        let (listener, handoff) = prepare_managed_listener(scope);
        activate_managed_listener(handoff, state).await;
        listener
    }

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

    /// A tool title is agent-authored text (Claude's Bash reporter uses the
    /// full command), so an inline credential must never reach the log.
    #[test]
    fn approval_request_log_carries_the_call_id_and_never_the_title() {
        let frame = AcpBroadcastFrame::from_qualified(
            "sess1".into(),
            1,
            Arc::new(crate::acp::Event::ApprovalRequested {
                approval: crate::acp::approvals::Approval {
                    nonce: crate::acp::approvals::Nonce("n1".into()),
                    tool_call: crate::acp::state::ToolCall {
                        id: "call-7".into(),
                        name: "curl -H 'Authorization: Bearer SECRET_SENTINEL_9f3'".into(),
                        kind: "execute".into(),
                        args_preview: String::new(),
                        started_at: chrono::Utc::now(),
                        parent_tool_call_id: None,
                        memory_recall: None,
                        diffs: Vec::new(),
                    },
                    destructive: false,
                    options: Vec::new(),
                    choice: false,
                    plan: None,
                    requested_at: chrono::Utc::now(),
                    resolved: None,
                },
            }),
            &crate::acp::event_sender::EventMetadata::history(),
            None,
            None,
            None,
        );
        let out = test_support::capture_log(|| log_frame(&frame));
        assert!(out.contains("approval requested"), "{out}");
        assert!(out.contains("call=call-7"), "{out}");
        assert!(out.contains("kind=execute"), "{out}");
        assert!(!out.contains("SECRET_SENTINEL_9f3"), "{out}");
    }

    /// A failed one-shot's message is its last stderr/stdout line, which can echo
    /// prompt text or credentials; the warning must name the outcome only.
    #[test]
    fn smart_rename_failure_log_never_carries_child_output() {
        use crate::session::smart_rename::SmartRenameError;
        let cases = [
            (
                Err(SmartRenameError::Failed(
                    "`claude` failed: token=SECRET_SENTINEL_c41".into(),
                )),
                "smart rename failed",
            ),
            (Err(SmartRenameError::TimedOut), "smart rename timed out"),
            (
                Err(SmartRenameError::Skipped("Session not found".into())),
                "smart rename skipped",
            ),
        ];
        for (result, expected) in cases {
            let out = test_support::capture_log(|| log_smart_rename_outcome("sess1", &result));
            assert!(out.contains(expected), "{out}");
            assert!(out.contains("session=sess1"), "{out}");
            assert!(!out.contains("SECRET_SENTINEL_c41"), "{out}");
        }
        let out = test_support::capture_log(|| log_smart_rename_outcome("sess1", &Ok(())));
        assert!(out.is_empty(), "{out}");
    }
}
