//! Opt-in rate-limit auto-resume (#1722) with a bounded redelivery budget (#3688).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::time::Timestamp;

use super::{is_resumable, query_control, query_store, resolve_per_profile, AppState};
use crate::acp::event_store::ControlKnowledge::{Indeterminate, Known};
use crate::acp::state::{RateLimitInfo, RATE_LIMIT_EXHAUSTED_RETRIES_REASON};
use crate::acp::supervisor::SupervisorError;

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
fn rate_limit_resume_at(resets_at: Timestamp, recorded_at_ms: i64, grace_secs: u32) -> Timestamp {
    let resets_plus_grace = resets_at
        .checked_add(crate::time::Duration::seconds(i64::from(grace_secs)))
        .unwrap_or(Timestamp::MAX);
    match Timestamp::from_millis(recorded_at_ms)
        .map(|t| t + crate::time::Duration::seconds(RATE_LIMIT_MIN_PARK_SECS))
    {
        Some(floor) if floor > resets_plus_grace => floor,
        _ => resets_plus_grace,
    }
}

fn scheduled_resume_at(info: &RateLimitInfo, recorded_at_ms: i64, streak: i64) -> Timestamp {
    match info.resets_at {
        Some(reset) => {
            rate_limit_resume_at(reset, recorded_at_ms, RATE_LIMIT_AUTO_RESUME_GRACE_SECS)
        }
        None => rate_limit_unknown_reset_retry_at(recorded_at_ms, streak),
    }
}

/// Use the same qualified calendar before scheduling and again at delivery.
struct ParkCalendar {
    info: RateLimitInfo,
    recorded_at_ms: i64,
    observed_at_ms: i64,
    own: Option<Timestamp>,
    generic: Option<Timestamp>,
}

impl ParkCalendar {
    fn new(park: &crate::acp::event_store::RateLimitPark) -> Self {
        use crate::acp::acp_client::{refine_from_status, Observation};
        let info = park.info.clone().unwrap_or_else(RateLimitInfo::undated);
        let info = match Timestamp::from_millis(park.recorded_at_ms) {
            Some(at) => refine_from_status(info, at, Observation::Recorded),
            None => info,
        };
        let carried = info.resets_at.map(|reset| {
            rate_limit_resume_at(
                reset,
                park.recorded_at_ms,
                RATE_LIMIT_AUTO_RESUME_GRACE_SECS,
            )
        });
        // A reset inherited from a report that named no window may extend, but never
        // stand in for, a named window's deadline.
        let (own, generic) = if park.inherited_window.as_deref() == Some("rate_limit")
            && info.kind != "rate_limit"
        {
            (None, carried)
        } else {
            (carried, None)
        };
        Self {
            info,
            recorded_at_ms: park.recorded_at_ms,
            observed_at_ms: park.observed_at_ms,
            own,
            generic,
        }
    }

    fn view(&self) -> failover::ParkView<'_> {
        failover::ParkView {
            kind: &self.info.kind,
            own_ms: self.own.map(Timestamp::as_millis),
            observed_at_ms: if self.generic.is_some() {
                self.recorded_at_ms
            } else {
                self.observed_at_ms
            },
            generic: self.generic.map(|at| crate::acp::event_store::WindowLimit {
                kind: "rate_limit".into(),
                until_ms: at.as_millis(),
                estimated: false,
                observed_at_ms: self.observed_at_ms,
            }),
        }
    }

    fn scheduled_at(&self, account: &failover::Reported, streak: i64) -> Timestamp {
        let carried = self.own.or(self.generic);
        match account.wake.as_ref().filter(|wake| !wake.own) {
            Some(wake) => Timestamp::from_millis(wake.at_ms).or(carried),
            None => carried.filter(|_| !account.generic_superseded),
        }
        .unwrap_or_else(|| rate_limit_unknown_reset_retry_at(self.recorded_at_ms, streak))
    }

    fn evidence(
        &self,
        scheduled_at: Timestamp,
        account: &failover::Reported,
    ) -> Option<(RateLimitInfo, Timestamp)> {
        // Always record explicit evidence under its source window and observation.
        // Only use an estimate when no applicable account report already stands.
        match (self.own, self.generic) {
            (Some(own), _) => Some((self.info.clone(), own)),
            (None, Some(generic)) => Some((
                RateLimitInfo {
                    kind: "rate_limit".into(),
                    ..self.info.clone()
                },
                generic,
            )),
            (None, None) if account.wake.is_none() => Some((self.info.clone(), scheduled_at)),
            _ => None,
        }
    }
}

fn rate_limit_unknown_reset_retry_at(recorded_at_ms: i64, redeliveries: i64) -> Timestamp {
    let shift = redeliveries.clamp(0, i64::from(RATE_LIMIT_UNKNOWN_RESET_MAX_SHIFT)) as u32;
    let retry_after = crate::time::Duration::seconds(
        RATE_LIMIT_UNKNOWN_RESET_RETRY_SECS.saturating_mul(1 << shift),
    );
    Timestamp::from_millis(recorded_at_ms).unwrap_or_else(Timestamp::now) + retry_after
}

fn row_minted_at_ms(entry: &crate::daemon::QueuedPromptEntry) -> Option<i64> {
    Timestamp::parse_rfc3339(&entry.created_at)
        .ok()
        .map(|queued_at| queued_at.as_millis())
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
fn queue_supersedes(mut minted_at_ms: impl Iterator<Item = i64>, limit_at_ms: Option<i64>) -> bool {
    let Some(limit_at_ms) = limit_at_ms else {
        return false;
    };
    minted_at_ms.any(|queued_at| queued_at > limit_at_ms)
}

/// Sent when a limit ended an agent-initiated turn, which has no prompt to redeliver.
const AGENT_TURN_CONTINUATION: &str = "continue";

/// What the continuation producer decided for one session (#4092).
#[must_use]
#[derive(Debug, Clone, Copy)]
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

pub(crate) struct PreparedContinuation {
    session_id: String,
    created_at: crate::time::Timestamp,
    pub(crate) prompt: Option<(String, Vec<crate::daemon::PromptAttachmentRef>)>,
    limit_at_ms: Option<i64>,
}

impl PreparedContinuation {
    pub(crate) async fn outcome(
        &self,
        state: &Arc<AppState>,
        id: &str,
        _submission: &tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<ContinuationOutcome, SupervisorError> {
        let rows = state.instances.read().await;
        let row = rows
            .iter()
            .find(|row| {
                self.session_id == id
                    && row.id == id
                    && row.created_at == self.created_at
                    && row.terminal_handoff.is_none()
            })
            .ok_or_else(|| SupervisorError::SpawnCancelled(id.to_owned()))?;
        Ok(
            if self.prompt.is_some()
                && queue_supersedes(
                    row.queued_prompts.iter().filter_map(row_minted_at_ms),
                    self.limit_at_ms,
                )
            {
                ContinuationOutcome::SupersededByQueue
            } else if self.prompt.is_some() {
                ContinuationOutcome::Installed
            } else {
                ContinuationOutcome::Stands
            },
        )
    }
}

pub(crate) async fn resolve_continuation(
    state: &Arc<AppState>,
    id: &str,
) -> Result<PreparedContinuation, SupervisorError> {
    let interrupted = query_control(
        &state.acp_event_store,
        id,
        "rate-limit continuation",
        |store, id| store.rate_limited_turn_prompt(id),
    )
    .await?;
    let prompt = match interrupted {
        Known(Some(prompt)) => Some(prompt),
        Known(None) => match query_control(
            &state.acp_event_store,
            id,
            "rate-limit agent-turn continuation",
            |store, id| store.rate_limit_interrupted_agent_turn(id),
        )
        .await?
        {
            Known(true) => Some((AGENT_TURN_CONTINUATION.to_owned(), Vec::new())),
            Known(false) => None,
            Indeterminate => return Err(SupervisorError::ControlStateUnavailable),
        },
        Indeterminate => return Err(SupervisorError::ControlStateUnavailable),
    };
    let limit_at_ms = if prompt.is_some() {
        query_control(
            &state.acp_event_store,
            id,
            "rate-limit supersession",
            |store, id| Ok(store.latest_rate_limit_event(id)?.map(|(_, at)| at)),
        )
        .await?
    } else {
        None
    };
    #[cfg(test)]
    state.session_service.await_install_barrier(id).await;
    let created_at = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id && row.terminal_handoff.is_none())
        .ok_or_else(|| SupervisorError::SpawnCancelled(id.to_owned()))?
        .created_at;
    Ok(PreparedContinuation {
        session_id: id.to_owned(),
        created_at,
        prompt,
        limit_at_ms,
    })
}

#[cfg(test)]
pub(crate) async fn install_rate_limit_continuation(
    state: &Arc<AppState>,
    id: &str,
    submission: &tokio::sync::OwnedMutexGuard<()>,
) -> Result<ContinuationOutcome, SupervisorError> {
    let prepared = resolve_continuation(state, id).await?;
    let outcome = prepared.outcome(state, id, submission).await?;
    install_prepared_continuation(state, id, submission, prepared, outcome).await
}

pub(crate) async fn install_prepared_continuation(
    state: &Arc<AppState>,
    id: &str,
    _submission: &tokio::sync::OwnedMutexGuard<()>,
    prepared: PreparedContinuation,
    outcome: ContinuationOutcome,
) -> Result<ContinuationOutcome, SupervisorError> {
    if prepared.session_id != id
        || !state.instances.read().await.iter().any(|row| {
            row.id == id && row.created_at == prepared.created_at && row.terminal_handoff.is_none()
        })
    {
        return Err(SupervisorError::SpawnCancelled(id.to_owned()));
    }
    match outcome {
        ContinuationOutcome::SupersededByQueue => {
            state.session_service.clear_pending_initial_turn(id).await;
        }
        ContinuationOutcome::Installed => {
            let (text, attachments) = prepared
                .prompt
                .ok_or(SupervisorError::ControlStateUnavailable)?;
            state
                .session_service
                .set_pending_initial_turn(id, text, attachments, crate::daemon::PromptOrigin::Aoe)
                .await;
        }
        ContinuationOutcome::Stands => {}
    }
    Ok(outcome)
}

pub(crate) struct QuotaDelivery {
    park_seq: u64,
}

impl QuotaDelivery {
    pub(crate) fn new(park_seq: u64) -> Self {
        Self { park_seq }
    }

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
        QuotaResume::LoginQueue { park_seq } if park.seq != park_seq => return Ok(()),
        _ => {}
    }
    let (profile, generation) = {
        let rows = state.instances.read().await;
        rows.iter()
            .find(|row| row.id == id && is_resumable(row))
            .map(|row| (row.source_profile.clone(), row.lifecycle_generation))
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
    let Known(streak) = query_control(
        &state.acp_event_store,
        id,
        "quota release budget",
        |store, id| store.rate_limit_redelivery_streak(id),
    )
    .await?
    else {
        return Err(SupervisorError::ControlStateUnavailable);
    };
    let mut resume_at = park
        .info
        .as_ref()
        .and_then(|info| info.resets_at)
        .unwrap_or_else(Timestamp::now);
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
        let calendar = ParkCalendar::new(&park);
        let reported = if failover_settings.enabled {
            let row = {
                let rows = state.instances.read().await;
                rows.iter()
                    .find(|row| row.id == id && is_resumable(row))
                    .cloned()
            };
            match row {
                Some(row) => failover::reported_wake(state, &row, &calendar.view()).await,
                None => return Err(SupervisorError::SpawnCancelled(id.to_owned())),
            }
        } else {
            failover::Reported::default()
        };
        resume_at = calendar.scheduled_at(&reported, streak);
        if let Some(attempt) = park.last_resume_attempt_ms.and_then(Timestamp::from_millis) {
            resume_at =
                resume_at.max(attempt + crate::time::Duration::seconds(RATE_LIMIT_MIN_PARK_SECS));
        }
        if Timestamp::now() < resume_at {
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
            .ok_or(SupervisorError::NativeStoreUnavailable)?
            .map_err(|_| SupervisorError::NativeStoreUnavailable)?;
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
        Some(resolve_continuation(state, id).await?)
    };
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
        ContinuationOutcome::SupersededByQueue
    } else {
        prepared
            .as_ref()
            .ok_or(SupervisorError::ControlStateUnavailable)?
            .outcome(state, id, &submission)
            .await?
    };
    let manual = matches!(kind, QuotaResume::Manual);
    if !state
        .instances
        .read()
        .await
        .iter()
        .any(|row| row.id == id && row.lifecycle_generation == generation && is_resumable(row))
    {
        return Err(SupervisorError::SpawnCancelled(id.to_owned()));
    }
    if let Some(unavailable) = state
        .session_service
        .prompt_native_unavailability(id)
        .await
        .map_err(|_| SupervisorError::ControlStateUnavailable)?
    {
        return Err(crate::acp::acp_client::AcpError::ConnectionUnavailable(unavailable).into());
    }
    if manual
        || matches!(
            outcome,
            ContinuationOutcome::Stands | ContinuationOutcome::Installed
        )
    {
        if manual && park.cap_reached {
            resume_at = Timestamp::now();
        }
        state
            .acp_supervisor
            .publish_rate_limit_auto_resumed(id, resume_at, manual)?;
    }
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
    let outcome = match prepared {
        Some(prepared) => {
            install_prepared_continuation(state, id, &submission, prepared, outcome).await?
        }
        None => {
            state.session_service.clear_pending_initial_turn(id).await;
            outcome
        }
    };
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

    let now = Timestamp::now();
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
        if !matches!(
            state
                .session_service
                .prompt_native_unavailability(&id)
                .await,
            Ok(None)
        ) {
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
        let calendar = ParkCalendar::new(&park);
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
        let account = if failover_settings.enabled {
            failover::reported_wake(state, &row, &calendar.view()).await
        } else {
            failover::Reported::default()
        };
        let scheduled_at = calendar.scheduled_at(&account, streak);
        // A park a swap or resume already touched belongs to the account it ran on then.
        let first_sight = park.last_resume_attempt_ms.is_none();
        if workerless && failover_settings.enabled && first_sight {
            if let Some((evidence, until)) = calendar.evidence(scheduled_at, &account) {
                failover::record_limit(state, &row, &evidence, until, park.observed_at_ms).await;
            }
        }
        let Ok(latest_seq) = query_control(
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
        if let Some(last_attempt) = park.last_resume_attempt_ms.and_then(Timestamp::from_millis) {
            resume_at = resume_at
                .max(last_attempt + crate::time::Duration::seconds(RATE_LIMIT_MIN_PARK_SECS));
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
        enable_auto_resume, queued_prompt, startup_errors, structured_instance, test_state, Tick,
    };
    use super::*;
    use crate::acp::event_store::ControlKnowledge::{Indeterminate, Known};
    use crate::acp::supervisor::test_support::managed::with_managed;
    use crate::acp::Event;
    use crate::server::test_support::managed_control_session;

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
        let recorded_at = crate::time::Timestamp::from_unix_secs(1_500_000).unwrap();
        let at = |n| rate_limit_unknown_reset_retry_at(recorded_at.as_millis(), n) - recorded_at;
        let hour = crate::time::Duration::seconds(RATE_LIMIT_UNKNOWN_RESET_RETRY_SECS);
        for (n, factor) in [(0, 1), (1, 2), (2, 4), (3, 8), (4, 16)] {
            assert_eq!(at(n), hour * factor);
        }
        let total = (0..RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES)
            .fold(crate::time::Duration::ZERO, |acc, n| acc + at(n));
        assert_eq!(total, hour * 31);
        assert_eq!(at(RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES + 50), hour * 16);

        let recorded = crate::time::Timestamp::from_unix_secs(1_000_000).unwrap();
        let ms = recorded.as_millis();
        let secs = crate::time::Duration::seconds;
        let cases = [
            (
                "far future reset",
                recorded + crate::time::Duration::hours(1),
                15,
                recorded + crate::time::Duration::hours(1) + secs(15),
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
        let limit = crate::time::Timestamp::from_unix_secs(1_700_000_000).unwrap();
        let limit_ms = limit.as_millis();
        let row = |seq: u64, created_at: &str| crate::daemon::QueuedPromptEntry {
            id: format!("q-{seq}"),
            seq,
            text: String::new(),
            attachments: Vec::new(),
            created_at: created_at.into(),
            origin_device: None,
            origin_plugin: None,
            origin: crate::daemon::PromptOrigin::Unknown,
        };
        let after = crate::time::Timestamp::from_unix_secs(1_700_000_001)
            .unwrap()
            .to_rfc3339();
        let before = crate::time::Timestamp::from_unix_secs(1_699_999_999)
            .unwrap()
            .to_rfc3339();
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
                queue_supersedes(rows.iter().filter_map(row_minted_at_ms), recorded),
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
                let mut live =
                    managed_control_session(scope, "quota-backoff", "claude", None).await;
                let id = live.id().to_owned();
                enable_auto_resume(true);
                live.park("run the nightly task").await;
                let before = live.state.acp_event_store.highest_seq(&id).unwrap();
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
                assert_eq!(live.state.acp_event_store.highest_seq(&id).unwrap(), before);
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
                let frontier = live.state.acp_event_store.highest_seq(&id).unwrap();
                let mut attempted = HashSet::from([id.clone()]);
                reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()).await;
                settle_delivery(&live.state, &id).await;
                assert!(attempted.contains(&id));
                assert_eq!(
                    live.state.acp_event_store.highest_seq(&id).unwrap(),
                    frontier
                );
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
                let mut live = managed_control_session(scope, "quota-cap", "claude", None).await;
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
                let frontier = live.state.acp_event_store.highest_seq(&id).unwrap();
                let held = live.state.session_service.prompt_submission(&id).await;
                let mut attempted = HashSet::from([id.clone()]);
                tokio::time::timeout(
                    Duration::from_secs(5),
                    reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()),
                )
                .await
                .unwrap();
                assert!(attempted.contains(&id));
                assert_eq!(
                    live.state.acp_event_store.highest_seq(&id).unwrap(),
                    frontier
                );
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
                let cap_frontier = live.state.acp_event_store.highest_seq(&id).unwrap();
                reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()).await;
                settle_delivery(&live.state, &id).await;
                assert_eq!(
                    live.state.acp_event_store.highest_seq(&id).unwrap(),
                    cap_frontier
                );
                live.state
                    .session_service
                    .enqueue_prompt(
                        &id,
                        "after-cap".into(),
                        "new user work".into(),
                        Vec::new(),
                        None,
                        crate::server::session_service::QueueSender::default(),
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
                    let mut live = managed_control_session(scope, label, "claude", None).await;
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
                                crate::daemon::PromptOrigin::Unknown,
                            )
                            .await;
                    }
                    let park = known_park(&live.state, &id);
                    let mut queued = queued_prompt();
                    queued.text = "manual prompt B".into();
                    queued.created_at = offset
                        .map(|offset| {
                            crate::time::Timestamp::from_millis(park.recorded_at_ms + offset)
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
                    let frontier = live.state.acp_event_store.highest_seq(&id).unwrap();
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
                let mut live = managed_control_session(scope, fault, "claude", None).await;
                let id = live.id().to_owned();
                enable_auto_resume(true);
                live.park("original interrupted work").await;
                live.make_quota_due();
                live.state.session_service.set_pending_initial_turn(&id, "original interrupted work".into(), Vec::new(), crate::daemon::PromptOrigin::Unknown).await;
                let park = known_park(&live.state, &id);
                let before = live.state.instances.read().await.iter().find(|row| row.id == id).unwrap().clone();
                let requests = live.native.requests();
                let frontier = live.state.acp_event_store.highest_seq(&id).unwrap();
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
                assert_eq!(live.state.acp_event_store.highest_seq(&id).unwrap(), frontier);
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
    async fn quota_publication_io_failure_installs_no_worker_or_new_pending_turn() {
        with_managed(|scope| {
            Box::pin(async move {
                for pending in [false, true] {
                    let mut live = managed_control_session(scope, if pending {
                        "quota-io-existing-pending"
                    } else {
                        "quota-io-no-pending"
                    }, "claude", None)
                    .await;
                    let id = live.id().to_owned();
                    enable_auto_resume(true);
                    live.park("original interrupted work").await;
                    live.make_quota_due();
                    if pending {
                        live.state
                            .session_service
                            .set_pending_initial_turn(
                                &id,
                                "existing pending work".into(),
                                Vec::new(),
                                crate::daemon::PromptOrigin::Unknown,
                            )
                            .await;
                    }
                    let park = known_park(&live.state, &id);
                    let budget = live
                        .state
                        .acp_event_store
                        .rate_limit_redelivery_streak(&id)
                        .unwrap();
                    let original = {
                        let registry = crate::process::worker_registry::registry_lock_until(
                            &id, tokio::time::Instant::now() + Duration::from_secs(10),
                        ).await.expect("the actual runner releases registry admission");
                        registry.record().unwrap().expect("the original runner remains registered")
                    };
                    assert!(original.detached_at.is_none());
                    let original_identity =
                        crate::acp::runner_lifecycle::RunnerIdentity::from_record(&original);
                    let original_birth = crate::process::process_birth_identity(original.pid)
                        .unwrap()
                        .expect("the original runner has a real kernel birth");
                    assert_eq!(
                        original_identity.birth_fingerprint,
                        Some(original_birth.fingerprint())
                    );
                    let original_lease = live.state.acp_supervisor.current_lease(&id).unwrap();
                    let original_actual = live.client.actual();
                    let available = live
                        .state
                        .acp_supervisor
                        .current_native_availability(&id)
                        .await
                        .expect("this supervisor owns the original installed Native client");
                    assert_eq!(available.lease, original_lease);
                    assert_eq!(available.owner, Some(original_identity));
                    assert!(std::sync::Arc::ptr_eq(&available.client, &original_actual));
                    assert!(original_actual.ensure_available().is_ok());
                    live.client = scope
                        .adopt_with(live.client, false, || async {
                            assert_eq!(live.state.acp_supervisor.count().await, 0);
                            assert!(live
                                .state
                                .acp_supervisor
                                .current_native_availability(&id)
                                .await
                                .is_none());
                            assert_eq!(
                                live.state.acp_supervisor.current_lease(&id).as_ref(),
                                Some(&original_lease)
                            );
                            assert!(original_actual.ensure_available().is_err());
                            // SDK shutdown closes its socket before the runner records detach.
                            // Wait for that write so quota admission does not race its registry lock.
                            tokio::time::timeout(Duration::from_secs(10), async {
                                loop {
                                    let record = crate::process::worker_registry::load(&id)
                                        .unwrap()
                                        .expect("the original runner remains registered");
                                    if record.detached_at.is_some() {
                                        break;
                                    }
                                    tokio::task::yield_now().await;
                                }
                            })
                            .await
                            .expect("the original runner committed its detach");
                            let retained = {
                                let registry = crate::process::worker_registry::registry_lock_until(
                                    &id, tokio::time::Instant::now() + Duration::from_secs(10),
                                ).await.expect("the actual runner releases registry admission");
                                registry.record().unwrap().expect("the original runner remains registered")
                            };
                            assert_eq!(
                                crate::acp::runner_lifecycle::RunnerIdentity::from_record(&retained),
                                original_identity
                            );
                            assert!(crate::process::process_birth_is_live(
                                original.pid,
                                &original_birth
                            )
                            .unwrap());
                            assert!(
                                crate::process::worker_registry::retained_process_may_be_alive(
                                    &original
                                )
                            );
                            let requests = live.native.requests();
                            let frontier = live.state.acp_event_store.highest_seq(&id).unwrap();
                            assert_eq!(
                                live.state
                                    .instances
                                    .read()
                                    .await
                                    .iter()
                                    .find(|row| row.id == id)
                                    .unwrap()
                                    .pending_initial_turn
                                    .as_ref()
                                    .map(|turn| turn.text.as_str()),
                                pending.then_some("existing pending work")
                            );
                            live.state.acp_event_store.test_conn().execute_batch(
                    "CREATE TEMP TRIGGER fail_quota_publication BEFORE INSERT ON acp_events
                     WHEN NEW.discriminant = 'RateLimitAutoResumed'
                     BEGIN SELECT RAISE(ABORT, 'genuine quota publication IO failure'); END;"
                ).unwrap();
                            for kind in [
                                QuotaResume::Manual,
                                QuotaResume::Automatic { park_seq: park.seq },
                            ] {
                                let submission =
                                    live.state.session_service.prompt_submission(&id).await;
                                let result =
                                    resume_rate_limited(&live.state, &id, submission, kind).await;
                                assert!(
                                    matches!(result, Err(SupervisorError::NativeStoreUnavailable)),
                                    "{result:?}"
                                );
                                assert_eq!(
                                    live.state.acp_supervisor.count().await,
                                    0,
                                    "failed durable publication must precede any worker installation"
                                );
                                assert!(live
                                    .state
                                    .acp_supervisor
                                    .current_native_availability(&id)
                                    .await
                                    .is_none());
                                assert_eq!(
                                    live.state.acp_supervisor.current_lease(&id).as_ref(),
                                    Some(&original_lease)
                                );
                                assert!(original_actual.ensure_available().is_err());
                                assert_eq!(
                                    live.state
                                        .instances
                                        .read()
                                        .await
                                        .iter()
                                        .find(|row| row.id == id)
                                        .unwrap()
                                        .pending_initial_turn
                                        .as_ref()
                                        .map(|turn| turn.text.as_str()),
                                    pending.then_some("existing pending work")
                                );
                                let profile = live
                                    .state
                                    .instances
                                    .read()
                                    .await
                                    .iter()
                                    .find(|row| row.id == id)
                                    .unwrap()
                                    .source_profile
                                    .clone();
                                let durable = crate::session::Storage::open_unwatched(&profile)
                                    .unwrap()
                                    .load_instances_complete()
                                    .unwrap()
                                    .into_iter()
                                    .find(|row| row.id == id)
                                    .unwrap();
                                assert_eq!(
                                    durable
                                        .pending_initial_turn
                                        .as_ref()
                                        .map(|turn| turn.text.as_str()),
                                    pending.then_some("existing pending work"),
                                    "genuine SQL refusal must preserve durable pending intent too"
                                );
                                assert_eq!(live.native.requests(), requests,
                        "failed publication must neither attach nor deliver autonomous input");
                                assert_eq!(live.state.acp_event_store.highest_seq(&id).unwrap(), frontier);
                                assert_eq!(
                                    live.state
                                        .acp_event_store
                                        .rate_limit_redelivery_streak(&id)
                                        .unwrap(),
                                    budget
                                );
                                assert_eq!(known_park(&live.state, &id).seq, park.seq);
                                assert_eq!(auto_resumed_breadcrumbs(&live.state, &id), 0);
                                let retained = {
                                    let registry = crate::process::worker_registry::registry_lock_until(
                                        &id, tokio::time::Instant::now() + Duration::from_secs(10),
                                    ).await.expect("the actual runner releases registry admission");
                                    registry.record().unwrap().expect("the original runner remains registered")
                                };
                                assert_eq!(
                                    crate::acp::runner_lifecycle::RunnerIdentity::from_record(
                                        &retained
                                    ),
                                    original_identity
                                );
                                assert!(crate::process::process_birth_is_live(
                                    original.pid,
                                    &original_birth
                                )
                                .unwrap());
                                assert!(
                                    crate::process::worker_registry::retained_process_may_be_alive(
                                        &retained
                                    )
                                );
                            }
                            live.state
                                .acp_event_store
                                .test_conn()
                                .execute_batch("DROP TRIGGER fail_quota_publication")
                                .unwrap();
                        })
                        .await;
                    live.client.ready().await;
                    live.client.stop_session().await;
                }
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn retained_unknown_history_allows_only_read_only_attach_then_distinct_explicit_input() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut live =
                    managed_control_session(scope, "quota-unknown-retained", "claude", None).await;
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
                // Usability precedes the final assignment/capability publications.
                // Observe those actual durable frames before the no-auto-resume frontier.
                tokio::time::timeout(Duration::from_secs(10), async {
                    let (mut assigned, mut capabilities) = (false, false);
                    while !assigned || !capabilities {
                        match live
                            .client
                            .next_event()
                            .await
                            .expect("actual retained handshake publication")
                        {
                            Event::AcpSessionAssigned { acp_session_id, .. }
                                if acp_session_id == live.sid =>
                            {
                                assigned = true
                            }
                            Event::PromptCapabilities { .. } => capabilities = true,
                            Event::AgentStartupError { message, .. } => panic!("{message}"),
                            Event::AcpConnectionUnavailable { .. } => {
                                panic!("the original retained channel stays live")
                            }
                            _ => {}
                        }
                    }
                })
                .await
                .expect("the retained handshake publications precede its negative frontier");
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
                let frontier = live.state.acp_event_store.highest_seq(&id).unwrap();
                let mut attempted = HashSet::from([id.clone()]);
                reap_rate_limit_resumes(&live.state, &mut attempted, &HashSet::new()).await;
                assert_eq!(
                    live.state.acp_event_store.highest_seq(&id).unwrap(),
                    frontier
                );
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
                let mut live =
                    managed_control_session(scope, "quota-native-loss", "claude", None).await;
                let id = live.id().to_owned();
                live.park("original work").await;
                live.state
                    .session_service
                    .set_pending_initial_turn(
                        &id,
                        "original work".into(),
                        Vec::new(),
                        crate::daemon::PromptOrigin::Unknown,
                    )
                    .await;
                let before = live.state.acp_event_store.highest_seq(&id).unwrap();
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
                    assert_eq!(live.state.acp_event_store.highest_seq(&id).unwrap(), before);
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
    async fn native_loss_during_continuation_preparation_does_not_publish_a_resume() {
        with_managed(|scope| {
            Box::pin(async move {
                for automatic in [false, true] {
                    let mut live =
                        managed_control_session(scope, "quota-loss-during-prepare", "claude", None)
                            .await;
                    let id = live.id().to_owned();
                    enable_auto_resume(true);
                    live.park("preserve original work").await;
                    live.make_quota_due();
                    live.state
                        .session_service
                        .set_pending_initial_turn(
                            &id,
                            "existing pending work".into(),
                            Vec::new(),
                            crate::daemon::PromptOrigin::Unknown,
                        )
                        .await;
                    if !automatic {
                        let submission = live.state.session_service.prompt_submission(&id).await;
                        live.state
                            .session_service
                            .enqueue_prompt(
                                &id,
                                "retained-queue".into(),
                                "queued work must remain".into(),
                                Vec::new(),
                                None,
                                crate::server::session_service::QueueSender::default(),
                            )
                            .await
                            .unwrap();
                        drop(submission);
                    }
                    let park_seq = known_park(&live.state, &id).seq;
                    let frontier = live.state.acp_event_store.highest_seq(&id).unwrap();
                    let budget = live
                        .state
                        .acp_event_store
                        .rate_limit_redelivery_streak(&id)
                        .unwrap();
                    let requests = live.native.requests();
                    let lease = live.state.acp_supervisor.current_lease(&id).unwrap();
                    let before = live
                        .state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .clone();
                    let mut barrier = live.state.session_service.arm_install_barrier();
                    let state = live.state.clone();
                    let target = id.clone();
                    let release_task = tokio::spawn(async move {
                        let submission = state.session_service.prompt_submission(&target).await;
                        let kind = if automatic {
                            QuotaResume::Automatic { park_seq }
                        } else {
                            QuotaResume::Manual
                        };
                        resume_rate_limited(&state, &target, submission, kind).await
                    });
                    let (observed, release) =
                        tokio::time::timeout(Duration::from_secs(10), barrier.recv())
                            .await
                            .unwrap()
                            .unwrap();
                    assert_eq!(observed, id);
                    live.client.actual().test_observe_native_loss();
                    release.send(()).unwrap();
                    let result = release_task.await.unwrap();
                    assert!(
                        matches!(
                            result,
                            Err(SupervisorError::Acp(
                                crate::acp::acp_client::AcpError::ConnectionUnavailable(_)
                            ))
                        ),
                        "{result:?}"
                    );
                    let after = live
                        .state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .clone();
                    assert_eq!(
                        live.state.acp_event_store.highest_seq(&id).unwrap(),
                        frontier
                    );
                    assert_eq!(
                        live.state
                            .acp_event_store
                            .rate_limit_redelivery_streak(&id)
                            .unwrap(),
                        budget
                    );
                    assert_eq!(known_park(&live.state, &id).seq, park_seq);
                    assert_eq!(auto_resumed_breadcrumbs(&live.state, &id), 0);
                    assert_eq!(
                        serde_json::to_value(after.pending_initial_turn).unwrap(),
                        serde_json::to_value(before.pending_initial_turn).unwrap()
                    );
                    assert_eq!(
                        serde_json::to_value(after.queued_prompts).unwrap(),
                        serde_json::to_value(before.queued_prompts).unwrap()
                    );
                    assert_eq!(live.native.requests(), requests);
                    assert_eq!(live.state.acp_supervisor.current_lease(&id), Some(lease));
                    drop(barrier);
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
                let mut live =
                    managed_control_session(scope, "quota-idle-hold", "claude", None).await;
                let id = live.id().to_owned();
                live.park("original work").await;
                std::fs::write(
                    crate::session::get_app_dir().unwrap().join("config.toml"),
                    "[acp]\nrate_limit_auto_resume = true\nauto_stop_idle_secs = 1\n",
                )
                .unwrap();
                live.client.lease.test_backdate_idle_baseline(
                    (crate::time::Timestamp::now() - crate::time::Duration::days(1)).as_millis(),
                );
                live.make_quota_due();
                let frontier = live.state.acp_event_store.highest_seq(&id).unwrap();
                let requests = live.native.requests();
                super::super::idle::reap_idle_workers(&live.state).await;
                assert_eq!(
                    live.state.acp_event_store.highest_seq(&id).unwrap(),
                    frontier
                );
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
            .prompt
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
        assert!(resolve_continuation(&state, id)
            .await
            .unwrap()
            .prompt
            .is_none());
        assert_eq!(
            state
                .acp_event_store
                .rate_limit_redelivery_streak(id)
                .unwrap(),
            Known(0)
        );
        state.shutdown.cancel();
    }
    /// Two accounts of one agent, failover on with `claude-b` in the pool, and the
    /// session's row on `claude-a`.
    async fn calendar_session(
        id: &str,
    ) -> (
        crate::session::test_support::AppDirGuard,
        Arc<AppState>,
        tempfile::TempDir,
        crate::tmux::status_rules::ProfileRegistryGuard,
    ) {
        let (home, state, project) = test_state(id);
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

    /// Midnight UTC of the day `at` falls on, and that day as a status text names it ("Nov 18").
    fn utc_midnight(at: Timestamp) -> (Timestamp, String) {
        let midnight =
            Timestamp::from_unix_secs(at.as_unix_secs().div_euclid(86_400) * 86_400).unwrap();
        let day = jiff::Timestamp::from_second(midnight.as_unix_secs())
            .unwrap()
            .to_zoned_datetime(jiff::tz::TimeZone::UTC)
            .date();
        (midnight, day.strftime("%b %-d").to_string())
    }

    fn record_calendar(
        state: &AppState,
        id: &str,
        reports: impl IntoIterator<Item = (RateLimitInfo, i64, Option<(i64, &'static str)>)>,
    ) {
        use crate::acp::event_sender::EventMetadata;
        use crate::acp::protocol::{
            AcpOriginalRequestResult, AcpPromptObservation, AcpPromptWindowId,
        };
        let store = &state.acp_event_store;
        let mut seq = store.highest_seq(id).unwrap();
        let mut metadata = crate::server::test_support::sql_projection_metadata();
        let mut reports = reports.into_iter().peekable();
        let at = reports.peek().expect("a calendar report").1;
        let prompt = crate::acp::state::test_support::prompt("run the nightly task");
        seq += 1;
        store
            .record_quota_calendar_at(id, seq, &prompt, &EventMetadata::host(&prompt), at, None)
            .unwrap();
        seq += 1;
        store
            .record_quota_calendar_at(id, seq, &Event::ThinkingStarted, &metadata, at, None)
            .unwrap();
        metadata.operation.as_mut().unwrap().outcome = Some(AcpOriginalRequestResult::Failed);
        metadata.prompt_observation = Some(AcpPromptObservation::Written {
            window_id: AcpPromptWindowId::new(),
        });
        let mut last = at;
        for (info, at, inherited) in reports {
            seq += 1;
            store
                .record_quota_calendar_at(
                    id,
                    seq,
                    &Event::RateLimit { info },
                    &metadata,
                    at,
                    inherited,
                )
                .unwrap();
            last = at;
        }
        let stopped = crate::acp::state::test_support::stopped("rate_limited");
        seq += 1;
        store
            .record_quota_calendar_at(
                id,
                seq,
                &stopped,
                &EventMetadata::host(&stopped),
                last,
                None,
            )
            .unwrap();
        state.acp_supervisor.hydrate_seqs([(id.to_owned(), seq)]);
        assert_eq!(known_park(state, id).recorded_at_ms, last);
    }

    async fn reap_calendar(
        state: &Arc<AppState>,
        attempted: &mut HashSet<String>,
    ) -> HashSet<String> {
        let before = attempted.clone();
        reap_rate_limit_resumes(state, attempted, &HashSet::new()).await;
        let released: HashSet<_> = before.difference(attempted).cloned().collect();
        for id in &released {
            settle_delivery(state, id).await;
        }
        released
    }

    fn park_at(
        state: &AppState,
        id: &str,
        at_ms: i64,
        info: RateLimitInfo,
        inherited: Option<(i64, &'static str)>,
    ) {
        record_calendar(state, id, [(info, at_ms, inherited)]);
    }

    /// A weekly park stored with no `resets_at` still names its reset in the status: the
    /// resume waits for it, not the 1h estimate, and the account records that window as reported.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_undated_weekly_park_waits_for_the_reset_its_status_names() {
        let id = "sess-failover-weekly-text";
        let (_home, state, _project, _registry) = calendar_session(id).await;
        let two_hours_ago = (Timestamp::now() - crate::time::Duration::hours(2)).as_millis();
        let (reset, day) = utc_midnight(Timestamp::now() + crate::time::Duration::days(2));
        let status =
            format!("Internal error: You've hit your weekly limit · resets {day}, 12am (UTC)");
        record_calendar(
            &state,
            id,
            [(
                RateLimitInfo {
                    status,
                    resets_at: None,
                    kind: "rate_limit".into(),
                },
                two_hours_ago,
                None,
            )],
        );
        let now_ms = Timestamp::now().as_millis();
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-b"),
            now_ms + 3_600_000,
            "usage",
            now_ms,
            false,
        );
        let mut attempted: HashSet<String> = [id.to_string()].into();

        let released = reap_calendar(&state, &mut attempted).await;

        assert!(
            released.is_empty() && attempted.contains(id),
            "the 1h estimate elapsed an hour ago; the weekly reset is two days out"
        );
        let stored = state
            .acp_event_store
            .account_state(&account_key_of("claude-a"))
            .unwrap();
        let window = stored.window("seven_day").unwrap();
        assert_eq!(
            (window.until_ms, window.estimated),
            (
                (reset
                    + crate::time::Duration::seconds(i64::from(RATE_LIMIT_AUTO_RESUME_GRACE_SECS)))
                .as_millis(),
                false
            )
        );
    }

    /// A later report for the account's window extended the deadline past the one an old
    /// park's status names: the old text must not release the session early.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_status_reset_never_outranks_a_newer_reported_deadline() {
        let id = "sess-failover-text-vs-report";
        let (_home, state, _project, _registry) = calendar_session(id).await;
        let recorded = Timestamp::now() - crate::time::Duration::days(3);
        let (_, day) = utc_midnight(recorded + crate::time::Duration::days(1));
        let status =
            format!("Internal error: You've hit your weekly limit · resets {day}, 12am (UTC)");
        record_calendar(
            &state,
            id,
            [(
                RateLimitInfo {
                    status,
                    resets_at: None,
                    kind: "rate_limit".into(),
                },
                recorded.as_millis(),
                None,
            )],
        );
        let now_ms = Timestamp::now().as_millis();
        for tool in ["claude-a", "claude-b"] {
            state.acp_event_store.record_account_limit(
                &account_key_of(tool),
                now_ms + 24 * 3_600_000,
                "seven_day",
                now_ms - 1_000,
                false,
            );
        }
        let mut attempted: HashSet<String> = [id.to_string()].into();

        let released = reap_calendar(&state, &mut attempted).await;

        assert!(
            released.is_empty() && attempted.contains(id),
            "the text named a reset two days ago; the account reported one a day from now"
        );
    }

    /// A park that already carries a reset (parsed or captured) still yields to a later report
    /// of the same window on the account, and an unnamed report counts for a named park.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_dated_park_yields_to_a_later_reported_deadline() {
        for (reported_kind, id) in [
            ("seven_day", "sess-dated-named"),
            ("rate_limit", "sess-dated-unnamed"),
        ] {
            let (_home, state, _project, _registry) = calendar_session(id).await;
            let recorded = Timestamp::now() - crate::time::Duration::hours(2);
            record_calendar(
                &state,
                id,
                [(
                    RateLimitInfo {
                        status: "You've hit your weekly limit".into(),
                        resets_at: Some(Timestamp::now() - crate::time::Duration::hours(1)),
                        kind: "seven_day".into(),
                    },
                    recorded.as_millis(),
                    None,
                )],
            );
            let now_ms = Timestamp::now().as_millis();
            for tool in ["claude-a", "claude-b"] {
                state.acp_event_store.record_account_limit(
                    &account_key_of(tool),
                    now_ms + 24 * 3_600_000,
                    reported_kind,
                    now_ms - 1_000,
                    false,
                );
            }
            let mut attempted: HashSet<String> = [id.to_string()].into();

            let released = reap_calendar(&state, &mut attempted).await;

            assert!(
                released.is_empty() && attempted.contains(id),
                "{reported_kind}: the park's own reset passed an hour ago, the account's runs a day more"
            );
        }
    }

    /// Of a dated park and the account's report for its window, the one observed last stands,
    /// whichever way it moves the deadline.
    #[tokio::test]
    #[serial_test::serial]
    async fn the_later_observation_of_a_window_wins_in_both_directions() {
        const HOUR_MS: i64 = 3_600_000;
        // (id, park reset, report deadline, report observed, session is released)
        // All offsets are in hours from now; the park was recorded two hours ago.
        for (id, park_reset, report_until, report_observed, released) in [
            ("sess-obs-report-extends", 10, 24, -1, false),
            ("sess-obs-report-corrects-down", 10, -1, -1, true),
            ("sess-obs-park-corrects-down", -1, 24, -3, true),
            ("sess-obs-park-corrects-up", 10, 1, -3, false),
        ] {
            let (_home, state, _project, _registry) = calendar_session(id).await;
            let recorded = Timestamp::now() - crate::time::Duration::hours(2);
            record_calendar(
                &state,
                id,
                [(
                    RateLimitInfo {
                        status: "You've hit your weekly limit".into(),
                        resets_at: Some(
                            Timestamp::now() + crate::time::Duration::hours(park_reset),
                        ),
                        kind: "seven_day".into(),
                    },
                    recorded.as_millis(),
                    None,
                )],
            );
            let now_ms = Timestamp::now().as_millis();
            state.acp_event_store.record_account_limit(
                &account_key_of("claude-a"),
                now_ms + report_until * HOUR_MS,
                "seven_day",
                now_ms + report_observed * HOUR_MS,
                false,
            );
            // The other account stays limited in another window, so no failover takes the park.
            state.acp_event_store.record_account_limit(
                &account_key_of("claude-b"),
                now_ms + 24 * HOUR_MS,
                "other_window",
                now_ms,
                false,
            );
            let mut attempted: HashSet<String> = [id.to_string()].into();

            let got = reap_calendar(&state, &mut attempted).await;

            assert_eq!(got.contains(id), released, "{id}");
        }
    }

    /// A legacy park whose text names only a clock time was recorded after that time passed (a
    /// delayed write): reading it against the recorded time would roll the reset to tomorrow, so
    /// it keeps the timed schedule and the account holds an estimate.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_legacy_clock_only_park_recorded_after_its_reset_keeps_the_timed_schedule() {
        let id = "sess-legacy-clock-only";
        let (_home, state, _project, _registry) = calendar_session(id).await;
        let recorded = Timestamp::now() - crate::time::Duration::hours(3);
        let clock = jiff::Timestamp::from_second(
            (recorded - crate::time::Duration::hours(1)).as_unix_secs(),
        )
        .unwrap()
        .to_zoned_datetime(jiff::tz::TimeZone::UTC)
        .strftime("%-I:%M%p")
        .to_string();
        let status = format!("Internal error: You've hit your limit · resets {clock} (UTC)");
        record_calendar(
            &state,
            id,
            [(
                RateLimitInfo {
                    status,
                    resets_at: None,
                    kind: "rate_limit".into(),
                },
                recorded.as_millis(),
                None,
            )],
        );
        let now_ms = Timestamp::now().as_millis();
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-b"),
            now_ms + 3_600_000,
            "other_window",
            now_ms,
            false,
        );
        let mut attempted: HashSet<String> = [id.to_string()].into();

        assert!(reap_calendar(&state, &mut attempted).await.contains(id));

        let stored = state
            .acp_event_store
            .account_state(&account_key_of("claude-a"))
            .unwrap();
        let window = stored.window("rate_limit").unwrap();
        assert!(window.estimated, "no reset was read from the text");
        assert_eq!(window.until_ms, recorded.as_millis() + 3_600_000);
    }

    /// Two sessions parked on one account's weekly window with no reset of their own. The older
    /// parked before the account's reported deadline ran out, the later after it, so the later
    /// one only records an estimate. Whichever the reaper handles first, the older one wakes at
    /// the reported deadline, not on its own timed schedule.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_later_parks_estimate_leaves_the_older_parks_baseline() {
        const MIN_MS: i64 = 60_000;
        let (older, later) = ("sess-baseline-older", "sess-baseline-later");
        for later_first in [true, false] {
            let (first, second) = if later_first {
                (later, older)
            } else {
                (older, later)
            };
            let (_home, state, project, _registry) = calendar_session(first).await;
            {
                let mut second_row = structured_instance(second, &project.path().to_string_lossy());
                second_row.tool = "claude-a".into();
                second_row.detect_as = "claude".into();
                state.instances.write().await.push(second_row);
            }
            let now_ms = Timestamp::now().as_millis();
            for (id, parked_at) in [(older, now_ms - 40 * MIN_MS), (later, now_ms - 5 * MIN_MS)] {
                record_calendar(
                    &state,
                    id,
                    [(
                        RateLimitInfo {
                            status: "You've hit your weekly limit".into(),
                            resets_at: None,
                            kind: "seven_day".into(),
                        },
                        parked_at,
                        None,
                    )],
                );
            }
            // The account's reported weekly deadline ran out ten minutes ago; the pool has no room.
            state.acp_event_store.record_account_limit(
                &account_key_of("claude-a"),
                now_ms - 10 * MIN_MS,
                "seven_day",
                now_ms - 300 * MIN_MS,
                false,
            );
            state.acp_event_store.record_account_limit(
                &account_key_of("claude-b"),
                now_ms + 24 * 60 * MIN_MS,
                "other_window",
                now_ms,
                false,
            );
            let mut attempted: HashSet<String> = [older.to_string(), later.to_string()].into();

            let released = reap_calendar(&state, &mut attempted).await;

            assert!(
                released.contains(older),
                "later handled first: {later_first}; the older park wakes at the reported deadline"
            );
            assert!(
                !released.contains(later),
                "later handled first: {later_first}; the later park keeps its own schedule"
            );
        }
    }

    /// A weekly park's own reset is evidence for the weekly window even when a newer window-less
    /// report extends the wake past it. If the session then moves away and that report is
    /// corrected to an earlier deadline, the account is still held until the weekly reset, not
    /// the corrected deadline.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_weekly_parks_reset_outlives_a_corrected_window_less_report() {
        const HOUR_MS: i64 = 3_600_000;
        let id = "sess-own-evidence";
        let (_home, state, _project, _registry) = calendar_session(id).await;
        let now_ms = Timestamp::now().as_millis();
        let weekly_reset = Timestamp::from_millis(now_ms + 5 * HOUR_MS).unwrap();
        let weekly_wake = rate_limit_resume_at(
            weekly_reset,
            now_ms - 5 * HOUR_MS,
            RATE_LIMIT_AUTO_RESUME_GRACE_SECS,
        );
        park_at(
            &state,
            id,
            now_ms - 5 * HOUR_MS,
            RateLimitInfo {
                status: "You've hit your weekly limit".into(),
                resets_at: Some(weekly_reset),
                kind: "seven_day".into(),
            },
            None,
        );
        let key = account_key_of("claude-a");
        // A window-less report, observed after the park, reaches an hour past the weekly reset.
        state.acp_event_store.record_account_limit(
            &key,
            now_ms + 6 * HOUR_MS,
            "rate_limit",
            now_ms - 4 * HOUR_MS,
            false,
        );
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-b"),
            now_ms + 24 * HOUR_MS,
            "other_window",
            now_ms,
            false,
        );
        let mut attempted: HashSet<String> = [id.to_string()].into();

        reap_calendar(&state, &mut attempted).await;

        // The session moves away; its window-less report is corrected to end sooner.
        state.acp_event_store.record_account_limit(
            &key,
            now_ms + 2 * HOUR_MS,
            "rate_limit",
            now_ms - HOUR_MS,
            false,
        );
        let account = state.acp_event_store.account_state(&key).unwrap();
        let weekly = account
            .window("seven_day")
            .expect("the weekly window is held");
        assert_eq!(weekly.until_ms, weekly_wake.as_millis());
        assert_eq!(weekly.observed_at_ms, now_ms - 5 * HOUR_MS);
        assert!(
            !account.has_headroom(now_ms + 3 * HOUR_MS),
            "the weekly window still holds the account"
        );
    }

    /// A reset an undated weekly rejection copied from a window-less report is that report's, not
    /// the weekly window's own: it may not replace a genuine weekly report that reaches further.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_inherited_window_less_reset_never_replaces_a_weekly_report() {
        const HOUR_MS: i64 = 3_600_000;
        let id = "sess-inherited-generic";
        let (_home, state, _project, _registry) = calendar_session(id).await;
        let now_ms = Timestamp::now().as_millis();
        // The rejection was recorded three hours ago and copied a reset that ended ten minutes
        // ago, from a window-less report observed then.
        park_at(
            &state,
            id,
            now_ms - 3 * HOUR_MS,
            RateLimitInfo {
                status: "You've hit your weekly limit".into(),
                resets_at: Timestamp::from_millis(now_ms - 10 * 60_000),
                kind: "seven_day".into(),
            },
            Some((now_ms - 3 * HOUR_MS, "rate_limit")),
        );
        // The account's genuine weekly report, observed before it, reaches four hours ahead.
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-a"),
            now_ms + 4 * HOUR_MS,
            "seven_day",
            now_ms - 5 * HOUR_MS,
            false,
        );
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-b"),
            now_ms + 24 * HOUR_MS,
            "other_window",
            now_ms,
            false,
        );
        let mut attempted: HashSet<String> = [id.to_string()].into();

        let released = reap_calendar(&state, &mut attempted).await;

        assert!(
            !released.contains(id),
            "the weekly report outlasts the window-less reset the park copied"
        );
        let weekly = state
            .acp_event_store
            .account_state(&account_key_of("claude-a"))
            .unwrap();
        assert_eq!(
            weekly.window("seven_day").map(|w| w.until_ms),
            Some(now_ms + 4 * HOUR_MS)
        );
    }

    /// Two sessions on one account. This one's undated weekly rejection copied a window-less reset
    /// to eight hours ahead, observed five hours ago. The other session's later window-less
    /// report corrected the account's deadline to half an hour after it was observed, so it is
    /// over, and it predates this park. No other account is free. The corrected report still
    /// invalidates the older reset: the park falls back to its timed schedule, not to 8 hours.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_elapsed_correction_invalidates_the_reset_a_park_carries() {
        const HOUR_MS: i64 = 3_600_000;
        let id = "sess-elapsed-correction";
        let (_home, state, _project, _registry) = calendar_session(id).await;
        let now_ms = Timestamp::now().as_millis();
        park_at(
            &state,
            id,
            now_ms - 3 * HOUR_MS,
            RateLimitInfo {
                status: "You've hit your weekly limit".into(),
                resets_at: Timestamp::from_millis(now_ms + 8 * HOUR_MS),
                kind: "seven_day".into(),
            },
            Some((now_ms - 5 * HOUR_MS, "rate_limit")),
        );
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-a"),
            now_ms - 7 * HOUR_MS / 2,
            "rate_limit",
            now_ms - 4 * HOUR_MS,
            false,
        );
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-b"),
            now_ms + 24 * HOUR_MS,
            "other_window",
            now_ms,
            false,
        );
        let mut attempted: HashSet<String> = [id.to_string()].into();

        let released = reap_calendar(&state, &mut attempted).await;

        assert!(
            released.contains(id),
            "the timed schedule applies once the carried reset is invalidated"
        );
    }

    /// A named park that inherited a window-less reset, written while the failover was off, leaves
    /// the account table empty. The first pass with it on records that evidence under the
    /// window-less window with the source's observation, not as the park's own window, so the
    /// account stays held for the other sessions that would fail over into it.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_inherited_window_less_reset_is_recorded_as_window_less_evidence() {
        const HOUR_MS: i64 = 3_600_000;
        let id = "sess-inherited-backfill";
        let (_home, state, _project, _registry) = calendar_session(id).await;
        let now_ms = Timestamp::now().as_millis();
        let reset = Timestamp::from_millis(now_ms + HOUR_MS).unwrap();
        let wake = rate_limit_resume_at(
            reset,
            now_ms - 3 * HOUR_MS,
            RATE_LIMIT_AUTO_RESUME_GRACE_SECS,
        );
        park_at(
            &state,
            id,
            now_ms - 3 * HOUR_MS,
            RateLimitInfo {
                status: "You've hit your weekly limit".into(),
                resets_at: Some(reset),
                kind: "seven_day".into(),
            },
            Some((now_ms - 4 * HOUR_MS, "rate_limit")),
        );
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-b"),
            now_ms + 24 * HOUR_MS,
            "other_window",
            now_ms,
            false,
        );
        let key = account_key_of("claude-a");
        assert!(state.acp_event_store.account_state(&key).is_none());
        let mut attempted: HashSet<String> = [id.to_string()].into();

        let released = reap_calendar(&state, &mut attempted).await;
        assert!(
            !released.contains(id),
            "with no account row yet, the inherited reset still holds the session parked"
        );

        let account = state.acp_event_store.account_state(&key).unwrap();
        let generic = account.window("rate_limit").expect("the generic evidence");
        assert_eq!(generic.until_ms, wake.as_millis());
        assert_eq!(generic.observed_at_ms, now_ms - 4 * HOUR_MS);
        assert!(!generic.estimated);
        assert!(
            account.window("seven_day").is_none(),
            "never promoted to the park's own window"
        );
        assert!(!account.has_headroom(now_ms));
    }

    /// A newer window-less report may only lengthen a named park's explicit reset: it can
    /// describe another window of the account, so it never ends the park sooner.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_unnamed_report_never_shortens_a_named_explicit_reset() {
        const HOUR_MS: i64 = 3_600_000;
        // (id, report deadline in hours from now, session is released)
        for (id, report_until, released) in [
            ("sess-unnamed-shorter", -1, false),
            ("sess-unnamed-longer", 24, false),
        ] {
            let (_home, state, _project, _registry) = calendar_session(id).await;
            let recorded = Timestamp::now() - crate::time::Duration::hours(2);
            record_calendar(
                &state,
                id,
                [(
                    RateLimitInfo {
                        status: "You've hit your weekly limit".into(),
                        resets_at: Some(Timestamp::now() + crate::time::Duration::hours(10)),
                        kind: "seven_day".into(),
                    },
                    recorded.as_millis(),
                    None,
                )],
            );
            let now_ms = Timestamp::now().as_millis();
            state.acp_event_store.record_account_limit(
                &account_key_of("claude-a"),
                now_ms + report_until * HOUR_MS,
                "rate_limit",
                now_ms - 1_000,
                false,
            );
            state.acp_event_store.record_account_limit(
                &account_key_of("claude-b"),
                now_ms + 24 * HOUR_MS,
                "other_window",
                now_ms,
                false,
            );
            let mut attempted: HashSet<String> = [id.to_string()].into();

            let got = reap_calendar(&state, &mut attempted).await;

            assert_eq!(got.contains(id), released, "{id}");
        }
    }

    /// A rejection that inherited its reset repeats it in a newer event; the reset was still
    /// observed when it first appeared, so a correction another session recorded in between
    /// stands, and the later event never overwrites it.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_inherited_reset_keeps_its_original_observation_time() {
        const HOUR_MS: i64 = 3_600_000;
        let id = "sess-obs-inherited";
        let (_home, state, _project, _registry) = calendar_session(id).await;
        let now_ms = Timestamp::now().as_millis();
        let reset = Timestamp::now() + crate::time::Duration::hours(10);
        let limit = RateLimitInfo {
            status: "You've hit your weekly limit".into(),
            resets_at: Some(reset),
            kind: "seven_day".into(),
        };
        // Report four hours ago; a later rejection inherited it, carrying that time.
        record_calendar(
            &state,
            id,
            [
                (limit.clone(), now_ms - 4 * HOUR_MS, None),
                (
                    limit,
                    now_ms - HOUR_MS,
                    Some((now_ms - 4 * HOUR_MS, "seven_day")),
                ),
            ],
        );
        // Another session corrected the window two hours ago: it has already cleared.
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-a"),
            now_ms - HOUR_MS,
            "seven_day",
            now_ms - 2 * HOUR_MS,
            false,
        );
        state.acp_event_store.record_account_limit(
            &account_key_of("claude-b"),
            now_ms + 24 * HOUR_MS,
            "other_window",
            now_ms,
            false,
        );
        let mut attempted: HashSet<String> = [id.to_string()].into();

        let released = reap_calendar(&state, &mut attempted).await;

        assert!(
            released.contains(id),
            "the correction is newer than the reset's first observation"
        );
        let stored = state
            .acp_event_store
            .account_state(&account_key_of("claude-a"))
            .unwrap();
        assert_eq!(
            stored.window("seven_day").unwrap().until_ms,
            now_ms - HOUR_MS,
            "the inherited copy must not overwrite the correction"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_direct_report_repeating_a_reset_is_observed_when_it_is_repeated() {
        const HOUR_MS: i64 = 3_600_000;
        let id = "sess-obs-reported-again";
        let (_home, state, _project, _registry) = calendar_session(id).await;
        let now_ms = Timestamp::now().as_millis();
        let reset = Timestamp::now() + crate::time::Duration::hours(10);
        let limit = RateLimitInfo {
            status: "You've hit your weekly limit".into(),
            resets_at: Some(reset),
            kind: "seven_day".into(),
        };
        // This session reports the reset, a sibling on the account corrects it, then this
        // session reports the same reset again: its history reads A, A, but the second is a
        // fresh observation, newer than the correction.
        let store = &state.acp_event_store;
        record_calendar(
            &state,
            id,
            [
                (limit.clone(), now_ms - 4 * HOUR_MS, None),
                (limit, now_ms - HOUR_MS, None),
            ],
        );
        store.record_account_limit(
            &account_key_of("claude-a"),
            now_ms - HOUR_MS,
            "seven_day",
            now_ms - 2 * HOUR_MS,
            false,
        );
        store.record_account_limit(
            &account_key_of("claude-b"),
            now_ms + 24 * HOUR_MS,
            "other_window",
            now_ms,
            false,
        );
        let mut attempted: HashSet<String> = [id.to_string()].into();

        let released = reap_calendar(&state, &mut attempted).await;

        assert!(
            !released.contains(id),
            "the fresh report outranks the older correction"
        );
        let stored = store.account_state(&account_key_of("claude-a")).unwrap();
        assert!(
            stored.window("seven_day").unwrap().until_ms > now_ms + 9 * HOUR_MS,
            "the correction must not wake the session early"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_newer_confirmation_of_the_same_deadline_is_recorded() {
        const HOUR_MS: i64 = 3_600_000;
        let id = "sess-obs-confirmed";
        let (_home, state, _project, _registry) = calendar_session(id).await;
        let now_ms = Timestamp::now().as_millis();
        let reset = Timestamp::from_millis(now_ms + 10 * HOUR_MS).unwrap();
        let deadline_ms =
            now_ms + 10 * HOUR_MS + i64::from(RATE_LIMIT_AUTO_RESUME_GRACE_SECS) * 1000;
        let store = &state.acp_event_store;
        let limit = RateLimitInfo {
            status: "You've hit your weekly limit".into(),
            resets_at: Some(reset),
            kind: "seven_day".into(),
        };
        // The session confirms, an hour ago, the deadline the table already holds from a
        // report four hours old.
        record_calendar(&state, id, [(limit, now_ms - HOUR_MS, None)]);
        let key = account_key_of("claude-a");
        store.record_account_limit(&key, deadline_ms, "seven_day", now_ms - 4 * HOUR_MS, false);
        store.record_account_limit(
            &account_key_of("claude-b"),
            now_ms + 24 * HOUR_MS,
            "other_window",
            now_ms,
            false,
        );
        let mut attempted: HashSet<String> = [id.to_string()].into();

        reap_calendar(&state, &mut attempted).await;

        // A sibling's delayed report, observed between the two, must not override it.
        store.record_account_limit(
            &key,
            now_ms - HOUR_MS,
            "seven_day",
            now_ms - 2 * HOUR_MS,
            false,
        );
        let stored = store.account_state(&key).unwrap();
        let window = stored.window("seven_day").unwrap();
        assert_eq!(window.until_ms, deadline_ms);
        assert_eq!(window.observed_at_ms, now_ms - HOUR_MS);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn an_undated_park_uses_the_accounts_reported_deadline() {
        let id = "sess-failover-undated";
        let (_home, state, _project, _registry) = calendar_session(id).await;
        // The park carries no reset time; it was recorded two hours ago.
        let two_hours_ago = (Timestamp::now() - crate::time::Duration::hours(2)).as_millis();
        record_calendar(
            &state,
            id,
            [(RateLimitInfo::undated(), two_hours_ago, None)],
        );
        let kind = known_park(&state, id).info.unwrap().kind;
        // The weekly reset was reported on another session two days out; the pool has no room.
        let now_ms = Timestamp::now().as_millis();
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

        let released = reap_calendar(&state, &mut attempted).await;

        assert!(
            released.is_empty() && attempted.contains(id),
            "the 1h estimate elapsed long ago; the reported deadline is two days out"
        );
        let stored = state
            .acp_event_store
            .account_state(&account_key_of("claude-a"))
            .unwrap();
        let window = stored.window(&kind).unwrap();
        assert_eq!((window.until_ms, window.estimated), (two_days, false));
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
                        "claude",
                        None,
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
                        started_at: crate::time::Timestamp::now(),
                    };
                    live.state
                        .acp_event_store
                        .record(
                            &id,
                            live.state.acp_event_store.highest_seq(&id).unwrap() + 1,
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
                        queued.created_at = crate::time::Timestamp::from_millis(
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
                    let frontier = live.state.acp_event_store.highest_seq(&id).unwrap();
                    live.state
                        .session_service
                        .drain_queued_prompts_once(&id)
                        .await;
                    assert_eq!(
                        live.state.acp_event_store.highest_seq(&id).unwrap(),
                        frontier
                    );
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
