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

/// Queues the rate-limit-interrupted prompt as the next turn so a resume continues the work (#3028).
pub(crate) async fn enqueue_rate_limit_continuation(state: &Arc<AppState>, id: &str) {
    let Some(Some((text, attachments))) = query_store(
        &state.acp_event_store,
        id,
        "rate-limit continuation",
        |s, id| s.rate_limited_turn_prompt(id),
    )
    .await
    else {
        return;
    };
    state
        .session_service
        .set_pending_initial_turn(id, text, attachments)
        .await;
}

/// Releases rate-limit parks whose window elapsed: queue the interrupted prompt,
/// publish the breadcrumb and free the `attempted` slot so this tick respawns.
/// The park and its times come from the durable event store (#3514).
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
            // `/acp/spawn` holds this lock through its continuation enqueue, so
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
        enqueue_rate_limit_continuation(state, &id).await;
        state
            .acp_supervisor
            .publish_rate_limit_auto_resumed(&id, resume_at, false);
        attempted.remove(&id);
        released.insert(id.clone());
        tracing::info!(
            target: "acp.supervisor",
            session = %id,
            resets_at = ?info.resets_at,
            resume_at = %resume_at,
            "rate-limit auto-resume: park window elapsed; respawning worker"
        );
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

    #[test]
    fn resume_at_is_the_later_of_reset_plus_grace_and_the_floor() {
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
    }

    /// A session parked on an elapsed limit with `redeliveries` resume cycles
    /// behind it and a pending continuation; events are backdated an hour.
    async fn parked_at_streak(
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
        let (_home, state, _project) = parked_at_streak(id, 0).await;
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
        let (_home, state, _project) = parked_at_streak(id, 0).await;
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
            let (_home, state, _project) = parked_at_streak(id, streak).await;
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
}
