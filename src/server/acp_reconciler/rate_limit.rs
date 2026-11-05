//! Opt-in rate-limit auto-resume (#1722) with a bounded redelivery budget (#3688).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::{is_resumable, query_control, query_store, resolve_per_profile, AppState};
use crate::acp::event_store::ControlKnowledge::{Indeterminate, Known};
use crate::acp::state::RATE_LIMIT_EXHAUSTED_RETRIES_REASON;
use crate::acp::supervisor::SupervisorError;

pub(super) const RATE_LIMIT_RESUME_INTERVAL: Duration = Duration::from_secs(15);

/// Added to the reported `resets_at` to absorb clock skew and adapter jitter.
const RATE_LIMIT_AUTO_RESUME_GRACE_SECS: u32 = 15;

/// Floor on the park window from when the limit was recorded, so a past
/// `resets_at` cannot drive a tight respawn loop.
const RATE_LIMIT_MIN_PARK_SECS: i64 = 30;

/// Base retry when no reset was reported (#3152), doubled per redelivery spent
/// so the five allowed redeliveries span 31 hours (#3688).
const RATE_LIMIT_UNKNOWN_RESET_RETRY_SECS: i64 = 3600;
const RATE_LIMIT_UNKNOWN_RESET_MAX_SHIFT: u32 = 4;

/// Redeliveries per rate-limit streak before the session parks on a terminal
/// stop. `EventStore::rate_limit_redelivery_streak` defines the streak.
const RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES: i64 = 5;

/// The later of the reported reset plus grace and the minimum park floor.
fn rate_limit_resume_at(
    resets_at: DateTime<Utc>,
    recorded_at_ms: i64,
    grace_secs: u32,
) -> DateTime<Utc> {
    let resets_plus_grace = resets_at + chrono::Duration::seconds(i64::from(grace_secs));
    match DateTime::from_timestamp_millis(recorded_at_ms)
        .map(|t| t + chrono::Duration::seconds(RATE_LIMIT_MIN_PARK_SECS))
    {
        Some(floor) if floor > resets_plus_grace => floor,
        _ => resets_plus_grace,
    }
}

fn rate_limit_unknown_reset_retry_at(recorded_at_ms: i64, redeliveries: i64) -> DateTime<Utc> {
    let shift = redeliveries.clamp(0, i64::from(RATE_LIMIT_UNKNOWN_RESET_MAX_SHIFT)) as u32;
    let retry_after =
        chrono::Duration::seconds(RATE_LIMIT_UNKNOWN_RESET_RETRY_SECS.saturating_mul(1 << shift));
    DateTime::from_timestamp_millis(recorded_at_ms).unwrap_or_else(Utc::now) + retry_after
}

fn row_minted_at_ms(entry: &crate::daemon::QueuedPromptEntry) -> Option<i64> {
    DateTime::parse_from_rfc3339(&entry.created_at)
        .ok()
        .map(|queued_at| queued_at.timestamp_millis())
}

/// Whether any queued prompt was minted after the limit that interrupted the
/// turn, so one of them replaced the continuation. Checking every row and not
/// the next one is what honours a replacement queued behind a follow-up the
/// user typed before the limit; that shape costs the interrupted prompt
/// instead, and the alternative costs the user's freshest word.
///
/// An unknown limit never supersedes: the interrupted request is the older one
/// and losing it is final, since the first prompt the drain delivers retires
/// the park.
fn queue_supersedes(minted_at_ms: &[i64], limit_at_ms: Option<i64>) -> bool {
    let Some(limit_at_ms) = limit_at_ms else {
        return false;
    };
    minted_at_ms
        .iter()
        .any(|queued_at| *queued_at > limit_at_ms)
}

/// Sent when a limit ended an agent-initiated turn, which has no prompt to redeliver.
const AGENT_TURN_CONTINUATION: &str = "continue";

/// What the continuation producer decided for one session (#4092).
#[must_use]
pub(crate) enum ContinuationOutcome {
    /// The interrupted prompt is the next turn, is already installed, or there
    /// was nothing to replay. Only this outcome licenses the automatic
    /// `RateLimitAutoResumed`, which is the budget's arming step.
    Stands,
    /// A queued prompt owns the next turn, and any continuation an earlier
    /// cadence installed is cleared. That prompt has no other route to a
    /// worker, so the caller still frees the respawn, but no automatic
    /// breadcrumb.
    SupersededByQueue,
}

async fn resolve_continuation(
    state: &Arc<AppState>,
    id: &str,
) -> Result<Option<(String, Vec<crate::daemon::PromptAttachmentRef>)>, SupervisorError> {
    let interrupted = query_control(
        &state.acp_event_store,
        id,
        "rate-limit continuation",
        |store, id| store.rate_limited_turn_prompt(id),
    )
    .await?;
    match interrupted {
        Known(Some(prompt)) => Ok(Some(prompt)),
        Known(None) => match query_control(
            &state.acp_event_store,
            id,
            "rate-limit agent-turn continuation",
            |store, id| store.rate_limit_interrupted_agent_turn(id),
        )
        .await?
        {
            Known(true) => Ok(Some((AGENT_TURN_CONTINUATION.to_owned(), Vec::new()))),
            Known(false) => Ok(None),
            Indeterminate => Err(SupervisorError::ControlStateUnavailable),
        },
        Indeterminate => Err(SupervisorError::ControlStateUnavailable),
    }
}

#[cfg(test)]
pub(crate) async fn install_rate_limit_continuation(
    state: &Arc<AppState>,
    id: &str,
    submission: &tokio::sync::OwnedMutexGuard<()>,
) -> Result<ContinuationOutcome, SupervisorError> {
    let prepared = resolve_continuation(state, id).await?;
    install_prepared_continuation(state, id, submission, prepared).await
}

async fn install_prepared_continuation(
    state: &Arc<AppState>,
    id: &str,
    _submission: &tokio::sync::OwnedMutexGuard<()>,
    prepared: Option<(String, Vec<crate::daemon::PromptAttachmentRef>)>,
) -> Result<ContinuationOutcome, SupervisorError> {
    if state
        .instances
        .read()
        .await
        .iter()
        .any(|row| row.id == id && row.terminal_handoff.is_some())
    {
        return Err(SupervisorError::SpawnCancelled(id.to_owned()));
    }
    let Some((text, attachments)) = prepared else {
        return Ok(ContinuationOutcome::Stands);
    };
    #[cfg(test)]
    state.session_service.await_install_barrier(id).await;
    let minted_at_ms: Vec<i64> = {
        let instances = state.instances.read().await;
        let inst = instances
            .iter()
            .find(|row| row.id == id)
            .ok_or_else(|| SupervisorError::SpawnCancelled(id.to_owned()))?;
        inst.queued_prompts
            .iter()
            .filter_map(row_minted_at_ms)
            .collect()
    };
    if !minted_at_ms.is_empty() {
        let limit_at_ms = query_control(
            &state.acp_event_store,
            id,
            "rate-limit supersession",
            |store, id| Ok(store.latest_rate_limit_event(id)?.map(|(_, at)| at)),
        )
        .await?;
        if queue_supersedes(&minted_at_ms, limit_at_ms) {
            state.session_service.clear_pending_initial_turn(id).await;
            return Ok(ContinuationOutcome::SupersededByQueue);
        }
    }
    state
        .session_service
        .set_pending_initial_turn(id, text, attachments)
        .await;
    Ok(ContinuationOutcome::Stands)
}

pub(crate) struct QuotaDelivery {
    park_seq: u64,
}

impl QuotaDelivery {
    pub(crate) fn park_seq(&self) -> u64 {
        self.park_seq
    }
}

#[derive(Clone, Copy)]
pub(crate) enum QuotaResume {
    Manual,
    Automatic { park_seq: u64 },
    QueuedAtCap { park_seq: u64 },
}

pub(crate) async fn resume_rate_limited(
    state: &Arc<AppState>,
    id: &str,
    submission: tokio::sync::OwnedMutexGuard<()>,
    kind: QuotaResume,
) -> Result<(), crate::acp::supervisor::SupervisorError> {
    use crate::acp::supervisor::SupervisorError;
    let park = match query_control(&state.acp_event_store, id, "quota release", |store, id| {
        store.rate_limit_park(id)
    })
    .await?
    {
        Known(Some(park)) => park,
        Known(None) => return Ok(()),
        Indeterminate => return Err(SupervisorError::ControlStateUnavailable),
    };
    match kind {
        QuotaResume::Automatic { park_seq } if park.seq != park_seq || park.cap_reached => {
            return Ok(())
        }
        QuotaResume::QueuedAtCap { park_seq } if park.seq != park_seq || !park.cap_reached => {
            return Ok(())
        }
        _ => {}
    }
    let profile = {
        let rows = state.instances.read().await;
        rows.iter()
            .find(|row| row.id == id && is_resumable(row))
            .map(|row| row.source_profile.clone())
            .ok_or_else(|| SupervisorError::SpawnCancelled(id.to_owned()))?
    };
    if let Some(unavailable) = state
        .session_service
        .prompt_native_unavailability(id)
        .await
        .map_err(|_| SupervisorError::ControlStateUnavailable)?
    {
        return Err(crate::acp::acp_client::AcpError::ConnectionUnavailable(unavailable).into());
    }
    let mut resume_at = park
        .info
        .as_ref()
        .and_then(|info| info.resets_at)
        .unwrap_or_else(Utc::now);
    if matches!(kind, QuotaResume::Automatic { .. }) {
        let enabled = resolve_per_profile([profile.clone()].into_iter(), |config| {
            config.acp.rate_limit_auto_resume
        })
        .await;
        if !enabled.get(&profile).copied().unwrap_or(false) {
            return Ok(());
        }
    }
    let streak = query_control(
        &state.acp_event_store,
        id,
        "quota release budget",
        |store, id| store.rate_limit_redelivery_streak(id),
    )
    .await?;
    let Known(streak) = streak else {
        return Err(SupervisorError::ControlStateUnavailable);
    };
    if matches!(kind, QuotaResume::Automatic { .. }) {
        resume_at = match park.info.as_ref().and_then(|info| info.resets_at) {
            Some(reset) => rate_limit_resume_at(
                reset,
                park.recorded_at_ms,
                RATE_LIMIT_AUTO_RESUME_GRACE_SECS,
            ),
            None => rate_limit_unknown_reset_retry_at(park.recorded_at_ms, streak),
        };
        if let Some(attempt) = park
            .last_resume_attempt_ms
            .and_then(DateTime::from_timestamp_millis)
        {
            resume_at =
                resume_at.max(attempt + chrono::Duration::seconds(RATE_LIMIT_MIN_PARK_SECS));
        }
        if Utc::now() < resume_at {
            return Ok(());
        }
        if streak >= RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES {
            let latest = query_store(
                &state.acp_event_store,
                id,
                "quota release frontier",
                |store, id| store.highest_seq(id),
            )
            .await
            .ok_or(SupervisorError::NativeStoreUnavailable)?;
            if state.acp_supervisor.publish_stopped_if_seq(
                id,
                RATE_LIMIT_EXHAUSTED_RETRIES_REASON,
                latest,
            ) {
                state.session_service.clear_pending_initial_turn(id).await;
            }
            return Ok(());
        }
    }
    let authority = if matches!(kind, QuotaResume::Manual) {
        crate::process::worker_registry::ResumeAuthority::Explicit
    } else {
        crate::process::worker_registry::ResumeAuthority::Automatic
    };
    let prepared = if matches!(kind, QuotaResume::QueuedAtCap { .. }) {
        None
    } else {
        resolve_continuation(state, id).await?
    };
    super::resume::ensure_rate_limit_worker(state, id, authority, false).await?;
    let control = state
        .session_service
        .fold_control_state(id)
        .await
        .map_err(|_| SupervisorError::ControlStateUnavailable)?;
    if !control.connection_ready || control.connection_unavailable.is_some() || control.turn_active
    {
        return Err(SupervisorError::SpawnCancelled(id.to_owned()));
    }
    if matches!(kind, QuotaResume::Automatic { .. }) {
        let enabled = resolve_per_profile([profile.clone()].into_iter(), |config| {
            config.acp.rate_limit_auto_resume
        })
        .await;
        if !enabled.get(&profile).copied().unwrap_or(false) {
            return Ok(());
        }
    }
    let current = query_control(
        &state.acp_event_store,
        id,
        "quota release recheck",
        |store, id| store.rate_limit_park(id),
    )
    .await?;
    match current {
        Known(Some(current)) if current.seq == park.seq => {}
        Known(_) => return Ok(()),
        Indeterminate => return Err(SupervisorError::ControlStateUnavailable),
    }
    let delivery = QuotaDelivery { park_seq: park.seq };
    let outcome = if matches!(kind, QuotaResume::QueuedAtCap { .. }) {
        if state
            .session_service
            .queued_prompts_snapshot(id)
            .await
            .is_empty()
        {
            return Ok(());
        }
        state.session_service.clear_pending_initial_turn(id).await;
        ContinuationOutcome::SupersededByQueue
    } else {
        install_prepared_continuation(state, id, &submission, prepared).await?
    };
    let manual = matches!(kind, QuotaResume::Manual);
    if manual || matches!(outcome, ContinuationOutcome::Stands) {
        if manual && park.cap_reached {
            resume_at = Utc::now();
        }
        state
            .acp_supervisor
            .publish_rate_limit_auto_resumed(id, resume_at, manual)?;
    }
    let handled = match outcome {
        ContinuationOutcome::Stands => {
            state
                .session_service
                .drain_pending_initial_turn_with_submission(id, &submission, Some(&delivery))
                .await
        }
        ContinuationOutcome::SupersededByQueue => {
            state
                .session_service
                .drain_queued_prompts_with_submission(id, &submission, Some(&delivery))
                .await
        }
    };
    if !handled {
        if let Some(unavailable) = state
            .session_service
            .prompt_native_unavailability(id)
            .await
            .map_err(|_| SupervisorError::ControlStateUnavailable)?
        {
            return Err(
                crate::acp::acp_client::AcpError::ConnectionUnavailable(unavailable).into(),
            );
        }
        return Err(SupervisorError::SpawnCancelled(id.to_owned()));
    }
    Ok(())
}

pub(super) async fn schedule_capped_queue(state: &Arc<AppState>, id: &str, park_seq: u64) -> bool {
    let Some(submission) = state.session_service.try_prompt_submission(id).await else {
        return false;
    };
    schedule_quota_release(
        state.clone(),
        id.to_owned(),
        submission,
        QuotaResume::QueuedAtCap { park_seq },
    );
    true
}

fn schedule_quota_release(
    state: Arc<AppState>,
    id: String,
    submission: tokio::sync::OwnedMutexGuard<()>,
    kind: QuotaResume,
) {
    crate::task_util::spawn_supervised(
        "acp.quota_release",
        crate::task_util::PanicPolicy::Log,
        async move {
            if let Err(error) = resume_rate_limited(&state, &id, submission, kind).await {
                tracing::warn!(target: "acp.supervisor", session = %id, %error,
                    "quota continuation remains held");
            }
        },
    );
}

/// Schedule due quota work without waiting for readiness on the global tick.
pub(super) async fn reap_rate_limit_resumes(
    state: &Arc<AppState>,
    attempted: &mut HashSet<String>,
    parked: &HashSet<String>,
) {
    let candidates: Vec<(String, String, bool)> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| is_resumable(i))
            .map(|i| {
                (
                    i.id.clone(),
                    i.source_profile.clone(),
                    !i.queued_prompts.is_empty(),
                )
            })
            .collect()
    };
    let mut eligible = Vec::new();
    for candidate in candidates {
        if parked.contains(&candidate.0) {
            tracing::trace!(target: "acp.supervisor", session = %candidate.0, "rate-limit auto-resume: skipped, session is crash-loop parked");
        } else {
            eligible.push(candidate);
        }
    }
    if eligible.is_empty() {
        return;
    }
    let enabled_by_profile = resolve_per_profile(eligible.iter().map(|c| c.1.clone()), |c| {
        c.acp.rate_limit_auto_resume
    })
    .await;

    let now = Utc::now();
    for (id, profile, has_queued_prompts) in eligible {
        let skip = |reason: &str| {
            tracing::trace!(target: "acp.supervisor", session = %id, "rate-limit auto-resume: skipped, {reason}");
        };
        let Ok(Known(Some(park))) = query_control(
            &state.acp_event_store,
            &id,
            "rate-limit auto-resume",
            |s, id| s.rate_limit_park(id),
        )
        .await
        else {
            continue;
        };
        if park.cap_reached {
            // The held slot keeps the resume pass off; a queued prompt has no other route to a worker.
            if has_queued_prompts {
                if schedule_capped_queue(state, &id, park.seq).await {
                    attempted.remove(&id);
                }
            } else {
                skip("redelivery cap reached and no prompt queued");
            }
            continue;
        }
        if !enabled_by_profile.get(&profile).copied().unwrap_or(false) {
            skip("not enabled for this profile");
            continue;
        }
        // A pruned `RateLimit` row has no reset time and follows the unknown-reset schedule.
        let reset = park.info.as_ref().and_then(|info| info.resets_at);
        let Ok(Known(streak)) = query_control(
            &state.acp_event_store,
            &id,
            "rate-limit redelivery streak",
            |s, id| s.rate_limit_redelivery_streak(id),
        )
        .await
        else {
            continue;
        };
        let mut resume_at = match reset {
            Some(resets_at) => rate_limit_resume_at(
                resets_at,
                park.recorded_at_ms,
                RATE_LIMIT_AUTO_RESUME_GRACE_SECS,
            ),
            None => rate_limit_unknown_reset_retry_at(park.recorded_at_ms, streak),
        };
        // A resume that fired but got no worker retries on the minimum park window.
        if let Some(last_attempt) = park
            .last_resume_attempt_ms
            .and_then(DateTime::from_timestamp_millis)
        {
            resume_at =
                resume_at.max(last_attempt + chrono::Duration::seconds(RATE_LIMIT_MIN_PARK_SECS));
        }
        if now < resume_at {
            skip("park window has not elapsed");
            continue;
        }
        // Contention is a refusal, not a queue: this is one sequential pass over
        // every session and must not stall behind a submission.
        let Some(submission) = state.session_service.try_prompt_submission(&id).await else {
            skip("a prompt submission owns the session");
            continue;
        };
        attempted.remove(&id);
        schedule_quota_release(
            state.clone(),
            id,
            submission,
            QuotaResume::Automatic { park_seq: park.seq },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_fixtures::{
        enable_auto_resume, queued_prompt, startup_errors, test_state, Tick,
    };
    use super::*;
    use crate::acp::event_store::ControlKnowledge::{Indeterminate, Known};
    use crate::acp::supervisor::test_support::managed::with_managed;
    use crate::acp::Event;
    use crate::server::test_support::managed_control_session;
    use chrono::TimeZone;

    fn known_park(state: &AppState, id: &str) -> crate::acp::event_store::RateLimitPark {
        match state
            .acp_event_store
            .rate_limit_park(id)
            .expect("read park")
        {
            Known(Some(park)) => park,
            value => panic!("expected a known park, got {value:?}"),
        }
    }

    fn delivered_prompts(state: &AppState, id: &str, frontier: u64) -> Vec<(String, bool)> {
        state
            .acp_event_store
            .replay_from(id, frontier)
            .expect("read delivered events")
            .iter()
            .filter_map(|stored| match stored.event.as_ref() {
                Event::UserPromptSent {
                    text, synthesized, ..
                } => Some((text.clone(), *synthesized)),
                _ => None,
            })
            .collect()
    }

    fn auto_resumed_breadcrumbs(state: &AppState, id: &str) -> usize {
        state
            .acp_event_store
            .replay_from(id, 0)
            .expect("read resume events")
            .iter()
            .filter(|stored| matches!(stored.event.as_ref(), Event::RateLimitAutoResumed { .. }))
            .count()
    }

    async fn settle_delivery(state: &Arc<AppState>, id: &str) {
        drop(
            tokio::time::timeout(
                Duration::from_secs(5),
                state.session_service.prompt_submission(id),
            )
            .await
            .expect("quota delivery releases submission custody"),
        );
    }

    async fn pending_turn(state: &AppState, id: &str) -> Option<bool> {
        state
            .instances
            .read()
            .await
            .iter()
            .find(|row| row.id == id)
            .and_then(|row| row.pending_initial_turn.as_ref())
            .map(|turn| turn.synthesized)
    }

    struct HiddenBudgetTable(Arc<crate::acp::event_store::EventStore>);
    impl Drop for HiddenBudgetTable {
        fn drop(&mut self) {
            self.0
                .test_conn()
                .execute_batch("ALTER TABLE hidden_quota_budgets RENAME TO acp_rate_limit_budgets")
                .unwrap();
        }
    }

    #[test]
    fn unknown_reset_backs_off_per_redelivery_spent() {
        let recorded_at = Utc.timestamp_opt(1_500_000, 0).unwrap();
        let at =
            |n| rate_limit_unknown_reset_retry_at(recorded_at.timestamp_millis(), n) - recorded_at;
        let hour = chrono::Duration::seconds(RATE_LIMIT_UNKNOWN_RESET_RETRY_SECS);
        for (n, factor) in [(0, 1), (1, 2), (2, 4), (3, 8), (4, 16)] {
            assert_eq!(at(n), hour * factor);
        }
        let total = (0..RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES)
            .fold(chrono::Duration::zero(), |acc, n| acc + at(n));
        assert_eq!(total, hour * 31);
        assert_eq!(at(RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES + 50), hour * 16);

        let recorded = Utc.timestamp_opt(1_000_000, 0).unwrap();
        let ms = recorded.timestamp_millis();
        let secs = chrono::Duration::seconds;
        let cases = [
            (
                "far future reset",
                recorded + chrono::Duration::hours(1),
                15,
                recorded + chrono::Duration::hours(1) + secs(15),
            ),
            (
                "past reset floors",
                recorded - secs(5),
                0,
                recorded + secs(RATE_LIMIT_MIN_PARK_SECS),
            ),
            ("grace above floor", recorded, 120, recorded + secs(120)),
        ];
        for (name, resets_at, grace, expected) in cases {
            assert_eq!(
                rate_limit_resume_at(resets_at, ms, grace),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn only_a_row_minted_after_the_limit_supersedes() {
        let limit = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let limit_ms = limit.timestamp_millis();
        let row = |seq: u64, created_at: &str| crate::daemon::QueuedPromptEntry {
            id: format!("q-{seq}"),
            seq,
            text: String::new(),
            attachments: Vec::new(),
            created_at: created_at.into(),
            origin_device: None,
            origin_plugin: None,
        };
        let after = Utc.timestamp_opt(1_700_000_001, 0).unwrap().to_rfc3339();
        let before = Utc.timestamp_opt(1_699_999_999, 0).unwrap().to_rfc3339();
        let times = |rows: &[crate::daemon::QueuedPromptEntry]| {
            rows.iter().filter_map(row_minted_at_ms).collect::<Vec<_>>()
        };
        let cases = [
            (
                "row after the limit",
                vec![row(1, &after)],
                Some(limit_ms),
                true,
            ),
            (
                "row before the limit",
                vec![row(1, &before)],
                Some(limit_ms),
                false,
            ),
            ("limit never recorded", vec![row(1, &after)], None, false),
            (
                "follow-up queued before the park, replacement after",
                vec![row(1, &before), row(2, &after)],
                Some(limit_ms),
                true,
            ),
        ];
        for (name, rows, recorded, expected) in cases {
            assert_eq!(
                queue_supersedes(&times(&rows), recorded),
                expected,
                "{name}"
            );
        }
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn rate_limit_resume_backs_off_after_a_failed_attempt() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = managed_control_session(scope, "quota-backoff").await;
                let id = live.id().to_owned();
                enable_auto_resume(true);
                live.park("run the nightly task").await;
                let before = live.state.acp_event_store.highest_seq(&id);
                let requests = live.native.requests();
                let submission = live.state.session_service.prompt_submission(&id).await;
                resume_rate_limited(
                    &live.state,
                    &id,
                    submission,
                    QuotaResume::Automatic {
                        park_seq: known_park(&live.state, &id).seq,
                    },
                )
                .await
                .unwrap();
                assert_eq!(live.state.acp_event_store.highest_seq(&id), before);
                assert_eq!(live.native.requests(), requests);
                live.make_quota_due();
                let submission = live.state.session_service.prompt_submission(&id).await;
                resume_rate_limited(
                    &live.state,
                    &id,
                    submission,
                    QuotaResume::Automatic {
                        park_seq: known_park(&live.state, &id).seq,
                    },
                )
                .await
                .unwrap();
                let retried = live.next_prompt().await;
                assert_eq!(
                    retried["params"]["prompt"][0]["text"],
                    "run the nightly task"
                );
                live.finish_prompt(&retried, Some("rate_limit")).await;
                assert_eq!(
                    live.state
                        .acp_event_store
                        .rate_limit_redelivery_streak(&id)
                        .unwrap(),
                    Known(1)
                );
                let frontier = live.state.acp_event_store.highest_seq(&id);
                let mut attempted = HashSet::from([id.clone()]);
                reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()).await;
                settle_delivery(&live.state, &id).await;
                assert!(attempted.contains(&id));
                assert_eq!(live.state.acp_event_store.highest_seq(&id), frontier);
                enable_auto_resume(false);
                live.state
                    .acp_supervisor
                    .publish_startup_error(&id, "spawn failed once".into());
                let errors = startup_errors(&live.state, &id);
                Tick::default().run(&live.state).await;
                assert_eq!(startup_errors(&live.state, &id), errors);
                assert!(matches!(
                    live.state.acp_event_store.rate_limit_park(&id).unwrap(),
                    Known(Some(_))
                ));
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rate_limit_reap_resumes_below_the_cap_and_parks_at_it() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = managed_control_session(scope, "quota-cap").await;
                let id = live.id().to_owned();
                let owner = live
                    .state
                    .acp_supervisor
                    .current_native_availability(&id)
                    .await
                    .unwrap()
                    .owner;
                enable_auto_resume(true);
                live.park("run the nightly task").await;
                for spent in 0..RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES {
                    live.make_quota_due();
                    let mut attempted = HashSet::from([id.clone()]);
                    reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()).await;
                    settle_delivery(&live.state, &id).await;
                    let request = live.next_prompt().await;
                    assert_eq!(request["params"]["sessionId"], live.sid);
                    assert_eq!(
                        request["params"]["prompt"][0]["text"],
                        "run the nightly task"
                    );
                    live.finish_prompt(&request, Some("rate_limit")).await;
                    assert_eq!(
                        live.state
                            .acp_event_store
                            .rate_limit_redelivery_streak(&id)
                            .unwrap(),
                        Known(spent + 1)
                    );
                }
                live.make_quota_due();
                let frontier = live.state.acp_event_store.highest_seq(&id);
                let held = live.state.session_service.prompt_submission(&id).await;
                let mut attempted = HashSet::from([id.clone()]);
                tokio::time::timeout(
                    Duration::from_secs(5),
                    reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()),
                )
                .await
                .unwrap();
                assert!(attempted.contains(&id));
                assert_eq!(live.state.acp_event_store.highest_seq(&id), frontier);
                drop(held);
                reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()).await;
                settle_delivery(&live.state, &id).await;
                assert!(!live.state.acp_supervisor.publish_stopped_if_seq(
                    &id,
                    RATE_LIMIT_EXHAUSTED_RETRIES_REASON,
                    frontier.saturating_sub(1)
                ));
                let cap = known_park(&live.state, &id);
                assert!(cap.cap_reached);
                assert_eq!(pending_turn(&live.state, &id).await, None);
                assert!(delivered_prompts(&live.state, &id, frontier).is_empty());
                assert_eq!(
                    live.state
                        .acp_event_store
                        .rate_limit_redelivery_streak(&id)
                        .unwrap(),
                    Known(0)
                );
                enable_auto_resume(false);
                let cap_frontier = live.state.acp_event_store.highest_seq(&id);
                reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()).await;
                settle_delivery(&live.state, &id).await;
                assert_eq!(live.state.acp_event_store.highest_seq(&id), cap_frontier);
                live.state
                    .session_service
                    .enqueue_prompt(
                        &id,
                        "after-cap".into(),
                        "new user work".into(),
                        Vec::new(),
                        None,
                        None,
                    )
                    .await
                    .unwrap();
                reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()).await;
                settle_delivery(&live.state, &id).await;
                let queued = live.next_prompt().await;
                assert_eq!(queued["params"]["prompt"][0]["text"], "new user work");
                assert_eq!(
                    auto_resumed_breadcrumbs(&live.state, &id),
                    RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES as usize
                );
                live.finish_prompt(&queued, None).await;
                assert_eq!(
                    live.state
                        .acp_supervisor
                        .current_native_availability(&id)
                        .await
                        .unwrap()
                        .owner,
                    owner
                );
                assert_eq!(
                    live.native
                        .requests()
                        .iter()
                        .filter(|method| *method == "session/new")
                        .count(),
                    1
                );
                assert!(!live
                    .native
                    .requests()
                    .iter()
                    .any(|method| method == "session/load"));
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn only_a_prompt_queued_after_the_park_supersedes_the_continuation() {
        with_managed(|scope| {
            Box::pin(async move {
                for (label, installed, offset, expect_queue) in [
                    ("queued-after", false, Some(1000_i64), true),
                    ("installed-and-queued-after", true, Some(1000), true),
                    ("queued-before", false, Some(-1000), false),
                    ("queued-unreadable", false, None, false),
                ] {
                    let mut live = managed_control_session(scope, label).await;
                    let id = live.id().to_owned();
                    enable_auto_resume(true);
                    live.park("run the nightly task").await;
                    live.make_quota_due();
                    if installed {
                        live.state
                            .session_service
                            .set_pending_initial_turn(
                                &id,
                                "run the nightly task".into(),
                                Vec::new(),
                            )
                            .await;
                    }
                    let park = known_park(&live.state, &id);
                    let mut queued = queued_prompt();
                    queued.text = "manual prompt B".into();
                    queued.created_at = offset
                        .map(|offset| {
                            DateTime::from_timestamp_millis(park.recorded_at_ms + offset)
                                .unwrap()
                                .to_rfc3339()
                        })
                        .unwrap_or_else(|| "unreadable".into());
                    live.state
                        .instances
                        .write()
                        .await
                        .iter_mut()
                        .find(|row| row.id == id)
                        .unwrap()
                        .queued_prompts
                        .push(queued);
                    let frontier = live.state.acp_event_store.highest_seq(&id);
                    let mut attempted = HashSet::from([id.clone()]);
                    reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()).await;
                    settle_delivery(&live.state, &id).await;
                    let request = live.next_prompt().await;
                    let expected = if expect_queue {
                        "manual prompt B"
                    } else {
                        "run the nightly task"
                    };
                    assert_eq!(request["params"]["prompt"][0]["text"], expected, "{label}");
                    assert_eq!(
                        delivered_prompts(&live.state, &id, frontier),
                        vec![(expected.into(), !expect_queue)]
                    );
                    assert_eq!(
                        auto_resumed_breadcrumbs(&live.state, &id),
                        usize::from(!expect_queue)
                    );
                    assert_eq!(pending_turn(&live.state, &id).await, None);
                    assert!(!attempted.contains(&id));
                    live.finish_prompt(&request, None).await;
                }
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unavailable_control_queries_preserve_intent_and_refuse_automation() {
        with_managed(|scope| Box::pin(async move {
            for fault in ["sql", "ownership", "budget"] {
                let mut live = managed_control_session(scope, fault).await;
                let id = live.id().to_owned();
                enable_auto_resume(true);
                live.park("original interrupted work").await;
                live.make_quota_due();
                live.state.session_service.set_pending_initial_turn(&id, "original interrupted work".into(), Vec::new()).await;
                let park = known_park(&live.state, &id);
                let before = live.state.instances.read().await.iter().find(|row| row.id == id).unwrap().clone();
                let requests = live.native.requests();
                let frontier = live.state.acp_event_store.highest_seq(&id);
                let hidden = if fault == "sql" {
                    live.state.acp_event_store.test_conn().execute_batch("ALTER TABLE acp_rate_limit_budgets RENAME TO hidden_quota_budgets").unwrap();
                    Some(HiddenBudgetTable(live.state.acp_event_store.clone()))
                } else {
                    if fault == "ownership" {
                        live.state.acp_event_store.test_conn().execute("DELETE FROM acp_events WHERE session_id=?1 AND seq<?2",
                            rusqlite::params![id, i64::try_from(park.seq).unwrap()]).unwrap();
                    } else {
                        live.state.acp_event_store.test_conn().execute("UPDATE acp_rate_limit_budgets SET known=0 WHERE session_id=?1", [&id]).unwrap();
                    }
                    None
                };
                live.state.acp_event_store.control_cache().forget(&id);
                if fault == "sql" {
                    assert!(live.state.acp_event_store.rate_limit_park(&id).is_err());
                } else if fault == "ownership" {
                    assert!(matches!(live.state.acp_event_store.rate_limit_park(&id).unwrap(), Indeterminate));
                } else {
                    assert_eq!(live.state.acp_event_store.rate_limit_redelivery_streak(&id).unwrap(), Indeterminate);
                }
                if fault != "budget" {
                    let submission = live.state.session_service.prompt_submission(&id).await;
                    assert!(install_rate_limit_continuation(&live.state, &id, &submission).await.is_err());
                    drop(submission);
                }
                let submission = live.state.session_service.prompt_submission(&id).await;
                assert!(resume_rate_limited(&live.state, &id, submission, QuotaResume::Automatic { park_seq: park.seq }).await.is_err());
                let submission = live.state.session_service.prompt_submission(&id).await;
                assert!(resume_rate_limited(&live.state, &id, submission, QuotaResume::Manual).await.is_err(),
                    "RESUME NOW repeats old work; it is not distinct explicit input after readiness");
                let mut attempted = HashSet::from([id.clone()]);
                reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()).await;
                settle_delivery(&live.state, &id).await;
                assert_eq!(live.state.acp_event_store.highest_seq(&id), frontier);
                assert_eq!(live.native.requests(), requests);
                let after = live.state.instances.read().await.iter().find(|row| row.id == id).unwrap().clone();
                assert_eq!(serde_json::to_value(&after.pending_initial_turn).unwrap(), serde_json::to_value(&before.pending_initial_turn).unwrap());
                assert_eq!(serde_json::to_value(&after.queued_prompts).unwrap(), serde_json::to_value(&before.queued_prompts).unwrap());
                assert_eq!(auto_resumed_breadcrumbs(&live.state, &id), 0);
                drop(hidden);
                live.state.acp_event_store.control_cache().forget(&id);
                // Retire this original while its Native request lane still has
                // an observer. Later fixture creation must not revive its intent.
                live.client.stop_session().await;
            }
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn retained_unknown_history_allows_only_read_only_attach_then_distinct_explicit_input() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = managed_control_session(scope, "quota-unknown-retained").await;
                let id = live.id().to_owned();
                live.park("old work must not replay").await;
                let original = live
                    .state
                    .acp_supervisor
                    .current_native_availability(&id)
                    .await
                    .unwrap();
                let park = known_park(&live.state, &id);
                live.state
                    .acp_event_store
                    .test_conn()
                    .execute(
                        "DELETE FROM acp_events WHERE session_id=?1 AND seq<?2",
                        rusqlite::params![id, i64::try_from(park.seq).unwrap()],
                    )
                    .unwrap();
                live.state.acp_event_store.control_cache().forget(&id);
                assert!(matches!(
                    live.state.acp_event_store.rate_limit_park(&id).unwrap(),
                    Indeterminate
                ));
                let durable_budget = live
                    .state
                    .acp_event_store
                    .rate_limit_redelivery_streak(&id)
                    .unwrap();
                assert_eq!(durable_budget, Known(0));
                live.client = scope.adopt(live.client, false).await;
                live.client.ready().await;
                live.state = live.client.app_state();
                let attached = live
                    .state
                    .acp_supervisor
                    .current_native_availability(&id)
                    .await
                    .unwrap();
                assert_eq!(attached.owner, original.owner);
                assert_eq!(
                    attached.client.connected_runner(),
                    original.client.connected_runner()
                );
                assert!(matches!(
                    live.state.acp_event_store.rate_limit_park(&id).unwrap(),
                    Indeterminate
                ));
                assert_eq!(
                    live.state
                        .acp_event_store
                        .rate_limit_redelivery_streak(&id)
                        .unwrap(),
                    durable_budget
                );
                let frontier = live.state.acp_event_store.highest_seq(&id);
                let mut attempted = HashSet::from([id.clone()]);
                reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()).await;
                assert_eq!(live.state.acp_event_store.highest_seq(&id), frontier);
                assert_eq!(auto_resumed_breadcrumbs(&live.state, &id), 0);
                let request = live
                    .start_prompt("a distinct explicit input after readiness")
                    .await;
                assert_eq!(request["params"]["sessionId"], live.sid);
                assert_eq!(
                    request["params"]["prompt"][0]["text"],
                    "a distinct explicit input after readiness"
                );
                assert_eq!(
                    delivered_prompts(&live.state, &id, frontier),
                    vec![("a distinct explicit input after readiness".into(), false)]
                );
                live.finish_prompt(&request, None).await;
                assert_eq!(
                    live.state
                        .acp_event_store
                        .has_in_flight_main_turn(&id)
                        .unwrap(),
                    Known(false)
                );
                assert_eq!(
                    live.state
                        .acp_event_store
                        .rate_limit_redelivery_streak(&id)
                        .unwrap(),
                    Known(0)
                );
                assert_eq!(
                    live.native
                        .requests()
                        .iter()
                        .filter(|method| *method == "session/new")
                        .count(),
                    1
                );
                assert!(!live
                    .native
                    .requests()
                    .iter()
                    .any(|method| method == "session/load"));
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn qualified_native_loss_preserves_quota_pending_queue_and_budget_before_delivery() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = managed_control_session(scope, "quota-native-loss").await;
                let id = live.id().to_owned();
                live.park("original work").await;
                live.state
                    .session_service
                    .set_pending_initial_turn(&id, "original work".into(), Vec::new())
                    .await;
                let before = live.state.acp_event_store.highest_seq(&id);
                let budget = live
                    .state
                    .acp_event_store
                    .rate_limit_redelivery_streak(&id)
                    .unwrap();
                let requests = live.native.requests();
                live.client.actual().test_observe_native_loss();
                for automatic in [false, true] {
                    let submission = live.state.session_service.prompt_submission(&id).await;
                    let kind = if automatic {
                        QuotaResume::Automatic {
                            park_seq: known_park(&live.state, &id).seq,
                        }
                    } else {
                        QuotaResume::Manual
                    };
                    assert!(matches!(
                        resume_rate_limited(&live.state, &id, submission, kind).await,
                        Err(SupervisorError::Acp(
                            crate::acp::acp_client::AcpError::ConnectionUnavailable(_)
                        ))
                    ));
                    assert_eq!(live.state.acp_event_store.highest_seq(&id), before);
                    assert_eq!(
                        live.state
                            .acp_event_store
                            .rate_limit_redelivery_streak(&id)
                            .unwrap(),
                        budget
                    );
                    assert_eq!(pending_turn(&live.state, &id).await, Some(true));
                    assert_eq!(live.native.requests(), requests);
                }
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn an_aged_quota_hold_prevents_idle_reaping_without_spending_retry_budget() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = managed_control_session(scope, "quota-idle-hold").await;
                let id = live.id().to_owned();
                live.park("original work").await;
                std::fs::write(
                    crate::session::get_app_dir().unwrap().join("config.toml"),
                    "[acp]\nrate_limit_auto_resume = true\nauto_stop_idle_secs = 1\n",
                )
                .unwrap();
                live.client.lease.test_backdate_idle_baseline(
                    (Utc::now() - chrono::Duration::days(1)).timestamp_millis(),
                );
                live.make_quota_due();
                let frontier = live.state.acp_event_store.highest_seq(&id);
                let requests = live.native.requests();
                super::super::idle::reap_idle_workers(&live.state).await;
                assert_eq!(live.state.acp_event_store.highest_seq(&id), frontier);
                assert_eq!(live.native.requests(), requests);
                assert_eq!(
                    live.state.acp_supervisor.current_lease(&id),
                    Some(live.client.lease.clone())
                );
                assert!(!live
                    .state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .is_idle_dormant());
                assert_eq!(
                    live.state
                        .acp_event_store
                        .rate_limit_redelivery_streak(&id)
                        .unwrap(),
                    Known(0)
                );
            })
        })
        .await;
    }

    #[tokio::test]
    async fn known_agent_turn_selects_continue_but_unknown_history_does_not() {
        use crate::acp::event_sender::EventMetadata;
        let (_home, state, _project) = test_state("agent-projection-data-only");
        let id = "agent-projection-data-only";
        let mut metadata = crate::server::test_support::sql_projection_metadata();
        let body = Event::AgentMessageChunk {
            text: "agent initiated".into(),
            message_id: None,
        };
        state
            .acp_event_store
            .record(id, 1, &Event::ThinkingStarted, &metadata)
            .unwrap();
        state
            .acp_event_store
            .record(id, 2, &body, &metadata)
            .unwrap();
        metadata.operation.as_mut().unwrap().outcome =
            Some(crate::acp::protocol::AcpOriginalRequestResult::Failed);
        metadata.prompt_observation = Some(crate::acp::protocol::AcpPromptObservation::Written {
            window_id: crate::acp::protocol::AcpPromptWindowId::new(),
        });
        let quota = Event::RateLimit {
            info: crate::acp::state::RateLimitInfo::undated(),
        };
        state
            .acp_event_store
            .record(id, 3, &quota, &metadata)
            .unwrap();
        let stopped = Event::Stopped {
            reason: "rate_limited".into(),
        };
        state
            .acp_event_store
            .record(id, 4, &stopped, &metadata)
            .unwrap();
        assert!(matches!(
            state.acp_event_store.rate_limited_turn_prompt(id).unwrap(),
            Known(None)
        ));
        assert_eq!(
            state
                .acp_event_store
                .rate_limit_interrupted_agent_turn(id)
                .unwrap(),
            Known(true)
        );
        let (text, attachments) = resolve_continuation(&state, id)
            .await
            .unwrap()
            .expect("known agent continuation");
        assert_eq!(text, AGENT_TURN_CONTINUATION);
        assert!(attachments.is_empty());
        state
            .acp_event_store
            .test_conn()
            .execute("DELETE FROM acp_events WHERE session_id=?1 AND seq<3", [id])
            .unwrap();
        state.acp_event_store.control_cache().forget(id);
        assert!(matches!(
            resolve_continuation(&state, id).await,
            Err(SupervisorError::ControlStateUnavailable)
        ));
        let host_stop = Event::Stopped {
            reason: "explicit_stop".into(),
        };
        state
            .acp_event_store
            .record(id, 5, &host_stop, &EventMetadata::host(&host_stop))
            .unwrap();
        assert!(resolve_continuation(&state, id).await.unwrap().is_none());
        assert_eq!(
            state
                .acp_event_store
                .rate_limit_redelivery_streak(id)
                .unwrap(),
            Known(0)
        );
        state.shutdown.cancel();
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn selected_quota_queue_delivers_stored_bytes_without_owning_historical_background_resources(
    ) {
        use base64::Engine as _;
        with_managed(|scope| {
            Box::pin(async move {
                for automatic in [false, true] {
                    let mut live = managed_control_session(
                        scope,
                        if automatic {
                            "quota-background-auto"
                        } else {
                            "quota-background-manual"
                        },
                    )
                    .await;
                    let id = live.id().to_owned();
                    enable_auto_resume(automatic);
                    live.park("older interrupted input").await;
                    live.make_quota_due();
                    let park = known_park(&live.state, &id);
                    let history = Event::BackgroundAgentLaunched {
                        agent_id: "historical-background".into(),
                        tool_call_id: "historical-tool".into(),
                        description: "retained transcript".into(),
                        prompt: "historical work".into(),
                        model: "test".into(),
                        output_file: String::new(),
                        workflow: None,
                        started_at: Utc::now(),
                    };
                    live.state
                        .acp_event_store
                        .record(
                            &id,
                            live.state.acp_event_store.highest_seq(&id) + 1,
                            &history,
                            &crate::acp::event_sender::EventMetadata::history(),
                        )
                        .unwrap();
                    live.state
                        .acp_supervisor
                        .hydrate_seqs(live.state.acp_event_store.all_session_seqs().unwrap());
                    let control = live
                        .state
                        .session_service
                        .fold_control_state(&id)
                        .await
                        .unwrap();
                    assert!(control.has_active_background_agent());
                    assert!(control
                        .background_agents
                        .iter()
                        .all(|agent| agent.resource_ref.is_none()));
                    assert_eq!(
                        live.state
                            .acp_event_store
                            .has_in_flight_main_turn(&id)
                            .unwrap(),
                        Known(false)
                    );
                    assert_eq!(
                        live.state.acp_event_store.has_in_flight_turn(&id).unwrap(),
                        Known(true)
                    );
                    let bytes = [
                        b"original-old-image".to_vec(),
                        b"original-new-image".to_vec(),
                    ];
                    for (index, data) in bytes.iter().enumerate() {
                        let mut queued = queued_prompt();
                        queued.id = format!("queued-{index}");
                        queued.seq = index as u64 + 1;
                        queued.text = if index == 0 {
                            "older queued intent"
                        } else {
                            "newer queued intent"
                        }
                        .into();
                        queued.created_at = DateTime::from_timestamp_millis(
                            park.recorded_at_ms + if index == 0 { -1000 } else { 1000 },
                        )
                        .unwrap()
                        .to_rfc3339();
                        queued.attachments = vec![crate::daemon::PromptAttachmentRef {
                            id: format!("image-{index}"),
                            kind: crate::daemon::PromptAttachmentKind::Image,
                            mime_type: "image/png".into(),
                            name: Some(format!("original-{index}.png")),
                            size: data.len() as u64,
                        }];
                        assert!(live.state.acp_event_store.record_pending_attachment(
                            &id,
                            &queued.id,
                            &crate::acp::event_store::AttachmentBlob {
                                id: format!("image-{index}"),
                                kind: crate::daemon::PromptAttachmentKind::Image,
                                mime_type: "image/png".into(),
                                name: Some(format!("original-{index}.png")),
                                data: data.clone(),
                            }
                        ));
                        live.state
                            .instances
                            .write()
                            .await
                            .iter_mut()
                            .find(|row| row.id == id)
                            .unwrap()
                            .queued_prompts
                            .push(queued);
                    }
                    let frontier = live.state.acp_event_store.highest_seq(&id);
                    live.state
                        .session_service
                        .drain_queued_prompts_once(&id)
                        .await;
                    assert_eq!(live.state.acp_event_store.highest_seq(&id), frontier);
                    let submission = live.state.session_service.prompt_submission(&id).await;
                    let kind = if automatic {
                        QuotaResume::Automatic { park_seq: park.seq }
                    } else {
                        QuotaResume::Manual
                    };
                    resume_rate_limited(&live.state, &id, submission, kind)
                        .await
                        .unwrap();
                    let request = live.next_prompt().await;
                    assert_eq!(request["params"]["sessionId"], live.sid);
                    let blocks = request["params"]["prompt"].as_array().unwrap();
                    let text = blocks
                        .iter()
                        .filter(|block| block["type"] == "text")
                        .map(|block| block["text"].as_str().unwrap())
                        .collect::<Vec<_>>()
                        .join("");
                    assert_eq!(text, "older queued intent\n\nnewer queued intent");
                    let images = blocks
                        .iter()
                        .filter(|block| block["type"] == "image")
                        .map(|block| {
                            base64::engine::general_purpose::STANDARD
                                .decode(block["data"].as_str().unwrap())
                                .unwrap()
                        })
                        .collect::<Vec<_>>();
                    assert_eq!(images, bytes);
                    assert_eq!(
                        delivered_prompts(&live.state, &id, frontier),
                        vec![(text, false)]
                    );
                    assert_eq!(
                        auto_resumed_breadcrumbs(&live.state, &id),
                        usize::from(!automatic)
                    );
                    assert!(live
                        .state
                        .session_service
                        .fold_control_state(&id)
                        .await
                        .unwrap()
                        .has_active_background_agent());
                    live.finish_prompt(&request, None).await;
                }
            })
        })
        .await;
    }
}
