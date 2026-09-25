use std::time::Duration;

use super::*;
use crate::acp::state::Event;

fn structured_instance(id: &str, idle: bool) -> crate::session::Instance {
    let mut inst = crate::session::Instance::new(id, &format!("/tmp/aoe-{id}"));
    inst.id = id.to_string();
    inst.view = crate::session::View::Structured;
    if idle {
        inst.status = crate::session::Status::Idle;
    }
    inst
}

fn structured_state(id: &str, idle: bool) -> Arc<AppState> {
    crate::server::test_support::build_test_app_state(vec![structured_instance(id, idle)])
}

fn prompt_req(text: &str) -> Result<Json<PromptRequest>, axum::extract::rejection::JsonRejection> {
    Ok(Json(PromptRequest {
        text: text.to_string(),
        attachments: Vec::new(),
        prompt_id: None,
        no_revive: false,
    }))
}

fn no_revive_prompt_req(
    text: &str,
) -> Result<Json<PromptRequest>, axum::extract::rejection::JsonRejection> {
    Ok(Json(PromptRequest {
        text: text.to_string(),
        attachments: Vec::new(),
        prompt_id: None,
        no_revive: true,
    }))
}

fn diff_req(
    markdown: &str,
) -> Result<Json<DiffCommentsPromptRequest>, axum::extract::rejection::JsonRejection> {
    Ok(Json(DiffCommentsPromptRequest {
        intro: String::new(),
        outro: String::new(),
        is_multi_repo: false,
        comments: Vec::new(),
        assembled_markdown: markdown.to_string(),
    }))
}

fn published(state: &AppState, id: &str, pred: impl Fn(&Event) -> bool) -> bool {
    state
        .acp_event_store
        .replay_from(id, 0)
        .iter()
        .any(|(_, e)| pred(e))
}

fn park_on_exhausted_rate_limit(state: &AppState, id: &str) {
    assert!(state.acp_supervisor.publish_stopped_if_seq(
        id,
        crate::acp::state::RATE_LIMIT_EXHAUSTED_RETRIES_REASON,
        0,
    ));
}

/// An ARMED park: the limit reported a reset time and the cap is nowhere near,
/// so auto-resume (when enabled at all) still owns the recovery. This is the
/// state a user hits the moment a weekly limit lands.
fn park_on_armed_rate_limit(state: &AppState, id: &str) {
    let store = &state.acp_event_store;
    store
        .record(
            id,
            1,
            &Event::RateLimit {
                info: crate::acp::state::RateLimitInfo {
                    status: "limited".into(),
                    resets_at: Some(chrono::Utc::now() + chrono::Duration::days(3)),
                    kind: "usage".into(),
                },
            },
        )
        .expect("seed the limit");
    store
        .record(
            id,
            2,
            &Event::Stopped {
                reason: "rate_limited".into(),
            },
        )
        .expect("seed the park");
    state.acp_supervisor.hydrate_seqs([(id.to_string(), 2)]);
    assert!(
        store.rate_limit_park(id).is_some_and(|p| !p.cap_reached),
        "the fixture must be an armed park, or these tests assert nothing"
    );
}

/// Both park shapes, for tests that must hold on either.
const PARKS: [(&str, fn(&AppState, &str)); 2] = [
    ("exhausted", park_on_exhausted_rate_limit),
    ("armed", park_on_armed_rate_limit),
];

/// A structured session whose worker starts fail: on the provider limit when
/// `rate_limited`, otherwise as a generic launch failure. The count proves a
/// case reached the start it forced.
fn failing_start_state(
    id: &str,
    rate_limited: bool,
) -> (Arc<AppState>, Arc<std::sync::atomic::AtomicUsize>) {
    let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&launches);
    let launcher: crate::acp::supervisor::Launcher = Arc::new(move |_config, _session_id| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            Err::<crate::acp::acp_client::AcpClient, _>(if rate_limited {
                AcpError::RateLimited(Box::new(crate::acp::state::RateLimitInfo {
                    status: "limited".into(),
                    resets_at: Some(chrono::Utc::now() + chrono::Duration::days(3)),
                    kind: "usage".into(),
                }))
            } else {
                AcpError::Spawn("forced launch failure".into())
            })
        })
    });
    let state = crate::server::test_support::build_test_app_state_with_launcher(
        vec![structured_instance(id, false)],
        launcher,
    );
    (state, launches)
}

/// #3172: an idle-dormant wake must release `instance_lock` before awaiting
/// the worker (the spawn needs it), and must not publish a prompt no worker
/// received. A held reservation stands in for a spawn in flight.
#[tokio::test]
async fn wake_prompt_frees_instance_lock_and_publishes_nothing_without_a_worker() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    use crate::acp::supervisor::{ResumeKind, ResumeReservationOutcome};

    let id = "sess-3172".to_string();
    let state = structured_state(&id, false);
    let reservation = match state
        .acp_supervisor
        .begin_resume(&id, ResumeKind::Spawn)
        .await
        .expect("begin_resume must not error under capacity")
    {
        ResumeReservationOutcome::Reserved(r) => r,
        ResumeReservationOutcome::AlreadyPresent => panic!("expected a fresh reservation"),
    };

    let mut waits = state.acp_supervisor.watch_worker_waits();
    let handler = tokio::spawn({
        let state = Arc::clone(&state);
        let id = id.clone();
        async move {
            acp_prompt(State(state), Path(id), prompt_req("lgtm"))
                .await
                .into_response()
        }
    });
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), waits.recv())
            .await
            .expect("worker readiness reached")
            .expect("worker wait observation"),
        id
    );

    let inst_lock = state.instance_lock(&id).await;
    let acquired = tokio::time::timeout(Duration::from_secs(2), inst_lock.lock()).await;
    assert!(
        acquired.is_ok(),
        "acp_prompt must not hold instance_lock while it waits for the worker"
    );
    drop(acquired);

    drop(reservation);
    let response = tokio::time::timeout(Duration::from_secs(30), handler)
        .await
        .expect("handler must finish once the reservation drops")
        .expect("handler task must not panic");
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!published(&state, &id, |e| matches!(
        e,
        Event::UserPromptSent { .. }
    )));
}

/// A Stop pressed right after Enter must wait out the prompt submission rather
/// than reach the agent before the prompt it names.
#[tokio::test]
async fn cancel_waits_for_an_in_flight_prompt_submission() {
    let id = "sess-stop-order".to_string();
    let state = structured_state(&id, false);
    let submission = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
        .expect("seeded session must admit a submission");
    let mut claims = state.session_service.watch_submission_claims();

    let cancel = {
        let state = Arc::clone(&state);
        let id = id.clone();
        async move { acp_cancel(State(state), Path(id)).await.into_response() }
    };
    tokio::pin!(cancel);
    assert!(futures_util::poll!(&mut cancel).is_pending());
    assert_eq!(claims.try_recv().expect("contender reached claim"), id);

    drop(submission);
    tokio::time::timeout(Duration::from_secs(10), cancel)
        .await
        .expect("cancel must finish once the guard drops");
}

/// #3859: both turn-starting handlers claim the submission guard before they
/// wake the session (`last_accessed_at` is what the wake stamps).
#[tokio::test]
#[serial_test::serial]
async fn prompt_handlers_claim_the_submission_guard_before_they_wake() {
    for diff_comments in [false, true] {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let id = "sess-3859".to_string();
        let state = structured_state(&id, false);
        assert!(state.instances.read().await[0].last_accessed_at.is_none());

        let held = state
            .session_service
            .prompt_submission_for_session(&id)
            .await
            .expect("seeded session must admit a submission");
        let mut claims = state.session_service.watch_submission_claims();

        let handler = tokio::spawn({
            let state = Arc::clone(&state);
            let id = id.clone();
            async move {
                if diff_comments {
                    acp_prompt_diff_comments(State(state), Path(id), diff_req("review this"))
                        .await
                        .into_response()
                } else {
                    acp_prompt(State(state), Path(id), prompt_req("think about this"))
                        .await
                        .into_response()
                }
            }
        });

        let claimed = tokio::time::timeout(Duration::from_secs(10), claims.recv())
            .await
            .expect("handler must reach its submission claim")
            .expect("the tap outlives the handler");
        assert_eq!(claimed, id);
        assert!(
            state.instances.read().await[0].last_accessed_at.is_none(),
            "diff_comments={diff_comments}: claim must precede the wake"
        );

        drop(held);
        tokio::time::timeout(Duration::from_secs(30), handler)
            .await
            .expect("the handler must finish once the guard drops")
            .expect("handler task must not panic");
        assert!(
            state.instances.read().await[0].last_accessed_at.is_some(),
            "diff_comments={diff_comments}: the handler must wake under the guard"
        );
    }
}

/// #3688 and the armed park: either is sendable at the shared decision point.
#[tokio::test]
async fn rate_limit_park_is_sendable_at_the_shared_decision_point() {
    for (park, seed) in PARKS {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let id = format!("sess-{park}-park-shared");
        let state = structured_state(&id, false);
        let service = &state.session_service;
        let id_ref = id.as_str();
        let decide = move || async move {
            let _guard = service
                .admit_prompt_submission(&SessionCaller::User, id_ref)
                .await
                .expect("session exists");
            service
                .prompt_dispatch_under_submission(id_ref, false)
                .await
        };
        assert_eq!(
            decide().await,
            PromptDispatch::Queued {
                reason: QueueReason::WorkerDown,
            },
            "{park}"
        );
        seed(&state, &id);
        assert_eq!(decide().await, PromptDispatch::Sent, "{park}");
    }
}

/// #4081 review: `no_revive` must refuse a prompt to a fully stopped worker
/// (`WorkerDown`) rather than queue it, since queuing still hands the prompt
/// off once something else revives the worker. An idle-dormant session is
/// refused without waking it. Neither refusal may discard a pending initial
/// turn on the way.
#[tokio::test]
async fn no_revive_refuses_a_stopped_or_dormant_worker_without_side_effects() {
    for dormant in [false, true] {
        let id = format!("sess-no-revive-dormant-{dormant}");
        let state = structured_state(&id, false);
        {
            let mut instances = state.instances.write().await;
            if dormant {
                instances[0].mark_idle_dormant();
            }
            instances[0].pending_initial_turn = Some(crate::session::PendingInitialTurn {
                text: "queued before the park".to_string(),
                attachments: Vec::new(),
                synthesized: true,
            });
        }

        let response = acp_prompt(
            State(Arc::clone(&state)),
            Path(id.clone()),
            no_revive_prompt_req("hello"),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT, "dormant={dormant}");
        let inst = state.instances.read().await[0].clone();
        assert!(
            inst.pending_initial_turn.is_some(),
            "dormant={dormant}: no_revive refusal must not clear the pending initial turn"
        );
        if dormant {
            assert!(
                inst.last_accessed_at.is_none(),
                "no_revive must refuse before the wake touches the session"
            );
            assert!(inst.is_idle_dormant());
        }
        assert!(state
            .session_service
            .queued_prompts_snapshot(&id)
            .await
            .is_empty());
        assert!(!published(&state, &id, |e| matches!(
            e,
            Event::UserPromptSent { .. }
        )));
    }
}

/// #3688 and the armed park: a prompt or review on a rate-limit park drives a
/// resume instead of queueing or refusing, so the prompt is the recovery the
/// user reaches for rather than RESUME NOW, which re-sends the rate-limited
/// turn before theirs can go. Each start is forced: one refused on the
/// still-live limit re-parks without a startup error, which would bury the
/// park, and any other failure reports one. The park stands either way until
/// a turn lands. No reservation is held: one would make `is_running` true and
/// skip the park probe.
#[tokio::test]
async fn turn_on_a_park_resumes_instead_of_queueing() {
    for (park, seed) in PARKS {
        for rate_limited in [false, true] {
            for diff_comments in [false, true] {
                let case = format!(
                    "{park} park, rate_limited={rate_limited}, diff_comments={diff_comments}"
                );
                let _app_dir = crate::session::test_support::isolate_app_dir();
                let id = format!("sess-{park}-park-{rate_limited}-{diff_comments}");
                let (state, launches) = failing_start_state(&id, rate_limited);
                seed(&state, &id);
                // The detached start holds a `SessionService` clone until it ends.
                let refs_at_rest = Arc::strong_count(&state.session_service);

                let response = if diff_comments {
                    acp_prompt_diff_comments(
                        State(Arc::clone(&state)),
                        Path(id.clone()),
                        diff_req("please address these"),
                    )
                    .await
                    .into_response()
                } else {
                    acp_prompt(
                        State(Arc::clone(&state)),
                        Path(id.clone()),
                        prompt_req("switch me to a model that still has quota"),
                    )
                    .await
                    .into_response()
                };
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{case}");
                tokio::time::timeout(Duration::from_secs(10), async {
                    while Arc::strong_count(&state.session_service) > refs_at_rest {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap_or_else(|_| panic!("{case}: the detached start never finished"));

                assert_eq!(
                    launches.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "{case}: the forced start was never reached"
                );
                assert!(
                    state
                        .session_service
                        .queued_prompts_snapshot(&id)
                        .await
                        .is_empty(),
                    "{case}"
                );
                assert_eq!(
                    published(&state, &id, |e| matches!(
                        e,
                        Event::AgentStartupError { .. }
                    )),
                    !rate_limited,
                    "{case}"
                );
                assert!(
                    !published(&state, &id, |e| matches!(
                        e,
                        Event::UserPromptSent { .. } | Event::UserDiffCommentsPrompt { .. }
                    )),
                    "{case}"
                );
                assert!(
                    state.acp_event_store.rate_limit_park(&id).is_some(),
                    "{case}: the park must stand until a turn lands"
                );
            }
        }
    }
}

/// #4081 review: a prompt into either park dispatches `Sent` (see
/// `turn_on_a_park_resumes_instead_of_queueing`), so `no_revive` is enforced
/// by `send_turn`'s resume decision. The refusal must start no worker and
/// leave the park, the queue, and a pending initial turn as they were.
#[tokio::test]
async fn no_revive_refuses_a_prompt_on_a_rate_limit_park() {
    for (park, seed) in PARKS {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let id = format!("sess-no-revive-{park}-park");
        let (state, launches) = failing_start_state(&id, true);
        seed(&state, &id);
        state.instances.write().await[0].pending_initial_turn =
            Some(crate::session::PendingInitialTurn {
                text: "queued before the park".to_string(),
                attachments: Vec::new(),
                synthesized: true,
            });

        let response = acp_prompt(
            State(Arc::clone(&state)),
            Path(id.clone()),
            no_revive_prompt_req("hello"),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT, "{park}");
        assert_eq!(
            launches.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "{park}: no_revive must not start a worker"
        );
        assert!(
            state
                .session_service
                .queued_prompts_snapshot(&id)
                .await
                .is_empty(),
            "{park}"
        );
        assert!(
            !published(&state, &id, |e| matches!(
                e,
                Event::UserPromptSent { .. } | Event::AgentStartupError { .. }
            )),
            "{park}"
        );
        assert!(
            state.acp_event_store.rate_limit_park(&id).is_some(),
            "{park}: the park must stand"
        );
        assert!(
            state.instances.read().await[0]
                .pending_initial_turn
                .is_some(),
            "{park}: no_revive refusal must not clear the pending initial turn"
        );
    }
}

/// #3621: a direct prompt parks while a drain owns the session, and the drain
/// that follows leaves its row queued behind the turn the prompt started.
#[tokio::test]
async fn a_direct_prompt_and_the_queue_drain_cannot_both_own_the_same_turn() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-3621-race".to_string();
    let state = structured_state(&id, true);
    let cmds = state
        .acp_supervisor
        .test_insert_worker_cmd_recording(&id)
        .await;
    state
        .session_service
        .enqueue_prompt(
            &id,
            "q1".into(),
            "queued follow-up".into(),
            vec![],
            None,
            "t0".into(),
        )
        .await
        .expect("session exists");

    let drain_owns_it = state.session_service.prompt_submission(&id).await;
    let mut claims = state.session_service.watch_submission_claims();
    let handler = {
        let state = Arc::clone(&state);
        let id = id.clone();
        async move {
            acp_prompt(State(state), Path(id), prompt_req("typed mid-delivery"))
                .await
                .into_response()
        }
    };
    tokio::pin!(handler);
    assert!(futures_util::poll!(&mut handler).is_pending());
    assert_eq!(claims.try_recv().expect("contender reached claim"), id);

    drop(drain_owns_it);
    let response = tokio::time::timeout(Duration::from_secs(30), handler)
        .await
        .expect("the handler must finish once the drain releases the session");
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    state.session_service.drain_queued_prompts_once(&id).await;
    assert_eq!(
        state
            .session_service
            .queued_prompts_snapshot(&id)
            .await
            .len(),
        1
    );
    state.acp_supervisor.test_flush_worker_commands(&id).await;
    assert_eq!(*cmds.lock().expect("cmd log mutex poisoned"), ["prompt"]);
}

/// #3649: a review that loses the guard to a turn-starting submission is
/// refused, with nothing sent or published.
#[tokio::test]
async fn diff_comments_refuse_to_open_a_turn_another_submission_started() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-3649-diff".to_string();
    let state = structured_state(&id, true);
    let cmds = state
        .acp_supervisor
        .test_insert_worker_cmd_recording(&id)
        .await;

    let winner = state.session_service.prompt_submission(&id).await;
    let mut claims = state.session_service.watch_submission_claims();
    let handler = {
        let state = Arc::clone(&state);
        let id = id.clone();
        async move {
            acp_prompt_diff_comments(State(state), Path(id), diff_req("review this"))
                .await
                .into_response()
        }
    };
    tokio::pin!(handler);
    assert!(futures_util::poll!(&mut handler).is_pending());
    assert_eq!(claims.try_recv().expect("contender reached claim"), id);

    state
        .acp_supervisor
        .publish_user_prompt_with_attachments(&id, "the winning turn".into(), &[], None, false)
        .await;
    drop(winner);

    let response = tokio::time::timeout(Duration::from_secs(10), handler)
        .await
        .expect("the handler must finish once the winner releases the session");
    assert_eq!(response.status(), StatusCode::CONFLICT);
    state.acp_supervisor.test_flush_worker_commands(&id).await;
    assert!(cmds.lock().expect("cmd log mutex poisoned").is_empty());
    assert!(!published(&state, &id, |e| matches!(
        e,
        Event::UserDiffCommentsPrompt { .. }
    )));
}

#[test]
fn prompt_persist_tiers() {
    // The recency-only tier does not clear a peer's archive by itself.
    let mut disk = crate::session::Instance::new("s", "/tmp/x");
    disk.view = crate::session::View::Structured;
    disk.last_accessed_at = Some(chrono::Utc::now() - chrono::Duration::seconds(60));
    disk.archived_at = Some(chrono::Utc::now() - chrono::Duration::seconds(30));
    crate::server::session_service::apply_prompt_persist_to_disk(&mut disk, false);
    assert!(disk.archived_at.is_some());
    assert!(disk.last_accessed_at > disk.archived_at);

    // A wake persist lifts a sunk row.
    let mut disk = crate::session::Instance::new("s", "/tmp/x");
    disk.view = crate::session::View::Structured;
    disk.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::minutes(10));
    crate::server::session_service::apply_prompt_persist_to_disk(&mut disk, true);
    assert!(disk.snoozed_until.is_none());
    assert!(disk.last_accessed_at.is_some());
}
