use std::time::Duration;

use super::*;
use crate::acp::state::Event;

fn structured_instance(id: &str, idle: bool) -> crate::session::Instance {
    let mut inst = crate::session::Instance::new(id, &format!("/tmp/aoe-{id}"));
    inst.id = id.to_string();
    inst.source_profile = "default".into();
    inst.view = crate::session::View::Structured;
    if idle {
        inst.status = crate::session::Status::Idle;
    }
    inst
}

async fn seed_native_row(state: &Arc<AppState>) {
    crate::session::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
    let row = state.instances.read().await[0].clone();
    crate::server::test_support::seed_instances_on_disk_for_test(
        &row.source_profile,
        vec![row.clone()],
    );
    crate::server::test_support::refresh_canonical_metadata_for_test(state).await;
}

async fn structured_state(id: &str, idle: bool) -> Arc<AppState> {
    let state =
        crate::server::test_support::build_test_app_state(vec![structured_instance(id, idle)]);
    seed_native_row(&state).await;
    state
}

fn prompt_req(text: &str) -> Result<Json<PromptRequest>, axum::extract::rejection::JsonRejection> {
    Ok(Json(PromptRequest {
        text: text.to_string(),
        attachments: Vec::new(),
        prompt_id: None,
        no_revive: false,
        queued_prompt_id: None,
    }))
}

fn send_now_req(
    text: &str,
    queued_prompt_id: &str,
) -> Result<Json<PromptRequest>, axum::extract::rejection::JsonRejection> {
    Ok(Json(PromptRequest {
        text: text.to_string(),
        attachments: Vec::new(),
        prompt_id: None,
        no_revive: false,
        queued_prompt_id: Some(queued_prompt_id.to_string()),
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
        queued_prompt_id: None,
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
async fn failing_start_state(
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
    let inst = structured_instance(id, false);
    let state =
        crate::server::test_support::build_test_app_state_with_launcher(vec![inst], launcher);
    seed_native_row(&state).await;
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
    let state = structured_state(&id, false).await;
    let reservation = match state
        .acp_supervisor
        .begin_resume(
            &id,
            ResumeKind::Spawn,
            crate::process::worker_registry::ResumeAuthority::Automatic,
        )
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
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-stop-order".to_string();
    let state = structured_state(&id, false).await;
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
        let state = structured_state(&id, false).await;
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
        let state = structured_state(&id, false).await;
        let service = &state.session_service;
        let id_ref = id.as_str();
        let decide = move || async move {
            let _guard = service
                .admit_prompt_submission(&SessionCaller::User, id_ref)
                .await
                .expect("session exists");
            service
                .prompt_dispatch_under_submission(id_ref, "", false, false)
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
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let id = format!("sess-no-revive-dormant-{dormant}");
        let state = structured_state(&id, false).await;
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
        seed_native_row(&state).await;

        let response = acp_prompt(
            State(Arc::clone(&state)),
            Path(id.clone()),
            no_revive_prompt_req("hello"),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT, "dormant={dormant}");
        assert_eq!(
            crate::daemon::ApiErrorCode::from_headers(response.status(), response.headers(), false),
            Some(crate::daemon::ApiErrorCode::NoRevive),
            "dormant={dormant}"
        );
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
                let (state, launches) = failing_start_state(&id, rate_limited).await;
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
        let (state, launches) = failing_start_state(&id, true).await;
        seed(&state, &id);
        state.instances.write().await[0].pending_initial_turn =
            Some(crate::session::PendingInitialTurn {
                text: "queued before the park".to_string(),
                attachments: Vec::new(),
                synthesized: true,
            });
        seed_native_row(&state).await;

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

/// #4109: a rate-limit park can retain a stale active-turn latch until the
/// worker's `Stopped` event is persisted. `no_revive` still refuses while the
/// worker is absent, regardless of the dispatch queue reason.
#[tokio::test]
async fn no_revive_refuses_a_workerless_rate_limit_park_with_a_stale_turn() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-no-revive-stale-rate-limit".to_string();
    let (state, launches) = failing_start_state(&id, true).await;
    seed_elapsed_rate_limit_park(&state, &id, true);
    state.instances.write().await[0].pending_initial_turn =
        Some(crate::session::PendingInitialTurn {
            text: "queued before the park".to_string(),
            attachments: Vec::new(),
            synthesized: true,
        });
    seed_native_row(&state).await;

    assert!(!state.acp_supervisor.is_running(&id).await);
    assert!(
        state
            .session_service
            .fold_control_state(&id)
            .await
            .turn_active,
        "the fixture must retain a stale active-turn latch"
    );

    let response = acp_prompt(
        State(Arc::clone(&state)),
        Path(id.clone()),
        no_revive_prompt_req("hello"),
    )
    .await
    .into_response();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        launches.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no_revive must not start a worker"
    );
    assert!(
        state
            .session_service
            .queued_prompts_snapshot(&id)
            .await
            .is_empty(),
        "a refusal must not enqueue the prompt"
    );
    assert!(
        state.acp_event_store.rate_limit_park(&id).is_some(),
        "the rate-limit park must remain intact"
    );
    assert!(
        state.instances.read().await[0]
            .pending_initial_turn
            .is_some(),
        "the refusal must preserve the pending initial turn"
    );
}

/// #3621: a direct prompt parks while a drain owns the session, and the drain
/// that follows leaves its row queued behind the turn the prompt started.
#[tokio::test]
async fn a_direct_prompt_and_the_queue_drain_cannot_both_own_the_same_turn() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-3621-race".to_string();
    let state = structured_state(&id, true).await;
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
            None,
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

#[tokio::test]
async fn queued_explicit_work_follows_a_failed_prompt_but_not_a_failed_runtime() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    for (reason, interaction, late) in [
        ("prompt_failed", None, false),
        ("prompt_failed", Some("approval"), false),
        ("prompt_failed", Some("elicitation"), false),
        ("prompt_failed", Some("approval"), true),
        ("prompt_failed", Some("elicitation"), true),
        ("session_reset_failed", None, false),
        ("startup_error", None, false),
    ] {
        let id = format!(
            "queued-after-{reason}-{}-{late}",
            interaction.unwrap_or("none")
        );
        let state = structured_state(&id, true).await;
        let commands = state
            .acp_supervisor
            .test_insert_worker_cmd_recording(&id)
            .await;
        let first = acp_prompt(
            State(state.clone()),
            Path(id.clone()),
            prompt_req("first explicit request"),
        )
        .await
        .into_response();
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        let second = acp_prompt(
            State(state.clone()),
            Path(id.clone()),
            prompt_req("second explicit request"),
        )
        .await
        .into_response();
        assert_eq!(second.status(), StatusCode::ACCEPTED);
        assert_eq!(
            state
                .session_service
                .queued_prompts_snapshot(&id)
                .await
                .len(),
            1
        );
        let inject = |event: Event| {
            let seq = state.acp_event_store.highest_seq_checked(&id).unwrap() + 1;
            state.acp_event_store.record(&id, seq, &event).unwrap();
            state.acp_control_cache.apply_if_cached(&id, seq, &event);
            state.acp_supervisor.hydrate_seqs([(id.clone(), seq)]);
        };
        let (mut requested, resolved) = match interaction {
            Some("approval") => {
                let approval = crate::acp::permissions::build_approval(
                    crate::acp::state::ToolCall {
                        id: "pending-tool".into(),
                        name: "shell".into(),
                        kind: "execute".into(),
                        args_preview: "{}".into(),
                        started_at: chrono::Utc::now(),
                        parent_tool_call_id: None,
                        memory_recall: None,
                        diffs: Vec::new(),
                    },
                    Vec::new(),
                );
                let nonce = approval.nonce.clone();
                (
                    Some(Event::ApprovalRequested { approval }),
                    Some(Event::ApprovalResolved {
                        nonce,
                        decision: crate::acp::approvals::ApprovalDecision::Allow,
                    }),
                )
            }
            Some("elicitation") => {
                let nonce = crate::acp::approvals::Nonce("pending-question".into());
                (
                    Some(Event::ElicitationRequested {
                        elicitation: crate::acp::elicitations::Elicitation {
                            nonce: nonce.clone(),
                            message: "Choose".into(),
                            title: None,
                            description: None,
                            tool_call_id: None,
                            questions: Vec::new(),
                            requested_at: chrono::Utc::now(),
                            resolved: None,
                        },
                    }),
                    Some(Event::ElicitationResolved {
                        nonce,
                        outcome: crate::acp::elicitations::ElicitationOutcome::Declined,
                        answers: Vec::new(),
                    }),
                )
            }
            _ => (None, None),
        };
        if !late {
            if let Some(event) = requested.take() {
                inject(event);
            }
        }
        if reason == "prompt_failed" {
            inject(Event::PromptRuntimeError {
                message: "first request failed".into(),
                evidence: None,
            });
        }
        if reason == "startup_error" {
            state
                .acp_supervisor
                .publish_startup_error(&id, "transport closed".into());
        } else {
            state
                .acp_supervisor
                .synthesize_stopped_for_orphan(&id, reason);
        }
        if let Some(event) = requested.take() {
            inject(event);
        }
        if let Some(event) = resolved {
            inject(event);
        }
        let control = state.session_service.fold_control_state(&id).await;
        assert!(!control.turn_active);
        assert_eq!(control.sidebar_status(), crate::session::Status::Error);
        if reason == "prompt_failed" {
            assert_eq!(
                control.sidebar_phase,
                crate::acp::state::SidebarPhase::PromptFailed
            );
            assert!(
                control.pending_approvals.is_empty() && control.pending_elicitations.is_empty()
            );
        }
        state.instances.write().await[0].status = crate::session::Status::Error;
        state.session_service.drain_queued_prompts_once(&id).await;
        state.acp_supervisor.test_flush_worker_commands(&id).await;
        let follows = reason == "prompt_failed";
        assert_eq!(
            commands.lock().unwrap().len(),
            if follows { 2 } else { 1 },
            "{reason}"
        );
        assert_eq!(
            state
                .session_service
                .queued_prompts_snapshot(&id)
                .await
                .len(),
            usize::from(!follows),
            "{reason}"
        );
        let prompts: Vec<_> = state
            .acp_event_store
            .replay_from(&id, 0)
            .into_iter()
            .filter_map(|(_, event)| match event {
                Event::UserPromptSent {
                    text, synthesized, ..
                } => {
                    assert!(!synthesized, "failed work is not redelivered");
                    Some(text)
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            prompts,
            if follows {
                vec!["first explicit request", "second explicit request"]
            } else {
                vec!["first explicit request"]
            }
        );
    }
}

/// #3649: a review that loses the guard to a turn-starting submission is
/// refused, with nothing sent or published.
#[tokio::test]
async fn diff_comments_refuse_to_open_a_turn_another_submission_started() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-3649-diff".to_string();
    let state = structured_state(&id, true).await;
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
        .publish_user_prompt_with_attachments(
            &id,
            "the winning turn".into(),
            &[],
            None,
            false,
            None,
        )
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

/// Seed the rate-limit state a resume acts on: prompt A interrupted by a limit
/// whose park window has already elapsed, the state the install reads.
/// Backdated an hour, under the redelivery cap. `busy` adds a running turn,
/// which is what makes B queue rather than dispatch.
fn seed_elapsed_rate_limit_park(state: &AppState, id: &str, busy: bool) {
    let store = &state.acp_event_store;
    let long_ago = chrono::Utc::now() - chrono::Duration::hours(1);
    let at = long_ago.timestamp_millis();
    let mut events = vec![
        Event::UserPromptSent {
            text: "interrupted prompt A".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: false,
            origin_plugin: None,
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
    if busy {
        events.push(Event::ThinkingStarted);
    }
    for (seq, event) in events.iter().enumerate() {
        store
            .record_at(id, seq as u64 + 1, event, at)
            .expect("seed the rate-limit park");
    }
    state
        .acp_supervisor
        .hydrate_seqs([(id.to_string(), store.highest_seq(id))]);
    let park = store.rate_limit_park(id).expect("an armed park");
    assert!(!park.cap_reached, "the fixture must stay under the cap");
    assert!(
        store.rate_limited_turn_prompt(id).is_some(),
        "the fixture must leave a continuation to install"
    );
}

/// The pending continuation as it stands on disk, which the in-memory slot can
/// disagree with. Reads the profile the writer resolved, and fails rather than
/// returning `None` when the row is missing.
async fn persisted_pending_turn(
    state: &AppState,
    id: &str,
) -> Option<crate::session::PendingInitialTurn> {
    let profile = state
        .instances
        .read()
        .await
        .iter()
        .find(|i| i.id == id)
        .expect("seeded session")
        .source_profile
        .clone();
    crate::server::test_support::load_instances_from_disk_for_test(&profile)
        .into_iter()
        .find(|i| i.id == id)
        .expect("the session row must be on disk, or this assertion is vacuous")
        .pending_initial_turn
}

/// #4092: a manual prompt may not slip between a rate-limit continuation's
/// store read and its installation. The producer holds the session's
/// submission authority across that window, so B is provably serialized
/// behind it rather than overtaking it and being overwritten.
#[tokio::test]
#[serial_test::serial]
async fn a_manual_prompt_cannot_overtake_a_continuation_install() {
    // `queued` is the #4079 shape: a live worker with a turn in flight parks B
    // on the server queue. `direct` has no turn, so B starts one.
    for queued in [false, true] {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let label = if queued { "queued" } else { "direct" };
        let id = format!("sess-4092-{label}");
        let inst = structured_instance(&id, false);
        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        seed_native_row(&state).await;
        state
            .acp_supervisor
            .test_insert_worker_cmd_recording(&id)
            .await;
        seed_elapsed_rate_limit_park(&state, &id, queued);
        // Causal precondition: the durable read this test ends on is live.
        state
            .session_service
            .set_pending_initial_turn(&id, "probe".into(), Vec::new())
            .await;
        assert_eq!(
            persisted_pending_turn(&state, &id).await.map(|t| t.text),
            Some("probe".to_string()),
            "{label}: a probe turn must reach disk before absence means anything"
        );
        state.session_service.clear_pending_initial_turn(&id).await;
        assert!(persisted_pending_turn(&state, &id).await.is_none());

        let mut barrier = state.session_service.arm_install_barrier();
        let mut claims = state.session_service.watch_submission_claims();
        let producer = tokio::spawn({
            let state = Arc::clone(&state);
            let id = id.clone();
            async move {
                let Some(submission) = state.session_service.try_prompt_submission(&id).await
                else {
                    return;
                };
                // B is blocked on the guard, so the queue is still empty here
                // and the producer must decide the continuation stands.
                let outcome = crate::server::acp_reconciler::install_rate_limit_continuation(
                    &state, &id, submission,
                )
                .await;
                assert!(matches!(
                    outcome,
                    crate::server::acp_reconciler::ContinuationOutcome::Stands
                ));
            }
        });
        // Causal barrier: A has been read from the store and is not installed.
        let (read_id, release) = tokio::time::timeout(Duration::from_secs(10), barrier.recv())
            .await
            .expect("the producer must reach the barrier")
            .expect("the barrier tap outlives the producer");
        assert_eq!(read_id, id, "{label}");
        assert!(
            state.instances.read().await[0]
                .pending_initial_turn
                .is_none(),
            "{label}: the barrier must sit before the installation"
        );

        let prompt = {
            let state = Arc::clone(&state);
            let id = id.clone();
            async move {
                acp_prompt(State(state), Path(id), prompt_req("manual prompt B"))
                    .await
                    .into_response()
            }
        };
        tokio::pin!(prompt);
        // B reached its submission claim, so it is contending for the session
        // the producer is holding. Its accepted decision must not land while
        // that install is in progress; the deadline bounds the wait, the claim
        // above is what establishes the contention.
        assert!(futures_util::poll!(&mut prompt).is_pending());
        assert_eq!(
            claims.try_recv().expect("the prompt must reach its claim"),
            id,
            "{label}"
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut prompt)
                .await
                .is_err(),
            "{label}: a submission must not land while the continuation install holds the session"
        );

        let _ = release.send(());
        tokio::time::timeout(Duration::from_secs(10), producer)
            .await
            .expect("the producer must finish once released")
            .expect("the producer must not panic");
        let response = tokio::time::timeout(Duration::from_secs(30), prompt)
            .await
            .expect("the prompt must finish once the producer releases the session");
        assert_eq!(response.status(), StatusCode::ACCEPTED, "{label}");

        assert_eq!(
            state.instances.read().await[0]
                .pending_initial_turn
                .as_ref()
                .map(|t| t.text.as_str()),
            None,
            "{label}: B superseded A, so no continuation may survive it"
        );
        assert!(
            persisted_pending_turn(&state, &id).await.is_none(),
            "{label}: the superseded continuation must not reach disk"
        );
    }
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

/// Send now on a queued plugin message keeps the sender the server stored, and the
/// row is consumed exactly once.
#[tokio::test]
async fn sending_a_queued_plugin_row_now_keeps_its_stored_sender() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-send-now";
    let state = structured_state(id, true).await;
    let _cmds = state
        .acp_supervisor
        .test_insert_worker_cmd_recording(id)
        .await;
    state
        .session_service
        .enqueue_prompt(
            id,
            "q1".into(),
            "PR #7 was merged".into(),
            vec![],
            None,
            Some("cron".into()),
        )
        .await
        .expect("session exists");

    let response = acp_prompt(
        State(Arc::clone(&state)),
        Path(id.to_string()),
        send_now_req("PR #7 was merged", "q1"),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert!(published(&state, id, |e| matches!(
        e,
        Event::UserPromptSent { origin_plugin: Some(p), .. } if p == "cron"
    )));
    assert!(state
        .session_service
        .queued_prompts_snapshot(id)
        .await
        .is_empty());

    let again = acp_prompt(
        State(Arc::clone(&state)),
        Path(id.to_string()),
        send_now_req("PR #7 was merged", "q1"),
    )
    .await
    .into_response();
    assert_eq!(again.status(), StatusCode::CONFLICT);
}

async fn state_with_queued_plugin_row(id: &str, idle: bool) -> Arc<AppState> {
    let state = structured_state(id, idle).await;
    state
        .session_service
        .enqueue_prompt(
            id,
            "q1".into(),
            "PR #7 was merged".into(),
            vec![],
            None,
            Some("cron".into()),
        )
        .await
        .expect("session exists");
    state
}

/// Different text must not be published under the plugin's name.
#[tokio::test]
async fn send_now_refuses_text_that_differs_from_the_stored_plugin_row() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-send-now-spoof";
    let state = state_with_queued_plugin_row(id, true).await;
    let _cmds = state
        .acp_supervisor
        .test_insert_worker_cmd_recording(id)
        .await;

    let response = acp_prompt(
        State(Arc::clone(&state)),
        Path(id.to_string()),
        send_now_req("something the plugin never said", "q1"),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(!published(&state, id, |e| matches!(
        e,
        Event::UserPromptSent { .. }
    )));
    assert_eq!(
        state
            .session_service
            .queued_prompts_snapshot(id)
            .await
            .len(),
        1
    );
}

/// A send naming a row must carry that row's text, so a 202 never covers text the daemon
/// does not hold (a queue edit racing Send now).
#[tokio::test]
async fn send_now_refuses_text_that_differs_from_a_user_row_too() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-send-now-stale";
    let state = structured_state(id, true).await;
    let _cmds = state
        .acp_supervisor
        .test_insert_worker_cmd_recording(id)
        .await;
    state
        .session_service
        .enqueue_prompt(id, "q1".into(), "stored".into(), vec![], None, None)
        .await
        .expect("session exists");

    let response = acp_prompt(
        State(Arc::clone(&state)),
        Path(id.to_string()),
        send_now_req("edited elsewhere", "q1"),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(!published(&state, id, |e| matches!(
        e,
        Event::UserPromptSent { .. }
    )));
    let queue = state.session_service.queued_prompts_snapshot(id).await;
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].text, "stored");
}

fn shot_ref() -> crate::daemon::PromptAttachmentRef {
    crate::daemon::PromptAttachmentRef {
        id: "att-1".into(),
        kind: crate::daemon::PromptAttachmentKind::Image,
        mime_type: "image/png".into(),
        name: Some("shot.png".into()),
        size: 6,
    }
}

async fn state_with_attachment_row(id: &str, store_bytes: bool) -> Arc<AppState> {
    let state = structured_state(id, true).await;
    state
        .session_service
        .enqueue_prompt(
            id,
            "q1".into(),
            "caption".into(),
            vec![shot_ref()],
            None,
            None,
        )
        .await
        .expect("session exists");
    if store_bytes {
        assert!(state.acp_event_store.record_pending_attachment(
            id,
            "q1",
            &crate::acp::event_store::AttachmentBlob {
                id: "att-1".into(),
                kind: crate::daemon::PromptAttachmentKind::Image,
                mime_type: "image/png".into(),
                name: Some("shot.png".into()),
                data: b"stored".to_vec(),
            },
        ));
    }
    state
}

/// Send now delivers the bytes the daemon stored for the row. Request attachments, even a
/// valid different one of the same count, never replace them.
#[tokio::test]
async fn send_now_delivers_the_rows_stored_attachments_not_the_requests() {
    use base64::Engine;
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-send-now-attachments";
    let state = state_with_attachment_row(id, true).await;
    let _cmds = state
        .acp_supervisor
        .test_insert_worker_cmd_recording(id)
        .await;

    let response = acp_prompt(
        State(Arc::clone(&state)),
        Path(id.to_string()),
        Ok(Json(PromptRequest {
            text: "caption".into(),
            attachments: vec![crate::acp::protocol::PromptAttachmentUpload {
                kind: crate::daemon::PromptAttachmentKind::Image,
                mime_type: "image/png".into(),
                data: base64::engine::general_purpose::STANDARD.encode(b"swapped-bytes"),
                name: Some("other.png".into()),
            }],
            prompt_id: None,
            no_revive: false,
            queued_prompt_id: Some("q1".into()),
        })),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let sent = state
        .acp_event_store
        .replay_from(id, 0)
        .into_iter()
        .find_map(|(_, e)| match e {
            Event::UserPromptSent { attachments, .. } => Some(attachments),
            _ => None,
        })
        .expect("prompt published");
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].name.as_deref(), Some("shot.png"));
    assert_eq!(sent[0].size, 6);
    assert!(state
        .session_service
        .queued_prompts_snapshot(id)
        .await
        .is_empty());
}

/// A row whose bytes are gone cannot be sent as if it never had attachments.
#[tokio::test]
async fn send_now_keeps_a_row_whose_attachment_bytes_are_gone() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-send-now-expired";
    let state = state_with_attachment_row(id, false).await;
    let _cmds = state
        .acp_supervisor
        .test_insert_worker_cmd_recording(id)
        .await;

    let response = acp_prompt(
        State(Arc::clone(&state)),
        Path(id.to_string()),
        send_now_req("caption", "q1"),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(!published(&state, id, |e| matches!(
        e,
        Event::UserPromptSent { .. }
    )));
    assert_eq!(
        state.session_service.queued_prompts_snapshot(id).await[0]
            .attachments
            .len(),
        1
    );
}

/// A refused send leaves the row for a retry instead of losing it.
#[tokio::test]
async fn a_refused_send_now_keeps_the_queued_row() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-send-now-refused";
    let state = state_with_queued_plugin_row(id, false).await;
    let response = acp_prompt(
        State(Arc::clone(&state)),
        Path(id.to_string()),
        Ok(Json(PromptRequest {
            text: "PR #7 was merged".into(),
            attachments: Vec::new(),
            prompt_id: None,
            no_revive: true,
            queued_prompt_id: Some("q1".into()),
        })),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let queue = state.session_service.queued_prompts_snapshot(id).await;
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].origin_plugin.as_deref(), Some("cron"));
}

/// A busy agent parks the send again: the same row stays, with its sender, and no copy is made.
#[tokio::test]
async fn send_now_behind_a_busy_agent_keeps_the_one_queued_row() {
    let _app_dir = crate::session::test_support::isolate_app_dir();
    let id = "sess-send-now-busy";
    let state = state_with_queued_plugin_row(id, true).await;
    let _cmds = state
        .acp_supervisor
        .test_insert_worker_cmd_recording(id)
        .await;
    state
        .acp_supervisor
        .publish_user_prompt_with_attachments(id, "the user's turn".into(), &[], None, false, None)
        .await;

    let response = acp_prompt(
        State(Arc::clone(&state)),
        Path(id.to_string()),
        send_now_req("PR #7 was merged", "q1"),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let queue = state.session_service.queued_prompts_snapshot(id).await;
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].id, "q1");
    assert_eq!(queue[0].origin_plugin.as_deref(), Some("cron"));
}
