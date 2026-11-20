//! Opt-in rate-limit auto-resume (#1722) with a bounded redelivery budget (#3688).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::{is_resumable, query_store, resolve_per_profile, AppState};
use crate::acp::state::{RateLimitInfo, RATE_LIMIT_EXHAUSTED_RETRIES_REASON};

mod failover;
#[cfg(test)]
pub(crate) use failover::in_flight as failover_in_flight;
pub(crate) use failover::{pending_target as failover_target, Settings as FailoverSettings};

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

/// When a park's window is expected to end: the reported reset plus grace, else the
/// unknown-reset schedule for the redeliveries spent.
fn scheduled_resume_at(
    info: &crate::acp::state::RateLimitInfo,
    recorded_at_ms: i64,
    streak: i64,
) -> DateTime<Utc> {
    match info.resets_at {
        Some(resets_at) => {
            rate_limit_resume_at(resets_at, recorded_at_ms, RATE_LIMIT_AUTO_RESUME_GRACE_SECS)
        }
        None => rate_limit_unknown_reset_retry_at(recorded_at_ms, streak),
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
    /// This call installed the interrupted prompt as the next turn. Licenses
    /// the automatic `RateLimitAutoResumed`, like [`Self::Stands`].
    Installed,
    /// The continuation is already in place, a handoff owns the session, or
    /// there was nothing to replay. Licenses the automatic
    /// `RateLimitAutoResumed`, which is the budget's arming step.
    Stands,
    /// A queued prompt owns the next turn, and any continuation an earlier
    /// cadence installed is cleared. That prompt has no other route to a
    /// worker, so the caller still frees the respawn, but no automatic
    /// breadcrumb.
    SupersededByQueue,
}

/// Select interrupted work under the caller's submission ownership.
pub(crate) async fn install_rate_limit_continuation(
    state: &Arc<AppState>,
    id: &str,
    _submission: &tokio::sync::OwnedMutexGuard<()>,
) -> ContinuationOutcome {
    if state
        .instances
        .read()
        .await
        .iter()
        .any(|row| row.id == id && row.terminal_handoff.is_some())
    {
        return ContinuationOutcome::Stands;
    }
    let Some(interrupted) = query_store(
        &state.acp_event_store,
        id,
        "rate-limit continuation",
        |s, id| s.rate_limited_turn_prompt(id),
    )
    .await
    else {
        return ContinuationOutcome::Stands;
    };
    let (text, attachments) = match interrupted {
        Some(prompt) => prompt,
        // Nothing to redeliver: an agent-initiated turn is continued the way
        // a user would, with a bare prompt (#4373).
        None => {
            let agent_turn = query_store(
                &state.acp_event_store,
                id,
                "rate-limit agent-turn continuation",
                |s, id| s.rate_limit_interrupted_agent_turn(id),
            )
            .await;
            if agent_turn != Some(true) {
                return ContinuationOutcome::Stands;
            }
            (AGENT_TURN_CONTINUATION.to_string(), Vec::new())
        }
    };
    #[cfg(test)]
    state.session_service.await_install_barrier(id).await;
    // A queued prompt publishes no event, so its row is the only record of a
    // supersession. Only the mint times leave the read guard: a row carries its
    // whole text, and copying that under the shared lock is not worth one
    // timestamp.
    let minted_at_ms: Vec<i64> = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return ContinuationOutcome::Stands;
        };
        inst.queued_prompts
            .iter()
            .filter_map(row_minted_at_ms)
            .collect()
    };
    if !minted_at_ms.is_empty() {
        // `None` covers a pruned limit row and a failed probe alike, and
        // neither proves a supersession, so the continuation stands.
        let limit_at_ms = query_store(
            &state.acp_event_store,
            id,
            "rate-limit supersession",
            |s, id| {
                s.latest_rate_limit_event(id)
                    .map(|(_, recorded_at_ms)| recorded_at_ms)
            },
        )
        .await
        .flatten();
        if queue_supersedes(&minted_at_ms, limit_at_ms) {
            // `/queue` does not clear the interrupted-turn slot.
            state.session_service.clear_pending_initial_turn(id).await;
            return ContinuationOutcome::SupersededByQueue;
        }
    }
    state
        .session_service
        .set_pending_initial_turn(id, text, attachments)
        .await;
    ContinuationOutcome::Installed
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
    LoginQueue { park_seq: u64 },
}

pub(crate) async fn resume_rate_limited(
    state: &Arc<AppState>,
    id: &str,
    submission: tokio::sync::OwnedMutexGuard<()>,
    kind: QuotaResume,
) -> Result<(), crate::acp::supervisor::SupervisorError> {
    use crate::acp::supervisor::SupervisorError;
    let Some(park) = query_store(&state.acp_event_store, id, "quota release", |store, id| {
        store.rate_limit_park(id)
    })
    .await
    .ok_or(SupervisorError::NativeStoreUnavailable)?
    else {
        return Ok(());
    };
    match kind {
        QuotaResume::Automatic { park_seq } if park.seq != park_seq || park.cap_reached => {
            return Ok(())
        }
        QuotaResume::QueuedAtCap { park_seq } if park.seq != park_seq || !park.cap_reached => {
            return Ok(())
        }
        QuotaResume::LoginQueue { park_seq } if park.seq != park_seq => return Ok(()),
        _ => {}
    }
    let profile = {
        let rows = state.instances.read().await;
        rows.iter()
            .find(|row| row.id == id && is_resumable(row))
            .map(|row| row.source_profile.clone())
            .ok_or_else(|| SupervisorError::SpawnCancelled(id.to_owned()))?
    };
    if let Some(unavailable) = state.session_service.prompt_native_unavailability(id).await {
        return Err(crate::acp::acp_client::AcpError::ConnectionUnavailable(unavailable).into());
    }
    let mut resume_at = park
        .info
        .as_ref()
        .and_then(|info| info.resets_at)
        .unwrap_or_else(Utc::now);
    if matches!(kind, QuotaResume::Automatic { .. }) {
        let settings = resolve_per_profile([profile.clone()].into_iter(), |config| {
            (
                config.acp.rate_limit_auto_resume,
                failover::Settings::of(config),
            )
        })
        .await;
        let (enabled, failover_settings) = settings.get(&profile).cloned().unwrap_or_default();
        if !enabled {
            return Ok(());
        }
        let streak = query_store(
            &state.acp_event_store,
            id,
            "quota release budget",
            |store, id| store.rate_limit_redelivery_streak(id),
        )
        .await
        .ok_or(SupervisorError::NativeStoreUnavailable)?;
        let info = park.info.clone().unwrap_or_else(RateLimitInfo::undated);
        let reported = if failover_settings.enabled {
            let row = {
                let rows = state.instances.read().await;
                rows.iter()
                    .find(|row| row.id == id && is_resumable(row))
                    .cloned()
            };
            match row {
                Some(row) => failover::reported_deadline(state, &row, &info).await,
                None => return Err(SupervisorError::SpawnCancelled(id.to_owned())),
            }
        } else {
            None
        };
        resume_at =
            reported.unwrap_or_else(|| scheduled_resume_at(&info, park.recorded_at_ms, streak));
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
    super::resume::ensure_rate_limit_worker(state, id, authority).await?;
    let control = state.session_service.fold_control_state(id).await;
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
    let current = query_store(
        &state.acp_event_store,
        id,
        "quota release recheck",
        |store, id| store.rate_limit_park(id),
    )
    .await
    .ok_or(SupervisorError::NativeStoreUnavailable)?;
    if !current.is_some_and(|current| current.seq == park.seq) {
        return Ok(());
    }
    let delivery = QuotaDelivery { park_seq: park.seq };
    let outcome = if matches!(kind, QuotaResume::LoginQueue { .. }) {
        if state
            .instances
            .read()
            .await
            .iter()
            .any(|row| row.id == id && row.pending_initial_turn.is_some())
        {
            ContinuationOutcome::Stands
        } else {
            ContinuationOutcome::SupersededByQueue
        }
    } else if matches!(kind, QuotaResume::QueuedAtCap { .. }) {
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
        install_rate_limit_continuation(state, id, &submission).await
    };
    let manual = matches!(kind, QuotaResume::Manual);
    // Both admitted continuation outcomes arm the budget before Native input.
    if manual
        || matches!(
            outcome,
            ContinuationOutcome::Stands | ContinuationOutcome::Installed
        )
    {
        if manual && park.cap_reached {
            resume_at = Utc::now();
        }
        state
            .acp_supervisor
            .publish_rate_limit_auto_resumed(id, resume_at, manual)?;
    }
    let handled = match outcome {
        ContinuationOutcome::Stands | ContinuationOutcome::Installed => {
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
        if let Some(unavailable) = state.session_service.prompt_native_unavailability(id).await {
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

pub(super) fn schedule_quota_release(
    state: Arc<AppState>,
    id: String,
    submission: tokio::sync::OwnedMutexGuard<()>,
    kind: QuotaResume,
) {
    crate::task_util::spawn_supervised(
        "acp.quota_release",
        crate::task_util::PanicPolicy::Log,
        async move {
            let login_release = matches!(kind, QuotaResume::LoginQueue { .. });
            if let Err(error) = resume_rate_limited(&state, &id, submission, kind).await {
                tracing::warn!(target: "acp.supervisor", session = %id, %error,
                    "quota continuation remains held");
            } else if login_release {
                state.session_service.sandbox_login.release(&id);
            }
        },
    );
}

/// Schedule due quota work without waiting for readiness on the global tick.
pub(crate) async fn reap_rate_limit_resumes(
    state: &Arc<AppState>,
    attempted: &mut HashSet<String>,
    parked: &HashSet<String>,
) {
    let candidates: Vec<crate::session::Instance> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| is_resumable(i))
            .cloned()
            .collect()
    };
    let mut eligible = Vec::new();
    for candidate in candidates {
        if parked.contains(&candidate.id) {
            tracing::trace!(target: "acp.supervisor", session = %candidate.id, "rate-limit auto-resume: skipped, session is crash-loop parked");
        } else {
            eligible.push(candidate);
        }
    }
    if eligible.is_empty() {
        return;
    }
    let settings_by_profile =
        resolve_per_profile(eligible.iter().map(|row| row.source_profile.clone()), |c| {
            (c.acp.rate_limit_auto_resume, failover::Settings::of(c))
        })
        .await;

    let now = Utc::now();
    for row in eligible {
        let id = row.id.clone();
        let has_queued_prompts = !row.queued_prompts.is_empty();
        if state.session_service.sandbox_login.is_held(&id)
            || crate::process::worker_registry::stop_requested(&id)
        {
            continue;
        }
        let skip = |reason: &str| {
            tracing::trace!(target: "acp.supervisor", session = %id, "rate-limit auto-resume: skipped, {reason}");
        };
        if state
            .session_service
            .prompt_native_unavailability(&id)
            .await
            .is_some()
        {
            skip("native ownership is unavailable");
            continue;
        }
        let workerless = !state.acp_supervisor.is_running(&id).await;
        let (auto_resume, failover_settings) = settings_by_profile
            .get(&row.source_profile)
            .cloned()
            .unwrap_or_default();
        if workerless && failover_settings.enabled {
            failover::note_auth_failure(state, &row).await;
        }
        let Some(park) = query_store(
            &state.acp_event_store,
            &id,
            "rate-limit auto-resume",
            |s, id| s.rate_limit_park(id),
        )
        .await
        else {
            continue;
        };
        let Some(park) = park else {
            skip("session is not parked on a rate limit");
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
        let info = park.info.clone().unwrap_or_else(RateLimitInfo::undated);
        let Some(streak) = query_store(
            &state.acp_event_store,
            &id,
            "rate-limit redelivery streak",
            |s, id| s.rate_limit_redelivery_streak(id),
        )
        .await
        else {
            continue;
        };
        let reported = if failover_settings.enabled {
            failover::reported_deadline(state, &row, &info).await
        } else {
            None
        };
        let scheduled_at =
            reported.unwrap_or_else(|| scheduled_resume_at(&info, park.recorded_at_ms, streak));
        // A park a swap or resume already touched belongs to the account it ran on then.
        let first_sight = park.last_resume_attempt_ms.is_none();
        if workerless && failover_settings.enabled && first_sight && reported.is_none() {
            failover::record_limit(state, &row, &info, scheduled_at, park.recorded_at_ms).await;
        }
        let Some(latest_seq) = query_store(
            &state.acp_event_store,
            &id,
            "rate-limit latest-seq",
            |s, id| s.highest_seq(id),
        )
        .await
        else {
            continue;
        };
        // Below the redelivery cap only: an entirely limited pool finds no target and
        // falls through to the timed resume, whose budget ends in the terminal park.
        if workerless
            && failover_settings.enabled
            && first_sight
            && streak < RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES
            && failover::maybe_fail_over(
                state,
                &row,
                &failover_settings.accounts,
                latest_seq,
                park.recorded_at_ms,
            )
            .await
        {
            skip("an account failover owns this park");
            continue;
        }
        if !auto_resume {
            skip("not enabled for this profile");
            continue;
        }
        let mut resume_at = scheduled_at;
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
    use crate::acp::Event;
    use chrono::TimeZone;

    /// #3152 / #3688: no reported reset still retries, doubling per redelivery.
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

    /// A session parked on an elapsed limit with `redeliveries` resume cycles
    /// behind it; events are backdated an hour. Its pending-turn slot is empty.
    async fn parked(
        id: &str,
        redeliveries: usize,
    ) -> (
        crate::session::test_support::AppDirGuard,
        Arc<AppState>,
        tempfile::TempDir,
    ) {
        let (home, state, project) = test_state(id);
        enable_auto_resume(true);
        let store = &state.acp_event_store;
        let long_ago = Utc::now() - chrono::Duration::hours(1);
        let rate_limit = || Event::RateLimit {
            info: crate::acp::state::RateLimitInfo {
                status: "limited".into(),
                resets_at: Some(long_ago),
                kind: "usage".into(),
            },
        };
        let prompt = || Event::UserPromptSent {
            merged_prompt_ids: Vec::new(),
            text: "run the nightly task".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: false,
            origin_plugin: None,
        };
        let stopped = || Event::Stopped {
            reason: "rate_limited".into(),
        };
        let mut events = vec![prompt(), rate_limit(), stopped()];
        for _ in 0..redeliveries {
            events.push(Event::RateLimitAutoResumed {
                resets_at: long_ago,
                manual: false,
            });
            events.extend([prompt(), rate_limit(), stopped()]);
        }
        for (seq, event) in events.iter().enumerate() {
            store
                .record_at(id, seq as u64 + 1, event, long_ago.timestamp_millis())
                .unwrap();
        }
        state
            .acp_supervisor
            .hydrate_seqs([(id.to_string(), store.highest_seq(id))]);
        (home, state, project)
    }

    /// [`parked`] with the interrupted prompt already installed as the
    /// pending initial turn, modelling a resume that fired and left it behind.
    async fn parked_with_continuation(
        id: &str,
        redeliveries: usize,
    ) -> (
        crate::session::test_support::AppDirGuard,
        Arc<AppState>,
        tempfile::TempDir,
    ) {
        let (home, state, project) = parked(id, redeliveries).await;
        state
            .session_service
            .set_pending_initial_turn(id, "run the nightly task".into(), Vec::new())
            .await;
        (home, state, project)
    }

    /// The queued continuation's `synthesized` flag, or `None` when no turn
    /// is queued.
    async fn pending_turn(state: &AppState, id: &str) -> Option<bool> {
        state
            .instances
            .read()
            .await
            .iter()
            .find(|i| i.id == id)
            .and_then(|i| i.pending_initial_turn.as_ref())
            .map(|t| t.synthesized)
    }

    async fn ready_worker(
        state: &Arc<AppState>,
        id: &str,
    ) -> Arc<std::sync::Mutex<Vec<&'static str>>> {
        let row = state
            .instances
            .read()
            .await
            .iter()
            .find(|row| row.id == id)
            .unwrap()
            .clone();
        let storage =
            Arc::new(crate::session::Storage::open_reference(&row.source_profile).unwrap());
        storage
            .update(|rows, _| {
                *rows.iter_mut().find(|stored| stored.id == id).unwrap() = row.clone();
                Ok(())
            })
            .unwrap();
        crate::server::test_support::refresh_canonical_metadata_for_test(state).await;
        let commands = state
            .acp_supervisor
            .test_insert_classified_worker(id, &row.tool)
            .await;
        let scope = crate::acp::state::NativeObservationScope::capture(&row, &storage).unwrap();
        state
            .acp_supervisor
            .current_lease(id)
            .unwrap()
            .set_admission(Some(crate::acp::supervisor::LaunchAdmission {
                store: storage,
                generation: row.lifecycle_generation,
                namespace: Some(state.profile_namespace.clone()),
                native_scope: Some(scope),
            }));
        state.acp_supervisor.test_observe_native_ready(id).await;
        commands
    }

    async fn settle_delivery(state: &Arc<AppState>, id: &str) {
        drop(
            tokio::time::timeout(
                Duration::from_secs(5),
                state.session_service.prompt_submission(id),
            )
            .await
            .expect("the supervised quota delivery must release submission ownership"),
        );
    }

    fn delivered_prompts(state: &AppState, id: &str, frontier: u64) -> Vec<(String, bool)> {
        state
            .acp_event_store
            .replay_from(id, frontier)
            .into_iter()
            .filter_map(|(_, event)| match event {
                Event::UserPromptSent {
                    text, synthesized, ..
                } => Some((text, synthesized)),
                _ => None,
            })
            .collect()
    }

    fn latest_stop_reason(state: &AppState, id: &str) -> Option<String> {
        state
            .acp_event_store
            .replay_from(id, 0)
            .into_iter()
            .rev()
            .find_map(|(_, e)| match e {
                Event::Stopped { reason } => Some(reason),
                _ => None,
            })
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rate_limit_resume_backs_off_after_a_failed_attempt() {
        let id = "sess-3514-backoff";
        let (_home, state, _project) = parked_with_continuation(id, 0).await;
        state
            .acp_supervisor
            .publish_rate_limit_auto_resumed(id, Utc::now(), false)
            .unwrap();
        state
            .acp_supervisor
            .publish_startup_error(id, "spawn failed".into());
        let mut attempted: HashSet<String> = [id.to_string()].into();

        let frontier = state.acp_event_store.highest_seq(id);
        reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;
        assert!(attempted.contains(id));
        assert_eq!(state.acp_event_store.highest_seq(id), frontier);
    }

    /// The resume loop holds a park even behind a newer startup error, so it
    /// never respawns into the same limit.
    #[tokio::test]
    #[serial_test::serial]
    async fn resume_loop_holds_a_parked_session_behind_a_startup_error() {
        let id = "sess-3514-hold";
        let (_home, state, _project) = parked_with_continuation(id, 0).await;
        enable_auto_resume(false);
        state
            .acp_supervisor
            .publish_startup_error(id, "spawn failed once".into());
        let errors_before = startup_errors(&state, id);

        let mut tick = Tick::default();
        tick.run(&state).await;

        assert!(tick.attempted.contains(id));
        assert_eq!(
            startup_errors(&state, id),
            errors_before,
            "no spawn was attempted"
        );
    }

    /// A cap releases only already queued user work, even with auto-resume off.
    #[tokio::test]
    #[serial_test::serial]
    async fn cap_park_releases_attempted_for_a_prompt_already_on_the_queue() {
        for has_queue in [true, false] {
            let id = "sess-3688-cap-queue";
            let (_home, state, _project) = test_state(id);
            enable_auto_resume(false);
            assert!(state.acp_supervisor.publish_stopped_if_seq(
                id,
                RATE_LIMIT_EXHAUSTED_RETRIES_REASON,
                0
            ));
            if has_queue {
                state.instances.write().await[0]
                    .queued_prompts
                    .push(queued_prompt());
            }
            ready_worker(&state, id).await;
            let _live = crate::acp::supervisor::test_support::save_agent_record(id, "claude", true);
            let frontier = state.acp_event_store.highest_seq(id);

            let mut tick = Tick::default();
            tick.attempted.insert(id.to_string());
            reap_rate_limit_resumes(&state, &mut tick.attempted, &HashSet::new()).await;
            assert_eq!(
                !tick.attempted.contains(id),
                has_queue,
                "has_queue={has_queue}"
            );
            settle_delivery(&state, id).await;

            tick.run(&state).await;
            settle_delivery(&state, id).await;
            assert_eq!(
                startup_errors(&state, id),
                0,
                "has_queue={has_queue}: {:?}",
                state.acp_event_store.replay_from(id, frontier)
            );
            assert_eq!(
                delivered_prompts(&state, id, frontier).len(),
                usize::from(has_queue)
            );
            assert_eq!(auto_resumed_breadcrumbs(&state, id), 0);
        }
    }

    enum Setup {
        None,
        CasAhead,
        SubmissionHeld,
    }

    /// The supervised decision delivers below cap, caps without delivery at
    /// budget, and preserves work when the frontier or submission is refused.
    #[tokio::test]
    #[serial_test::serial]
    async fn rate_limit_reap_resumes_below_the_cap_and_parks_at_it() {
        let max = RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES as usize;
        // (streak, setup, scheduled, latest stop reason, remaining initial turn).
        let cases = [
            (max - 1, Setup::None, true, "rate_limited", None),
            (
                max,
                Setup::None,
                true,
                RATE_LIMIT_EXHAUSTED_RETRIES_REASON,
                None,
            ),
            (max, Setup::CasAhead, true, "rate_limited", Some(true)),
            (
                max,
                Setup::SubmissionHeld,
                false,
                "rate_limited",
                Some(true),
            ),
        ];
        for (streak, setup, scheduled, reason, kept) in cases {
            let id = "sess-3688";
            let (_home, state, _project) = parked_with_continuation(id, streak).await;
            ready_worker(&state, id).await;
            let _guard = match setup {
                Setup::None => None,
                Setup::CasAhead => {
                    let ahead = state.acp_event_store.highest_seq(id) + 1;
                    state.acp_supervisor.hydrate_seqs([(id.to_string(), ahead)]);
                    None
                }
                Setup::SubmissionHeld => Some(state.session_service.prompt_submission(id).await),
            };
            let mut attempted: HashSet<String> = [id.to_string()].into();

            tokio::time::timeout(
                Duration::from_secs(5),
                reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()),
            )
            .await
            .expect("a contended submission must not block the tick");
            if !matches!(setup, Setup::SubmissionHeld) {
                settle_delivery(&state, id).await;
            }

            let case = format!("streak={streak}");
            assert_eq!(!attempted.contains(id), scheduled, "{case}");
            assert_eq!(
                latest_stop_reason(&state, id).as_deref(),
                Some(reason),
                "{case}"
            );
            assert_eq!(pending_turn(&state, id).await, kept, "{case}");
        }
    }

    /// An elapsed agent-initiated hold continues on its current qualified actor,
    /// without mistaking observational attachment for replay consent.
    #[tokio::test]
    #[serial_test::serial]
    async fn agent_initiated_park_installs_a_continue_prompt() {
        let id = "sess-4373-continue";
        let (_home, state, _project) = test_state(id);
        enable_auto_resume(true);
        let long_ago = Utc::now() - chrono::Duration::hours(1);
        let events = [
            Event::AgentMessageChunk {
                text: "x".into(),
                message_id: None,
            },
            Event::RateLimit {
                info: crate::acp::state::RateLimitInfo {
                    status: "limited".into(),
                    resets_at: Some(long_ago),
                    kind: "usage".into(),
                },
            },
            Event::Stopped {
                reason: "rate_limited".into(),
            },
        ];
        for (seq, event) in events.iter().enumerate() {
            state
                .acp_event_store
                .record_at(id, seq as u64 + 1, event, long_ago.timestamp_millis())
                .unwrap();
        }
        state
            .acp_supervisor
            .hydrate_seqs([(id.to_string(), state.acp_event_store.highest_seq(id))]);

        let mut attempted: HashSet<String> = [id.to_string()].into();

        ready_worker(&state, id).await;
        let frontier = state.acp_event_store.highest_seq(id);
        reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;
        assert!(!attempted.contains(id));
        settle_delivery(&state, id).await;
        assert_eq!(pending_turn(&state, id).await, None);
        assert_eq!(
            delivered_prompts(&state, id, frontier),
            vec![(AGENT_TURN_CONTINUATION.to_string(), true)]
        );
        assert_eq!(auto_resumed_breadcrumbs(&state, id), 1);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn automatic_quota_delivery_obeys_reported_reset_floor_and_unknown_backoff() {
        let now = Utc::now();
        let cases = [
            (
                Some(now + chrono::Duration::hours(1)),
                now - chrono::Duration::days(1),
                false,
            ),
            // A recorded clock ahead of now still imposes the minimum park floor.
            (
                Some(now - chrono::Duration::hours(1)),
                now + chrono::Duration::hours(1),
                false,
            ),
            (None, now - chrono::Duration::minutes(30), false),
            (
                Some(now - chrono::Duration::hours(1)),
                now - chrono::Duration::days(1),
                true,
            ),
            (None, now - chrono::Duration::days(1), true),
        ];
        for (reset, recorded, due) in cases {
            let id = "quota-policy-clock";
            let (_home, state, _project) = test_state(id);
            enable_auto_resume(true);
            for (seq, event) in [
                crate::acp::state::test_support::prompt("original interrupted work"),
                Event::RateLimit {
                    info: crate::acp::state::RateLimitInfo {
                        status: "limited".into(),
                        resets_at: reset,
                        kind: "usage".into(),
                    },
                },
                Event::Stopped {
                    reason: "rate_limited".into(),
                },
            ]
            .iter()
            .enumerate()
            {
                state
                    .acp_event_store
                    .record_at(id, seq as u64 + 1, event, recorded.timestamp_millis())
                    .unwrap();
            }
            state
                .acp_supervisor
                .hydrate_seqs([(id.into(), state.acp_event_store.highest_seq(id))]);
            state
                .session_service
                .set_pending_initial_turn(id, "original interrupted work".into(), Vec::new())
                .await;
            let commands = ready_worker(&state, id).await;
            let lease = state.acp_supervisor.current_lease(id).unwrap();
            let park = state.acp_event_store.rate_limit_park(id).unwrap();
            let frontier = state.acp_event_store.highest_seq(id);
            let submission = state.session_service.prompt_submission(id).await;
            resume_rate_limited(
                &state,
                id,
                submission,
                QuotaResume::Automatic { park_seq: park.seq },
            )
            .await
            .unwrap();
            state.acp_supervisor.test_flush_worker_commands(id).await;
            assert_eq!(
                commands
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|name| **name == "prompt")
                    .count(),
                usize::from(due)
            );
            assert_eq!(
                delivered_prompts(&state, id, frontier).len(),
                usize::from(due)
            );
            assert_eq!(auto_resumed_breadcrumbs(&state, id), usize::from(due));
            assert_eq!(
                pending_turn(&state, id).await,
                if due { None } else { Some(true) }
            );
            assert_eq!(
                state.acp_event_store.rate_limit_redelivery_streak(id),
                i64::from(due)
            );
            assert!(state.acp_supervisor.current_lease(id) == Some(lease));
            assert_eq!(startup_errors(&state, id), 0);
            state.shutdown.cancel();
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn qualified_native_loss_preserves_quota_pending_queue_and_budget_before_delivery() {
        use futures_util::FutureExt as _;
        for automatic in [false, true] {
            let id = "quota-native-refusal";
            let (_home, state, _project) = parked_with_continuation(id, 2).await;
            state.instances.write().await[0]
                .queued_prompts
                .push(queued_prompt());
            let commands = ready_worker(&state, id).await;
            let original = state
                .acp_supervisor
                .current_native_availability(id)
                .await
                .unwrap();
            let before = state.instances.read().await[0].clone();
            let frontier = state.acp_event_store.highest_seq(id);
            let budget = state.acp_event_store.rate_limit_redelivery_streak(id);
            let park = state.acp_event_store.rate_limit_park(id).unwrap();
            let checked = std::panic::AssertUnwindSafe(async {
                original.client.test_observe_native_loss();
                let submission = state.session_service.prompt_submission(id).await;
                let kind = if automatic {
                    QuotaResume::Automatic { park_seq: park.seq }
                } else {
                    QuotaResume::Manual
                };
                let error = resume_rate_limited(&state, id, submission, kind)
                    .await
                    .unwrap_err();
                assert!(matches!(
                    error,
                    crate::acp::supervisor::SupervisorError::Acp(
                        crate::acp::acp_client::AcpError::ConnectionUnavailable(_)
                    )
                ));
                original.client.test_flush_commands().await;
                let lost = state
                    .acp_supervisor
                    .current_native_availability(id)
                    .await
                    .unwrap();
                assert!(
                    !lost.ready && lost.unavailable.is_some(),
                    "FIFO observation cannot restore native authority"
                );
                assert!(commands.lock().unwrap().is_empty());
                assert_eq!(state.acp_event_store.highest_seq(id), frontier);
                assert_eq!(
                    state.acp_event_store.rate_limit_redelivery_streak(id),
                    budget
                );
                let after = state.instances.read().await[0].clone();
                assert_eq!(
                    serde_json::to_value(after.pending_initial_turn).unwrap(),
                    serde_json::to_value(before.pending_initial_turn).unwrap()
                );
                assert_eq!(
                    serde_json::to_value(after.queued_prompts).unwrap(),
                    serde_json::to_value(before.queued_prompts).unwrap()
                );
                assert_eq!(after.acp_session_id, before.acp_session_id);
                assert!(state.acp_supervisor.current_lease(id) == Some(original.lease.clone()));
                assert_eq!(startup_errors(&state, id), 0);
            })
            .catch_unwind()
            .await;
            state.acp_supervisor.test_remove_worker(id).await;
            original.client.shutdown().await.unwrap();
            state.shutdown.cancel();
            if let Err(panic) = checked {
                std::panic::resume_unwind(panic);
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn an_aged_quota_hold_prevents_idle_reaping_without_spending_retry_budget() {
        let id = "quota-idle-hold";
        let (_home, state, _project) = parked_with_continuation(id, 1).await;
        std::fs::write(
            crate::session::get_app_dir().unwrap().join("config.toml"),
            "[acp]\nrate_limit_auto_resume = true\nauto_stop_idle_secs = 1\n",
        )
        .unwrap();
        let commands = ready_worker(&state, id).await;
        let lease = state.acp_supervisor.current_lease(id).unwrap();
        lease.test_backdate_idle_baseline(
            (Utc::now() - chrono::Duration::days(1)).timestamp_millis(),
        );
        let frontier = state.acp_event_store.highest_seq(id);
        super::super::idle::reap_idle_workers(&state).await;
        state.acp_supervisor.test_flush_worker_commands(id).await;
        assert!(state.acp_supervisor.current_lease(id) == Some(lease));
        assert!(commands.lock().unwrap().is_empty());
        assert_eq!(state.acp_event_store.highest_seq(id), frontier);
        assert_eq!(state.acp_event_store.rate_limit_redelivery_streak(id), 1);
        assert!(!state.instances.read().await[0].is_idle_dormant());
        assert_eq!(pending_turn(&state, id).await, Some(true));
        state.shutdown.cancel();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn selected_quota_queue_delivers_mixed_age_rows_and_stored_bytes_to_the_same_native_actor(
    ) {
        use crate::acp::supervisor::test_support::HeldAttachRunner;
        use base64::Engine as _;
        let id = "quota-native-queue-payload";
        let (_home, state, project) = parked_with_continuation(id, 0).await;
        enable_auto_resume(false);
        let park = state.acp_event_store.rate_limit_park(id).unwrap();
        let bytes = [
            b"original-old-image".to_vec(),
            b"original-new-image".to_vec(),
        ];
        {
            let mut rows = state.instances.write().await;
            rows[0].acp_session_id = Some("same-native".into());
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
                assert!(state.acp_event_store.record_pending_attachment(
                    id,
                    &queued.id,
                    &crate::acp::event_store::AttachmentBlob {
                        id: format!("image-{index}"),
                        kind: crate::daemon::PromptAttachmentKind::Image,
                        mime_type: "image/png".into(),
                        name: Some(format!("original-{index}.png")),
                        data: data.clone(),
                    }
                ));
                rows[0].queued_prompts.push(queued);
            }
        }
        let row = state.instances.read().await[0].clone();
        crate::session::Storage::open_reference("default")
            .unwrap()
            .update(|rows, _| {
                *rows.iter_mut().find(|value| value.id == id).unwrap() = row.clone();
                Ok(())
            })
            .unwrap();
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let mut runner = HeldAttachRunner::recording_prompt(
            id,
            project.path(),
            17,
            Some("default"),
            "same-native",
        )
        .await;
        let mut events = state.acp_events_tx.subscribe();
        state
            .acp_supervisor
            .attach(id.into(), project.path().into(), Vec::new(), false, None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let frame = events.recv().await.unwrap();
                if matches!(frame.event.as_ref(), Event::AcpSessionAssigned { .. }) {
                    break;
                }
                assert!(!matches!(
                    frame.event.as_ref(),
                    Event::AgentStartupError { .. }
                ));
            }
        })
        .await
        .unwrap();
        assert_eq!(runner.resumed_session().await, "same-native");
        let original = state
            .acp_supervisor
            .current_native_availability(id)
            .await
            .unwrap();
        assert_eq!(original.owner, Some(runner.identity));
        assert_eq!(
            state.acp_event_store.rate_limit_park(id).unwrap().seq,
            park.seq,
            "read-only attachment cannot release quota"
        );
        assert_eq!(pending_turn(&state, id).await, Some(true));
        assert_eq!(auto_resumed_breadcrumbs(&state, id), 0);
        let frontier = state.acp_event_store.highest_seq(id);
        let submission = state.session_service.prompt_submission(id).await;
        resume_rate_limited(&state, id, submission, QuotaResume::Manual)
            .await
            .unwrap();
        let request = tokio::time::timeout(Duration::from_secs(10), runner.next_prompt())
            .await
            .unwrap();
        assert_eq!(request["sessionId"], "same-native");
        let blocks = request["prompt"].as_array().unwrap();
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
                assert_eq!(block["mimeType"], "image/png");
                base64::engine::general_purpose::STANDARD
                    .decode(block["data"].as_str().unwrap())
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(images, bytes);
        assert_eq!(delivered_prompts(&state, id, frontier), vec![(text, false)]);
        assert_eq!(auto_resumed_breadcrumbs(&state, id), 1);
        assert_eq!(pending_turn(&state, id).await, None);
        assert!(state.instances.read().await[0].queued_prompts.is_empty());
        assert!(state.acp_supervisor.current_lease(id) == Some(original.lease.clone()));
        assert!(crate::process::worker_registry::is_record_live(
            &runner.record
        ));
        runner.complete_turn();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let frame = events.recv().await.unwrap();
                if matches!(frame.event.as_ref(), Event::Stopped { reason } if reason == "prompt_complete") { break; }
            }
        }).await.unwrap();
        assert_eq!(
            state.instances.read().await[0].acp_session_id.as_deref(),
            Some("same-native")
        );
        state.acp_supervisor.test_remove_worker(id).await;
        original.client.shutdown().await.unwrap();
        state.shutdown.cancel();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn authorized_quota_queue_preserves_background_work_and_same_actor() {
        for automatic in [false, true] {
            let id = "quota-background-queue";
            let (_home, state, _project) = parked_with_continuation(id, 0).await;
            enable_auto_resume(automatic);
            let launched = Event::BackgroundAgentLaunched {
                agent_id: "retained-background".into(),
                tool_call_id: "retained-tool".into(),
                description: "retained background work".into(),
                prompt: "finish background work".into(),
                model: "test".into(),
                output_file: String::new(),
                workflow: None,
                started_at: Utc::now(),
            };
            let progress = Event::BackgroundAgentProgress {
                agent_id: "retained-background".into(),
                status: crate::acp::state::BackgroundAgentStatus::Running,
                tool_count: 1,
                tools: Vec::new(),
                last_tool: None,
                last_text: None,
                at: Utc::now(),
            };
            for event in [launched, progress] {
                state
                    .acp_event_store
                    .record(id, state.acp_event_store.highest_seq(id) + 1, &event)
                    .unwrap();
            }
            state
                .acp_supervisor
                .hydrate_seqs([(id.to_string(), state.acp_event_store.highest_seq(id))]);
            {
                let mut rows = state.instances.write().await;
                rows[0].status = crate::session::Status::Running;
                let mut queued = queued_prompt();
                queued.text = "newer queued intent".into();
                rows[0].queued_prompts.push(queued);
            }
            let commands = ready_worker(&state, id).await;
            let original = state
                .acp_supervisor
                .current_native_availability(id)
                .await
                .unwrap();
            let owner = original.lease.clone();
            let control = state.session_service.fold_control_state(id).await;
            assert!(
                control.connection_ready
                    && !control.turn_active
                    && control.has_active_background_agent()
            );
            assert_eq!(control.sidebar_status(), crate::session::Status::Running);
            assert!(state.acp_event_store.has_in_flight_turn(id));
            assert!(!state.acp_event_store.has_in_flight_main_turn(id));
            let park_seq = state.acp_event_store.rate_limit_park(id).unwrap().seq;
            let frontier = state.acp_event_store.highest_seq(id);
            state.session_service.drain_queued_prompts_once(id).await;
            assert_eq!(
                state.acp_event_store.highest_seq(id),
                frontier,
                "ordinary drain cannot release a quota hold"
            );
            assert_eq!(pending_turn(&state, id).await, Some(true));
            let submission = state.session_service.prompt_submission(id).await;
            let kind = if automatic {
                QuotaResume::Automatic { park_seq }
            } else {
                QuotaResume::Manual
            };
            resume_rate_limited(&state, id, submission, kind)
                .await
                .unwrap();
            original.client.test_flush_commands().await;
            assert_eq!(
                commands
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|name| **name == "prompt")
                    .count(),
                1
            );
            assert!(state.acp_event_store.rate_limit_park(id).is_none());
            assert_eq!(
                delivered_prompts(&state, id, frontier),
                vec![("newer queued intent".into(), false)]
            );
            assert_eq!(
                auto_resumed_breadcrumbs(&state, id),
                usize::from(!automatic)
            );
            assert_eq!(pending_turn(&state, id).await, None);
            assert!(state.instances.read().await[0].queued_prompts.is_empty());
            assert!(state.acp_supervisor.current_lease(id) == Some(owner));
            assert!(state
                .session_service
                .fold_control_state(id)
                .await
                .has_active_background_agent());
            assert_eq!(startup_errors(&state, id), 0);
            state.acp_supervisor.test_remove_worker(id).await;
            original.client.shutdown().await.unwrap();
            state.shutdown.cancel();
        }
    }

    fn auto_resumed_breadcrumbs(state: &AppState, id: &str) -> usize {
        state
            .acp_event_store
            .replay_from(id, 0)
            .into_iter()
            .filter(|(_, e)| matches!(e, Event::RateLimitAutoResumed { .. }))
            .count()
    }

    /// Only a row minted after the limit replaced the turn. A row before it and
    /// a limit that was never recorded both leave the continuation standing: the
    /// interrupted request is the older one and losing it is final.
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

    /// #4092: a queued prompt owns the next turn when at least one row was
    /// minted after the limit that interrupted it. A row queued while the
    /// interrupted turn was still running
    /// is a follow-up the user wants behind it, so the continuation is installed
    /// and its redelivery charged. A row minted after the park clears a
    /// continuation an earlier cadence installed, because `/queue` never clears
    /// the slot itself. The rows are seeded on the instance because the endpoint
    /// stamps the server's clock, which no test can place in the past.
    #[tokio::test]
    #[serial_test::serial]
    async fn only_a_prompt_queued_after_the_park_supersedes_the_continuation() {
        // The `parked` fixture backdates the limit an hour, so these straddle it.
        let after = Utc::now();
        let before = Utc::now() - chrono::Duration::hours(2);
        // (label, initial continuation installed, queued at, continuation chosen, breadcrumbs)
        let cases = [
            ("queued-after", false, after.to_rfc3339(), None, 0),
            (
                "installed-and-queued-after",
                true,
                after.to_rfc3339(),
                None,
                0,
            ),
            ("queued-before", false, before.to_rfc3339(), Some(true), 1),
            ("queued-at-unreadable", false, "t0".into(), Some(true), 1),
        ];
        for (label, installed, queued_at, kept, breadcrumbs) in cases {
            let id = format!("sess-4092-{label}");
            let (_home, state, _project) = parked(&id, 0).await;
            if installed {
                state
                    .session_service
                    .set_pending_initial_turn(&id, "run the nightly task".into(), Vec::new())
                    .await;
            }
            state.instances.write().await[0].queued_prompts.push(
                crate::daemon::QueuedPromptEntry {
                    id: "q-1".into(),
                    seq: 0,
                    text: "manual prompt B".into(),
                    attachments: Vec::new(),
                    created_at: queued_at,
                    origin_device: None,
                    origin_plugin: None,
                },
            );
            assert!(
                !state
                    .session_service
                    .queued_prompts_snapshot(&id)
                    .await
                    .is_empty(),
                "{label}: the queue row must land, or the case asserts nothing"
            );
            assert!(
                state.acp_event_store.rate_limit_park(&id).is_some(),
                "{label}: a queued prompt must leave the park standing"
            );
            ready_worker(&state, &id).await;
            let frontier = state.acp_event_store.highest_seq(&id);
            let mut attempted: HashSet<String> = [id.clone()].into();

            reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;
            settle_delivery(&state, &id).await;

            assert_eq!(pending_turn(&state, &id).await, None, "{label}");
            let expected = if kept.is_some() {
                ("run the nightly task", true)
            } else {
                ("manual prompt B", false)
            };
            assert_eq!(
                delivered_prompts(&state, &id, frontier),
                vec![(expected.0.into(), expected.1)],
                "{label}"
            );
            assert_eq!(
                auto_resumed_breadcrumbs(&state, &id),
                breadcrumbs,
                "{label}: a refused continuation must not spend a redelivery"
            );
            assert!(
                !attempted.contains(&id),
                "{label}: the queued prompt has no other route to a worker"
            );
        }
    }

    /// Attaching to the same actor is observational: it neither releases the
    /// quota hold nor supersedes the interrupted prompt selected for replay.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_manual_install_lands_after_its_own_worker_assigns_the_session() {
        let id = "sess-4092-assigned";
        let (_home, state, _project) = parked(id, 0).await;
        let next_seq = state.acp_event_store.highest_seq(id) + 1;
        state
            .acp_event_store
            .record_at(
                id,
                next_seq,
                &Event::AcpSessionAssigned {
                    acp_session_id: "sid-1".into(),
                    connection_ready: false,
                    native_subject: None,
                },
                Utc::now().timestamp_millis(),
            )
            .unwrap();
        assert!(
            state.acp_event_store.rate_limit_park(id).is_some(),
            "an observed assignment must not release the quota hold"
        );

        let _outcome = install_rate_limit_continuation(
            &state,
            id,
            &state.session_service.prompt_submission(id).await,
        )
        .await;

        assert_eq!(
            pending_turn(&state, id).await,
            Some(true),
            "the interrupted prompt is still the work to continue"
        );
    }

    /// A prompt submission owns the session, so the pass refuses rather than
    /// stalling its single sequential run behind it: the held slot keeps the
    /// resume pass off, nothing is released and no breadcrumb is spent, and
    /// the next cadence retries (#4092).
    #[tokio::test]
    #[serial_test::serial]
    async fn the_pass_refuses_while_a_prompt_submission_owns_the_session() {
        let id = "sess-4092-contended";
        let (_home, state, _project) = parked(id, 0).await;
        assert!(
            state.acp_event_store.rate_limit_park(id).is_some(),
            "the fixture must arm a park, or the pass skips it for an unrelated reason"
        );
        let _submission = state.session_service.prompt_submission(id).await;
        let mut attempted: HashSet<String> = [id.to_string()].into();

        tokio::time::timeout(
            Duration::from_secs(5),
            reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()),
        )
        .await
        .expect("a contended submission guard must not block the tick");

        assert!(
            attempted.contains(id),
            "the session stays held until the next cadence"
        );
        assert_eq!(pending_turn(&state, id).await, None);
        assert_eq!(auto_resumed_breadcrumbs(&state, id), 0);
    }

    /// Two accounts of one agent, failover on with `claude-b` in the pool, and the
    /// session's row on `claude-a`.
    async fn failover_session(
        id: &str,
        redeliveries: usize,
    ) -> (
        crate::session::test_support::AppDirGuard,
        Arc<AppState>,
        tempfile::TempDir,
        crate::tmux::status_rules::ProfileRegistryGuard,
    ) {
        let (home, state, project) = parked(id, redeliveries).await;
        let app = crate::session::get_app_dir().unwrap();
        std::fs::write(
            app.join("config.toml"),
            "[acp]\nrate_limit_auto_resume = true\nrate_limit_failover = true\n\
             rate_limit_failover_accounts = [\"claude-a\", \"claude-b\"]\n\
             [session.agent_detect_as]\nclaude-a = \"claude\"\nclaude-b = \"claude\"\nclaude-c = \"claude\"\n\
             [session.agent_config_dir]\nclaude-a = \"~/.claude-a\"\nclaude-b = \"~/.claude-b\"\n",
        )
        .unwrap();
        let profile = crate::session::config::effective_profile("default");
        let registry = crate::tmux::status_rules::ProfileRegistryGuard::take(&profile);
        crate::session::config::profile_config::resolve_config_or_warn(&profile);
        {
            let mut rows = state.instances.write().await;
            rows[0].tool = "claude-a".into();
            rows[0].detect_as = "claude".into();
        }
        (home, state, project, registry)
    }

    fn account_key_of(tool: &str) -> String {
        failover::path_key(&dirs::home_dir().unwrap().join(format!(".{tool}")))
    }

    /// A pool name without its own `[session.agent_config_dir]` entry would resolve to the
    /// environment's or the default directory, an account the pool never named.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_pool_account_needs_a_declared_config_directory() {
        let (_home, state, _project, _registry) = failover_session("sess-pool-declared", 0).await;
        let row = state.instances.read().await[0].clone();
        let pool = failover::resolve_pool(&row, &["claude-b".into(), "claude-c".into()]);
        assert_eq!(
            pool.iter().map(|a| a.tool.as_str()).collect::<Vec<_>>(),
            ["claude-b"],
            "claude-c declares no directory"
        );
    }

    /// With the failover off the account table is never touched: a park keeps its own schedule
    /// and nothing it reports lands in, or is read from, the table.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_park_leaves_the_account_table_alone_when_failover_is_off() {
        let id = "sess-failover-off";
        let (_home, state, _project, _registry) = failover_session(id, 0).await;
        std::fs::write(
            crate::session::get_app_dir().unwrap().join("config.toml"),
            "[acp]\nrate_limit_auto_resume = true\nrate_limit_failover = false\n\
             [session.agent_detect_as]\nclaude-a = \"claude\"\n\
             [session.agent_config_dir]\nclaude-a = \"~/.claude-a\"\n",
        )
        .unwrap();
        crate::session::config::profile_config::resolve_config_or_warn(
            &crate::session::config::effective_profile("default"),
        );
        let mut attempted: HashSet<String> = [id.to_string()].into();

        reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;

        assert!(
            state
                .acp_event_store
                .account_state(&account_key_of("claude-a"))
                .is_none(),
            "no account state is written while the failover is off"
        );
    }

    /// A running swap has already moved the row onto its target, so the list still names that
    /// target instead of recomputing one from the moved row.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_list_keeps_naming_the_target_of_a_running_swap() {
        let id = "sess-failover-running";
        let (_home, state, _project, _registry) = failover_session(id, 0).await;
        state.instances.write().await[0].tool = "claude-b".into();
        let _running = failover::hold_in_flight(id, "claude-b");

        state.runtime.publish(&state).await.unwrap();
        let response = crate::server::api::list_sessions(
            axum::extract::State(Arc::clone(&state)),
            axum::extract::Query(crate::daemon::ListSessionsQuery { state: None }),
            axum::http::HeaderMap::new(),
        )
        .await;
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let envelope: crate::daemon::SessionsEnvelope = serde_json::from_slice(&body).unwrap();
        let row = envelope.sessions.into_iter().find(|s| s.id == id).unwrap();
        assert_eq!(row.rate_limit_failover_to.as_deref(), Some("claude-b"));
    }

    /// While a swap runs, its launch can fail and leave the row naming the target before the
    /// rollback puts the old account back. A park seen in that window belongs to the old account:
    /// the reaper must not write its window under the target, whether it reads the row during the
    /// swap or holds a snapshot of it past the rollback.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_park_seen_mid_swap_never_lands_on_the_swap_target() {
        let id = "sess-failover-midswap";
        let (_home, state, _project, _registry) = failover_session(id, 0).await;
        let before = state.instances.read().await[0].clone();
        state.instances.write().await[0].tool = "claude-b".into();
        let target_snapshot = state.instances.read().await[0].clone();
        let key_b = account_key_of("claude-b");
        let weekly = crate::acp::state::RateLimitInfo {
            status: "limited".into(),
            resets_at: Some(Utc::now() + chrono::Duration::days(7)),
            kind: "seven_day".into(),
        };
        let until = Utc::now() + chrono::Duration::days(7);

        // The reaper pass runs while the claim is held and the row still names the target.
        let running = failover::hold_in_flight(id, "claude-b");
        let mut attempted: HashSet<String> = [id.to_string()].into();
        reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;
        assert!(
            state.acp_event_store.account_state(&key_b).is_none(),
            "nothing is recorded for the target during the swap"
        );

        // A snapshot taken then, used after the rollback restored the old account.
        state.instances.write().await[0].tool = before.tool.clone();
        drop(running);
        failover::record_limit(&state, &target_snapshot, &weekly, until, 1).await;
        assert!(
            state.acp_event_store.account_state(&key_b).is_none(),
            "a stale snapshot of the target cannot write the old account's window"
        );
        assert!(failover::reported_deadline(
            &state,
            &target_snapshot,
            &crate::acp::state::RateLimitInfo {
                resets_at: None,
                ..weekly
            }
        )
        .await
        .is_none());
    }

    /// An agent whose conversation AoE cannot locate in an account root has no account here: its
    /// limit must not be written under, or read back from, the default Claude account.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_non_claude_agent_takes_no_part_in_the_account_limits() {
        let id = "sess-non-claude-limits";
        let (_home, state, _project, _registry) = failover_session(id, 0).await;
        {
            let mut rows = state.instances.write().await;
            rows[0].tool = "codex".into();
            rows[0].detect_as = "codex".into();
        }
        let row = state.instances.read().await[0].clone();
        let claude_default = dirs::home_dir().unwrap().join(".claude");
        let claude_key = claude_default.to_string_lossy().into_owned();
        let now_ms = Utc::now().timestamp_millis();
        state.acp_event_store.record_account_limit(
            &claude_key,
            now_ms + 3_600_000,
            "usage",
            now_ms,
            false,
        );

        assert_eq!(failover::account_key(&row), None);
        let undated = crate::acp::state::RateLimitInfo {
            status: "limited".into(),
            resets_at: None,
            kind: "usage".into(),
        };
        assert_eq!(
            failover::reported_deadline(&state, &row, &undated).await,
            None,
            "the non-Claude park inherits no Claude deadline"
        );

        let mut attempted: HashSet<String> = [id.to_string()].into();
        reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;
        let claude = state.acp_event_store.account_state(&claude_key).unwrap();
        assert_eq!(
            claude.window("usage").map(|w| w.until_ms),
            Some(now_ms + 3_600_000),
            "the Claude account's window is untouched by the non-Claude park"
        );
    }

    /// Waits for the detached failover task to finish.
    async fn failover_settled(id: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while failover::in_flight(id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the failover task must finish");
    }

    /// A park records its account's limit, and a failover the swap refuses (this row has
    /// no recorded conversation) leaves the park to the timed resume: one redelivery, never
    /// a second failover for the same park.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_refused_failover_falls_back_to_the_timed_resume() {
        let id = "sess-failover-refused";
        let (_home, state, _project, _registry) = failover_session(id, 0).await;
        let mut attempted: HashSet<String> = [id.to_string()].into();

        reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;
        assert!(
            attempted.contains(id),
            "the failover owns the park while it runs"
        );
        failover_settled(id).await;

        let key = account_key_of("claude-a");
        let limit = state.acp_event_store.account_state(&key);
        assert!(
            limit.is_some_and(|s| s.window("usage").is_some()),
            "limit recorded under {key}"
        );
        assert_eq!(state.instances.read().await[0].tool, "claude-a");
        assert_eq!(auto_resumed_breadcrumbs(&state, id), 0);

        let submission = state.session_service.prompt_submission(id).await;
        reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;
        assert!(
            !failover::in_flight(id),
            "the refused park is not tried again"
        );
        assert!(attempted.contains(id));
        drop(submission);
        let _commands = ready_worker(&state, id).await;
        let frontier = state.acp_event_store.highest_seq(id);
        reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;
        assert!(!attempted.contains(id), "the timed resume takes over");
        settle_delivery(&state, id).await;
        assert_eq!(auto_resumed_breadcrumbs(&state, id), 1);
        assert_eq!(pending_turn(&state, id).await, None);
        assert_eq!(
            delivered_prompts(&state, id, frontier),
            vec![("run the nightly task".to_string(), true)]
        );
    }

    /// With every pool account limited there is no target: the park runs the timed
    /// resume under its budget, and at the cap it still ends in the terminal park.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_entirely_limited_pool_keeps_the_redelivery_budget() {
        let max = RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES as usize;
        for (streak, released, reason) in [
            (0, true, "rate_limited"),
            (max, false, RATE_LIMIT_EXHAUSTED_RETRIES_REASON),
        ] {
            let id = "sess-failover-pool-limited";
            let (_home, state, _project, _registry) = failover_session(id, streak).await;
            for tool in ["claude-a", "claude-b"] {
                state.acp_event_store.record_account_limit(
                    &account_key_of(tool),
                    Utc::now().timestamp_millis() + 3_600_000,
                    "usage",
                    Utc::now().timestamp_millis(),
                    false,
                );
            }
            let mut attempted: HashSet<String> = [id.to_string()].into();

            let submission = state.session_service.prompt_submission(id).await;
            reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;
            assert!(!failover::in_flight(id), "no failover was started");
            let park = state
                .acp_event_store
                .rate_limit_park(id)
                .unwrap()
                .recorded_at_ms;
            assert!(failover::attempts_made(id, park).is_none());
            drop(submission);
            let _commands = if released {
                Some(ready_worker(&state, id).await)
            } else {
                None
            };
            let frontier = state.acp_event_store.highest_seq(id);
            reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;
            assert!(
                !attempted.contains(id),
                "the due park was scheduled: streak={streak}"
            );
            settle_delivery(&state, id).await;
            assert_eq!(latest_stop_reason(&state, id).as_deref(), Some(reason));
            assert_eq!(
                delivered_prompts(&state, id, frontier).len(),
                usize::from(released),
                "streak={streak}"
            );
            assert_eq!(
                auto_resumed_breadcrumbs(&state, id),
                streak + usize::from(released),
                "streak={streak}"
            );
        }
    }

    /// The list response names the account a failover is about to move a parked session to, so
    /// the banner shows the switch instead of a choice; with no headroom in the pool, or with
    /// the failover off, it stays absent and the choice remains.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_session_list_announces_a_pending_failover_only_while_one_can_happen() {
        let id = "sess-failover-signal";
        let (_home, state, _project, _registry) = failover_session(id, 0).await;
        let listed = || async {
            state.runtime.publish(&state).await.unwrap();
            let response = crate::server::api::list_sessions(
                axum::extract::State(Arc::clone(&state)),
                axum::extract::Query(crate::daemon::ListSessionsQuery { state: None }),
                axum::http::HeaderMap::new(),
            )
            .await;
            let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap();
            let envelope: crate::daemon::SessionsEnvelope = serde_json::from_slice(&body).unwrap();
            let row = envelope.sessions.into_iter().find(|s| s.id == id).unwrap();
            assert!(row.rate_limit.is_some(), "the fixture session is parked");
            row.rate_limit_failover_to
        };
        assert_eq!(listed().await.as_deref(), Some("claude-b"));

        let now_ms = Utc::now().timestamp_millis();
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-b"),
            now_ms + 3_600_000,
            "usage",
            now_ms,
            false,
        );
        assert_eq!(listed().await, None, "no pool account has headroom");

        state.acp_event_store.record_account_limit(
            &account_key_of("claude-b"),
            now_ms - 1,
            "usage",
            now_ms + 1,
            false,
        );
        assert_eq!(listed().await.as_deref(), Some("claude-b"), "it freed up");

        crate::session::config::update_config(|c| c.acp.rate_limit_failover = false).unwrap();
        assert_eq!(listed().await, None, "failover is off");
    }

    /// A park made while the whole pool was limited fails over on a later pass once an
    /// account frees up, as long as the timed resume has not fired yet.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_park_made_while_the_pool_was_limited_fails_over_when_an_account_frees_up() {
        let id = "sess-failover-later";
        let (_home, state, _project, _registry) = failover_session(id, 0).await;
        let now_ms = Utc::now().timestamp_millis();
        // The park's own window is two hours out, and both accounts are limited that long.
        let resets_at = Utc::now() + chrono::Duration::hours(2);
        let next = state.acp_event_store.highest_seq(id);
        for (offset, event) in [
            Event::RateLimit {
                info: crate::acp::state::RateLimitInfo {
                    status: "limited".into(),
                    resets_at: Some(resets_at),
                    kind: "usage".into(),
                },
            },
            Event::Stopped {
                reason: "rate_limited".into(),
            },
        ]
        .iter()
        .enumerate()
        {
            state
                .acp_event_store
                .record_at(id, next + 1 + offset as u64, event, now_ms)
                .unwrap();
        }
        state
            .acp_supervisor
            .hydrate_seqs([(id.to_string(), state.acp_event_store.highest_seq(id))]);
        for tool in ["claude-a", "claude-b"] {
            state.acp_event_store.record_account_limit(
                &account_key_of(tool),
                now_ms + 7_200_000,
                "usage",
                now_ms,
                false,
            );
        }
        let park = state
            .acp_event_store
            .rate_limit_park(id)
            .unwrap()
            .recorded_at_ms;
        let mut attempted: HashSet<String> = [id.to_string()].into();

        reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;
        assert!(attempted.contains(id), "the window has not elapsed");
        assert_eq!(auto_resumed_breadcrumbs(&state, id), 0);
        assert_eq!(pending_turn(&state, id).await, None);
        assert!(
            failover::attempts_made(id, park).is_none(),
            "no pool account had headroom, so nothing was tried"
        );

        state.acp_event_store.record_account_limit(
            &account_key_of("claude-b"),
            now_ms - 1,
            "usage",
            now_ms + 1,
            false,
        );
        reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;
        failover_settled(id).await;
        assert!(
            failover::attempts_made(id, park).is_some(),
            "the freed account was tried on the next pass"
        );
    }

    /// A fresh session parked on an account-wide limit never saw the dated reset, so its own
    /// park has none. The account's reported deadline, still in force for the same window,
    /// stands in for it: the timed resume waits for it instead of the 1h estimate, and the
    /// estimate never overwrites it.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_undated_park_uses_the_accounts_reported_deadline() {
        let id = "sess-failover-undated";
        let (_home, state, _project, _registry) = failover_session(id, 0).await;
        // The park carries no reset time; it was recorded two hours ago.
        let two_hours_ago = (Utc::now() - chrono::Duration::hours(2)).timestamp_millis();
        let next = state.acp_event_store.highest_seq(id);
        for (offset, event) in [
            Event::RateLimit {
                info: crate::acp::state::RateLimitInfo::undated(),
            },
            Event::Stopped {
                reason: "rate_limited".into(),
            },
        ]
        .iter()
        .enumerate()
        {
            state
                .acp_event_store
                .record_at(id, next + 1 + offset as u64, event, two_hours_ago)
                .unwrap();
        }
        state
            .acp_supervisor
            .hydrate_seqs([(id.to_string(), state.acp_event_store.highest_seq(id))]);
        let kind = state
            .acp_event_store
            .latest_rate_limit_event(id)
            .map(|(info, _)| info.kind)
            .unwrap();
        // The weekly reset was reported on another session two days out; the pool has no room.
        let now_ms = Utc::now().timestamp_millis();
        let two_days = now_ms + 2 * 24 * 3600 * 1000;
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-a"),
            two_days,
            &kind,
            now_ms - 1_000,
            false,
        );
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-b"),
            two_days,
            &kind,
            now_ms - 1_000,
            false,
        );
        let mut attempted: HashSet<String> = [id.to_string()].into();

        reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;

        assert!(
            attempted.contains(id),
            "the 1h estimate elapsed long ago; the reported deadline is two days out"
        );
        assert_eq!(auto_resumed_breadcrumbs(&state, id), 0);
        assert_eq!(pending_turn(&state, id).await, None);
        let stored = state
            .acp_event_store
            .account_state(&account_key_of("claude-a"))
            .unwrap();
        let window = stored.window(&kind).unwrap();
        assert_eq!((window.until_ms, window.estimated), (two_days, false));
    }
}
