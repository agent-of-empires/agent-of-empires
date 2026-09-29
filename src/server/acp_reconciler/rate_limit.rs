//! Opt-in rate-limit auto-resume (#1722) with a bounded redelivery budget (#3688).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::{is_resumable, query_store, resolve_per_profile, AppState};
use crate::acp::state::RATE_LIMIT_EXHAUSTED_RETRIES_REASON;

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

/// A queue row's mint time in epoch ms, or `None` when it cannot be read.
fn queued_at_ms(entry: &crate::daemon::QueuedPromptEntry) -> Option<i64> {
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
/// An unreadable mint time or an unknown limit never supersedes: the
/// interrupted request is the older one and losing it is final, since the
/// first prompt the drain delivers retires the park.
fn queue_supersedes(queued_at_ms: &[i64], limit_at_ms: Option<i64>) -> bool {
    let Some(limit_at_ms) = limit_at_ms else {
        return false;
    };
    queued_at_ms
        .iter()
        .any(|queued_at| *queued_at > limit_at_ms)
}

/// What the continuation producer decided for one session (#4092).
pub(crate) enum ContinuationOutcome {
    /// The interrupted prompt is the next turn, is already installed, or there
    /// was nothing to replay. Only this outcome licenses the automatic
    /// `RateLimitAutoResumed`, which is the budget's arming step.
    Stands,
    /// A queued prompt owns the next turn. It has no other route to a worker,
    /// so the caller still frees the respawn, but no automatic breadcrumb.
    SupersededByQueue,
}

/// Installs the rate-limit-interrupted prompt as the next turn so a resume
/// continues the work (#3028).
///
/// Takes the session's submission authority by value: it cannot be handed
/// over, because `prompt_submission` is a plain mutex and a second claim by
/// the same task would deadlock. Holding it is the price of the call, and it
/// is what stops a turn-accepting surface from slipping between the
/// supersession checks and the install (#4092).
///
/// Park liveness is not a supersession oracle here
/// ([`crate::acp::event_store::EventStore::rate_limit_park`]): a fresh worker
/// publishes `AcpSessionAssigned`, which retires its own park.
pub(crate) async fn install_rate_limit_continuation(
    state: &Arc<AppState>,
    id: &str,
    _submission: tokio::sync::OwnedMutexGuard<()>,
) -> ContinuationOutcome {
    let Some(Some((text, attachments))) = query_store(
        &state.acp_event_store,
        id,
        "rate-limit continuation",
        |s, id| s.rate_limited_turn_prompt(id),
    )
    .await
    else {
        return ContinuationOutcome::Stands;
    };
    #[cfg(test)]
    state.session_service.await_install_barrier(id).await;
    // A queued prompt publishes no event, so its row is the only record of a
    // supersession. Only the mint times leave the read guard: a row carries its
    // whole text, and copying that under the shared lock is not worth one
    // timestamp. A continuation an earlier cadence installed is still next.
    let queued_at_ms: Vec<i64> = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return ContinuationOutcome::Stands;
        };
        if inst.pending_initial_turn.is_some() {
            return ContinuationOutcome::Stands;
        }
        inst.queued_prompts
            .iter()
            .filter_map(queued_at_ms)
            .collect()
    };
    if !queued_at_ms.is_empty() {
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
        if queue_supersedes(&queued_at_ms, limit_at_ms) {
            return ContinuationOutcome::SupersededByQueue;
        }
    }
    state
        .session_service
        .set_pending_initial_turn(id, text, attachments)
        .await;
    ContinuationOutcome::Stands
}

/// Releases rate-limit parks whose window elapsed: install the interrupted
/// prompt unless a queued one took the session, publish the breadcrumb for a
/// delivered continuation, and free the `attempted` slot so this tick
/// respawns. The park and its times come from the durable event store (#3514).
/// Returns the released ids so the resume loop does not re-hold them.
pub(super) async fn reap_rate_limit_resumes(
    state: &Arc<AppState>,
    attempted: &mut HashSet<String>,
    parked: &HashSet<String>,
) -> HashSet<String> {
    let mut released = HashSet::new();
    let candidates: Vec<(String, String, bool)> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| is_resumable(i) && attempted.contains(&i.id))
            .map(|i| {
                (
                    i.id.clone(),
                    i.source_profile.clone(),
                    !i.queued_prompts.is_empty(),
                )
            })
            .collect()
    };
    // Only a workerless session is parked; a crash-loop park waits for a manual retry.
    let mut workerless = Vec::new();
    for candidate in candidates {
        if parked.contains(&candidate.0) {
            tracing::debug!(target: "acp.supervisor", session = %candidate.0, "rate-limit auto-resume: skipped, session is crash-loop parked");
        } else if !state.acp_supervisor.is_running(&candidate.0).await {
            workerless.push(candidate);
        }
    }
    if workerless.is_empty() {
        return released;
    }
    let enabled_by_profile = resolve_per_profile(workerless.iter().map(|c| c.1.clone()), |c| {
        c.acp.rate_limit_auto_resume
    })
    .await;

    let now = Utc::now();
    for (id, profile, has_queued_prompts) in workerless {
        let skip = |reason: &str| {
            tracing::debug!(target: "acp.supervisor", session = %id, "rate-limit auto-resume: skipped, {reason}");
        };
        if !enabled_by_profile.get(&profile).copied().unwrap_or(false) {
            skip("not enabled for this profile");
            continue;
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
                tracing::info!(target: "acp.supervisor", session = %id, "rate-limit auto-resume: releasing the redelivery-cap park for a queued prompt");
                attempted.remove(&id);
                released.insert(id.clone());
            } else {
                skip("redelivery cap reached and no prompt queued");
            }
            continue;
        }
        // A pruned `RateLimit` row has no reset time and follows the unknown-reset schedule.
        let info = park
            .info
            .clone()
            .unwrap_or_else(crate::acp::state::RateLimitInfo::undated);
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
        let mut resume_at = match info.resets_at {
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
        if state.acp_supervisor.is_running(&id).await {
            skip("worker is already live");
            continue;
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
        if streak >= RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES {
            // `/acp/spawn` holds this lock through its continuation install, so
            // the CAS plus clear cannot interleave with it. `try_lock` because
            // blocking would stall the tick; contention is a refusal.
            let instance_lock = state.instance_lock(&id).await;
            let Ok(_guard) = instance_lock.try_lock() else {
                continue;
            };
            if state.acp_supervisor.publish_stopped_if_seq(
                &id,
                RATE_LIMIT_EXHAUSTED_RETRIES_REASON,
                latest_seq,
            ) {
                state.session_service.clear_pending_initial_turn(&id).await;
                tracing::warn!(
                    target: "acp.supervisor",
                    session = %id,
                    redeliveries = streak,
                    max = RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES,
                    "rate-limit auto-resume: redelivery cap reached; parking session with a terminal stop"
                );
            }
            continue;
        }
        // Contention is a refusal, not a queue: this is one sequential pass over
        // every session and must not stall behind a submission.
        let Some(submission) = state.session_service.try_prompt_submission(&id).await else {
            skip("a prompt submission owns the session");
            continue;
        };
        let outcome = install_rate_limit_continuation(state, &id, submission).await;
        match outcome {
            ContinuationOutcome::Stands => {
                state
                    .acp_supervisor
                    .publish_rate_limit_auto_resumed(&id, resume_at, false);
                tracing::info!(
                    target: "acp.supervisor",
                    session = %id,
                    resets_at = ?info.resets_at,
                    resume_at = %resume_at,
                    "rate-limit auto-resume: park window elapsed; respawning worker"
                );
            }
            ContinuationOutcome::SupersededByQueue => {
                tracing::info!(
                    target: "acp.supervisor",
                    session = %id,
                    "rate-limit auto-resume: a queued prompt superseded the interrupted prompt; respawning for the queue"
                );
            }
        }
        attempted.remove(&id);
        released.insert(id.clone());
    }
    released
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
            text: "run the nightly task".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: false,
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
            .publish_rate_limit_auto_resumed(id, Utc::now(), false);
        state
            .acp_supervisor
            .publish_startup_error(id, "spawn failed".into());
        let mut attempted: HashSet<String> = [id.to_string()].into();

        let released = reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;

        assert!(released.is_empty() && attempted.contains(id));
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

    /// #3688: a cap park must release its `attempted` slot for an already
    /// queued prompt (nothing else would deliver it) and hold it otherwise.
    #[tokio::test]
    #[serial_test::serial]
    async fn cap_park_releases_attempted_for_a_prompt_already_on_the_queue() {
        for has_queue in [true, false] {
            let id = "sess-3688-cap-queue";
            let (_home, state, _project) = test_state(id);
            enable_auto_resume(true);
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

            let mut tick = Tick::default();
            tick.attempted.insert(id.to_string());
            reap_rate_limit_resumes(&state, &mut tick.attempted, &HashSet::new()).await;
            assert_eq!(
                !tick.attempted.contains(id),
                has_queue,
                "has_queue={has_queue}"
            );

            tick.run(&state).await;
            assert_eq!(
                startup_errors(&state, id) > 0,
                has_queue,
                "has_queue={has_queue}"
            );
        }
    }

    enum Setup {
        None,
        /// A publish landed between the probe and the CAS.
        CasAhead,
        /// A manual `/acp/spawn` holds the instance lock.
        LockHeld,
    }

    /// Below the cap the pass resumes; at the cap it publishes the terminal park
    /// and drops the continuation, unless the CAS refuses or the lock is contended.
    #[tokio::test]
    #[serial_test::serial]
    async fn rate_limit_reap_resumes_below_the_cap_and_parks_at_it() {
        let max = RATE_LIMIT_AUTO_RESUME_MAX_REDELIVERIES as usize;
        // (streak, setup, released, latest stop reason, queued continuation).
        // A kept continuation is daemon-queued, not user-typed, so it carries
        // `synthesized` and the transcript model skips a duplicate row (#4041).
        let cases = [
            (max - 1, Setup::None, true, "rate_limited", Some(true)),
            (
                max,
                Setup::None,
                false,
                RATE_LIMIT_EXHAUSTED_RETRIES_REASON,
                None,
            ),
            (max, Setup::CasAhead, false, "rate_limited", Some(true)),
            (max, Setup::LockHeld, false, "rate_limited", Some(true)),
        ];
        for (streak, setup, released, reason, kept) in cases {
            let id = "sess-3688";
            let (_home, state, _project) = parked_with_continuation(id, streak).await;
            let lock = state.instance_lock(id).await;
            let _guard = match setup {
                Setup::None => None,
                Setup::CasAhead => {
                    let ahead = state.acp_event_store.highest_seq(id) + 1;
                    state.acp_supervisor.hydrate_seqs([(id.to_string(), ahead)]);
                    None
                }
                Setup::LockHeld => Some(lock.lock().await),
            };
            let mut attempted: HashSet<String> = [id.to_string()].into();

            tokio::time::timeout(
                Duration::from_secs(5),
                reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()),
            )
            .await
            .expect("a contended instance_lock must not block the tick");

            let case = format!("streak={streak}");
            assert_eq!(!attempted.contains(id), released, "{case}");
            assert_eq!(
                latest_stop_reason(&state, id).as_deref(),
                Some(reason),
                "{case}"
            );
            assert_eq!(pending_turn(&state, id).await, kept, "{case}");
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

    /// Only a row minted after the limit replaced the turn. A row before it, a
    /// limit that was never recorded, and a row whose mint time cannot be read
    /// all leave the continuation standing: the interrupted request is the older
    /// one and losing it is final.
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
        };
        let after = Utc.timestamp_opt(1_700_000_001, 0).unwrap().to_rfc3339();
        let before = Utc.timestamp_opt(1_699_999_999, 0).unwrap().to_rfc3339();
        let times = |rows: &[crate::daemon::QueuedPromptEntry]| {
            rows.iter().filter_map(queued_at_ms).collect::<Vec<_>>()
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
                "mint time unreadable",
                vec![row(1, "t0")],
                Some(limit_ms),
                false,
            ),
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

    /// #4092: a queued prompt owns the next turn only while the pending-turn
    /// slot is free and at least one row was minted after the limit that
    /// interrupted it. A row queued while the interrupted turn was still running
    /// is a follow-up the user wants behind it, so the continuation is installed
    /// and its redelivery charged. The rows are seeded on the instance because
    /// the endpoint stamps the server's clock, which no test can place in the
    /// past.
    #[tokio::test]
    #[serial_test::serial]
    async fn only_a_prompt_queued_after_the_park_supersedes_the_continuation() {
        // The `parked` fixture backdates the limit an hour, so these straddle it.
        let after = Utc::now();
        let before = Utc::now() - chrono::Duration::hours(2);
        // (label, continuation installed, queued at, turn kept, breadcrumbs)
        let cases = [
            ("queued-after", false, after, None, 0),
            ("installed-and-queued-after", true, after, Some(true), 1),
            ("queued-before", false, before, Some(true), 1),
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
                    created_at: queued_at.to_rfc3339(),
                    origin_device: None,
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
            let mut attempted: HashSet<String> = [id.clone()].into();

            let released = reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()).await;

            assert_eq!(pending_turn(&state, &id).await, kept, "{label}");
            assert_eq!(
                auto_resumed_breadcrumbs(&state, &id),
                breadcrumbs,
                "{label}: a refused continuation must not spend a redelivery"
            );
            assert!(
                !attempted.contains(&id) && released.contains(&id),
                "{label}: the queued prompt has no other route to a worker"
            );
        }
    }

    /// #4092: a spawn's own worker publishes `AcpSessionAssigned`, which
    /// retires the park without any prompt having superseded anything, so the
    /// install must key on the interrupted prompt rather than on park liveness.
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
                },
                Utc::now().timestamp_millis(),
            )
            .unwrap();
        assert!(
            state.acp_event_store.rate_limit_park(id).is_none(),
            "the assignment must retire the park, or the case asserts nothing"
        );

        install_rate_limit_continuation(
            &state,
            id,
            state.session_service.prompt_submission(id).await,
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

        let released = tokio::time::timeout(
            Duration::from_secs(5),
            reap_rate_limit_resumes(&state, &mut attempted, &HashSet::new()),
        )
        .await
        .expect("a contended submission guard must not block the tick");

        assert!(
            attempted.contains(id),
            "the session stays held until the next cadence"
        );
        assert!(
            !released.contains(id),
            "nothing was resumed, so nothing is released"
        );
        assert_eq!(pending_turn(&state, id).await, None);
        assert_eq!(auto_resumed_breadcrumbs(&state, id), 0);
    }
}
