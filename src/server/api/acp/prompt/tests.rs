use std::time::Duration;

use super::*;
use crate::acp::state::Event;

fn structured_instance(id: &str, idle: bool) -> crate::session::Instance {
    let mut inst = crate::session::Instance::new(id, &format!("/tmp/aoe-{id}"));
    inst.id = id.to_string();
    inst.source_profile = "default".into();
    inst.view = crate::session::View::Structured;
    crate::server::test_support::select_retained_for_test(&mut inst);
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
        .unwrap()
        .iter()
        .any(|stored| pred(stored.event.as_ref()))
}

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
    use crate::acp::supervisor::ResumeKind;

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
        Some(r) => r,
        None => panic!("expected a fresh reservation"),
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
#[serial_test::serial]
async fn turn_on_a_park_attaches_the_retained_actor_then_delivers_only_new_explicit_input() {
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            for diff_comments in [false, true] {
                let mut live = crate::server::test_support::managed_control_session(
                    scope,
                    if diff_comments {
                        "quota-explicit-review"
                    } else {
                        "quota-explicit-text"
                    },
                )
                .await;
                live.park("old interrupted input must not replay").await;
                let id = live.id().to_owned();
                let original = live
                    .state
                    .acp_supervisor
                    .current_native_availability(&id)
                    .await
                    .unwrap()
                    .owner;
                let frontier = live.state.acp_event_store.highest_seq(&id);
                live.client = scope
                    .reconcile_after(live.client, |state, id| async move {
                        let response = if diff_comments {
                            acp_prompt_diff_comments(
                                State(state),
                                Path(id),
                                diff_req("please address these"),
                            )
                            .await
                            .into_response()
                        } else {
                            acp_prompt(State(state), Path(id), prompt_req("a distinct new input"))
                                .await
                                .into_response()
                        };
                        assert_eq!(response.status(), StatusCode::ACCEPTED);
                    })
                    .await;
                live.client.ready().await;
                live.state = live.client.app_state();
                let request = live.next_prompt().await;
                assert_eq!(request["params"]["sessionId"], live.sid);
                let expected = if diff_comments {
                    "please address these"
                } else {
                    "a distinct new input"
                };
                assert_eq!(request["params"]["prompt"][0]["text"], expected);
                let events = live
                    .state
                    .acp_event_store
                    .replay_from(&id, frontier)
                    .unwrap();
                assert!(!events.iter().any(|stored| matches!(
                    stored.event.as_ref(),
                    Event::RateLimitAutoResumed { .. }
                )));
                assert!(live
                    .state
                    .session_service
                    .queued_prompts_snapshot(&id)
                    .await
                    .is_empty());
                assert_eq!(
                    live.state
                        .acp_supervisor
                        .current_native_availability(&id)
                        .await
                        .unwrap()
                        .owner,
                    original
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
                live.finish_prompt(&request, None).await;
            }
        })
    })
    .await;
}

/// #4081 review: a prompt into either park dispatches `Sent` (see
/// `turn_on_a_park_resumes_instead_of_queueing`), so `no_revive` is enforced
/// by `send_turn`'s resume decision. The refusal must start no worker and
/// leave the park, the queue, and a pending initial turn as they were.
#[tokio::test]
#[serial_test::serial]
async fn no_revive_preserves_workerless_parked_or_active_work_without_side_effects() {
    use crate::acp::event_store::ControlKnowledge::Known;
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            for parked in [false, true] {
                let mut live = crate::server::test_support::managed_control_session(
                    scope,
                    if parked {
                        "no-revive-park"
                    } else {
                        "no-revive-active"
                    },
                )
                .await;
                let active = live.start_prompt("original governed work").await;
                if parked {
                    live.finish_prompt(&active, Some("rate_limit")).await;
                }
                let id = live.id().to_owned();
                live.state
                    .session_service
                    .set_pending_initial_turn(&id, "existing continuation".into(), Vec::new())
                    .await;
                let native_methods = live.native.requests();
                live.client = scope
                    .reconcile_after(live.client, |state, id| async move {
                        let before = state.acp_event_store.highest_seq(&id);
                        let budget = state
                            .acp_event_store
                            .rate_limit_redelivery_streak(&id)
                            .unwrap();
                        assert_eq!(
                            state.acp_event_store.has_in_flight_main_turn(&id).unwrap(),
                            Known(!parked)
                        );
                        let response = acp_prompt(
                            State(state.clone()),
                            Path(id.clone()),
                            no_revive_prompt_req("must refuse"),
                        )
                        .await
                        .into_response();
                        assert_eq!(response.status(), StatusCode::CONFLICT);
                        assert_eq!(state.acp_event_store.highest_seq(&id), before);
                        assert_eq!(
                            state
                                .acp_event_store
                                .rate_limit_redelivery_streak(&id)
                                .unwrap(),
                            budget
                        );
                        assert!(state
                            .session_service
                            .queued_prompts_snapshot(&id)
                            .await
                            .is_empty());
                        assert_eq!(
                            persisted_pending_turn(&state, &id).await.unwrap().text,
                            "existing continuation"
                        );
                        crate::server::acp_reconciler::ensure_rate_limit_worker(
                            &state,
                            &id,
                            crate::process::worker_registry::ResumeAuthority::Explicit,
                            true,
                        )
                        .await
                        .unwrap();
                    })
                    .await;
                live.client.ready().await;
                live.state = live.client.app_state();
                assert_eq!(live.native.requests(), native_methods);
                if !parked {
                    live.finish_prompt(&active, None).await;
                }
            }
        })
    })
    .await;
}

/// #3621: a direct prompt parks while a drain owns the session, and the drain
/// that follows leaves its row queued behind the turn the prompt started.
#[tokio::test]
#[serial_test::serial]
async fn a_direct_prompt_and_the_queue_drain_cannot_both_own_the_same_turn() {
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            let mut live =
                crate::server::test_support::managed_control_session(scope, "queue-direct-race")
                    .await;
            let id = live.id().to_owned();
            let state = live.state.clone();
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
            let request = live.next_prompt().await;
            assert_eq!(request["params"]["prompt"][0]["text"], "typed mid-delivery");
            tokio::time::timeout(Duration::from_secs(10), async {
                while !state
                    .session_service
                    .fold_control_state(&id)
                    .await
                    .unwrap()
                    .turn_active
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the actual Native admission owns the canonical turn before draining");

            state.session_service.drain_queued_prompts_once(&id).await;
            assert_eq!(
                state
                    .session_service
                    .queued_prompts_snapshot(&id)
                    .await
                    .len(),
                1
            );
            assert_eq!(
                live.native
                    .requests()
                    .iter()
                    .filter(|method| *method == "session/prompt")
                    .count(),
                1
            );
            live.finish_prompt(&request, None).await;
        })
    })
    .await;
}

async fn request_queue_interaction(
    live: &mut crate::server::test_support::ManagedControlSession,
    kind: &str,
) -> crate::acp::approvals::Nonce {
    let (method, params) = if kind == "approval" {
        (
            "session/request_permission",
            serde_json::json!({
                "sessionId": live.sid,
                "toolCall": {"toolCallId": "pending-tool", "title": "Wait for user"},
                "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"}]
            }),
        )
    } else {
        (
            "elicitation/create",
            serde_json::json!({
                "mode": "form", "sessionId": live.sid, "message": "Choose",
                "requestedSchema": {"type": "object", "properties": {"answer": {"type": "string"}}}
            }),
        )
    };
    live.native
        .send(serde_json::json!({
            "jsonrpc": "2.0", "id": "queue-interaction", "method": method, "params": params,
        }))
        .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match live
                .client
                .next_event()
                .await
                .expect("actual callback reaches the daemon")
            {
                Event::ApprovalRequested { approval } if kind == "approval" => {
                    return approval.nonce
                }
                Event::ElicitationRequested { elicitation } if kind == "elicitation" => {
                    return elicitation.nonce
                }
                _ => {}
            }
        }
    })
    .await
    .expect("actual Original nonce is published")
}

#[tokio::test]
#[serial_test::serial]
async fn queued_explicit_work_follows_a_failed_prompt_but_not_a_failed_runtime() {
    use crate::acp::supervisor::test_support::managed::with_managed;
    for (runtime_loss, interaction, late) in [
        (false, None, false),
        (false, Some("approval"), false),
        (false, Some("elicitation"), false),
        (false, Some("approval"), true),
        (false, Some("elicitation"), true),
        (true, None, false),
    ] {
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = crate::server::test_support::managed_control_session(
                    scope,
                    "queue-after-failure",
                )
                .await;
                let id = live.id().to_owned();
                let state = live.state.clone();
                let first = live.start_prompt("first explicit request").await;
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
                let interaction_nonce = if !late {
                    if let Some(kind) = interaction {
                        Some(request_queue_interaction(&mut live, kind).await)
                    } else {
                        None
                    }
                } else {
                    None
                };
                let _runtime_pause = if runtime_loss {
                    Some(live.client.pause_drain().await)
                } else {
                    None
                };
                if runtime_loss {
                    live.native.close_stdout().await;
                    tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            if state
                                .session_service
                                .prompt_native_unavailability(&id)
                                .await
                                .unwrap()
                                .is_some()
                            {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("physical Native EOF refuses queued delivery");
                } else {
                    live.finish_prompt(&first, Some("prompt_failed")).await;
                    if let Some(kind) = interaction {
                        let nonce = if late {
                            request_queue_interaction(&mut live, kind).await
                        } else {
                            interaction_nonce.unwrap()
                        };
                        if kind == "approval" {
                            state
                                .acp_supervisor
                                .resolve_permission(
                                    &id,
                                    nonce,
                                    crate::acp::approvals::ApprovalDecision::Allow,
                                    Some("allow".into()),
                                )
                                .await
                                .unwrap();
                        } else {
                            state
                                .acp_supervisor
                                .resolve_elicitation(
                                    &id,
                                    nonce,
                                    crate::acp::elicitations::ElicitationResolution::Decline,
                                )
                                .await
                                .unwrap();
                        }
                        let reply =
                            tokio::time::timeout(Duration::from_secs(10), live.native.reply())
                                .await
                                .expect("actual pending callback settles");
                        assert_eq!(reply["id"], "queue-interaction");
                    }
                    tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            let control =
                                state.session_service.fold_control_state(&id).await.unwrap();
                            assert!(!control.turn_active);
                            assert!(control.connection_ready);
                            assert!(control.connection_unavailable.is_none());
                            if control.pending_approvals.is_empty()
                                && control.pending_elicitations.is_empty()
                            {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("settled Original callbacks cannot strand explicit queued work");
                }
                state
                    .instances
                    .write()
                    .await
                    .iter_mut()
                    .find(|row| row.id == id)
                    .unwrap()
                    .status = crate::session::Status::Error;
                state.session_service.drain_queued_prompts_once(&id).await;
                if !runtime_loss {
                    let next = live.next_prompt().await;
                    assert_eq!(
                        next["params"]["prompt"][0]["text"],
                        "second explicit request"
                    );
                    live.finish_prompt(&next, None).await;
                }
                assert_eq!(
                    live.native
                        .requests()
                        .iter()
                        .filter(|method| *method == "session/prompt")
                        .count(),
                    if runtime_loss { 1 } else { 2 },
                );
                assert_eq!(
                    state
                        .session_service
                        .queued_prompts_snapshot(&id)
                        .await
                        .len(),
                    usize::from(runtime_loss),
                );
                let prompts: Vec<_> = state
                    .acp_event_store
                    .replay_from(&id, 0)
                    .unwrap()
                    .into_iter()
                    .filter_map(|stored| match Arc::unwrap_or_clone(stored.event) {
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
                    if runtime_loss {
                        vec!["first explicit request"]
                    } else {
                        vec!["first explicit request", "second explicit request"]
                    }
                );
            })
        })
        .await;
    }
}

/// #3649: a review that loses the guard to a turn-starting submission is
/// refused, with nothing sent or published.
#[tokio::test]
#[serial_test::serial]
async fn diff_comments_refuse_to_open_a_turn_another_submission_started() {
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            let mut live =
                crate::server::test_support::managed_control_session(scope, "queue-diff-race")
                    .await;
            let id = live.id().to_owned();
            let state = live.state.clone();

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
                .session_service
                .send_turn(
                    &SessionCaller::User,
                    &id,
                    SendTurnRequest {
                        resume_authority:
                            crate::process::worker_registry::ResumeAuthority::Explicit,
                        resume_reservation: None,
                        text: "the winning turn",
                        attachments: &[],
                        woke_idle_dormant: false,
                        prompt_id: None,
                        synthesized: false,
                        no_revive: false,
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("winning Native input refused: {error}"));
            let winning_prompt = live.next_prompt().await;
            assert_eq!(
                winning_prompt["params"]["prompt"][0]["text"],
                "the winning turn"
            );
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if state
                        .session_service
                        .fold_control_state(&id)
                        .await
                        .unwrap()
                        .turn_active
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("actual Native admission durably opens the winning turn");
            let methods_before = live.native.requests();
            drop(winner);

            let response = tokio::time::timeout(Duration::from_secs(10), handler)
                .await
                .expect("the handler must finish once the winner releases the session");
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(live.native.requests(), methods_before);
            assert!(!published(&state, &id, |e| matches!(
                e,
                Event::UserDiffCommentsPrompt { .. }
            )));
            assert!(state
                .session_service
                .queued_prompts_snapshot(&id)
                .await
                .is_empty());
            live.finish_prompt(&winning_prompt, None).await;
        })
    })
    .await;
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
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            let mut live = crate::server::test_support::managed_control_session(
                scope,
                "continuation-install-race",
            )
            .await;
            live.park("interrupted prompt A").await;
            let state = live.state.clone();
            let id = live.id().to_owned();
            state
                .session_service
                .set_pending_initial_turn(&id, "probe".into(), Vec::new())
                .await;
            assert_eq!(
                persisted_pending_turn(&state, &id).await.unwrap().text,
                "probe"
            );
            state.session_service.clear_pending_initial_turn(&id).await;
            assert!(persisted_pending_turn(&state, &id).await.is_none());
            let mut barrier = state.session_service.arm_install_barrier();
            let mut claims = state.session_service.watch_submission_claims();
            let producer_state = state.clone();
            let producer_id = id.clone();
            let producer = scope.spawn_helper(async move {
                let submission = producer_state
                    .session_service
                    .try_prompt_submission(&producer_id)
                    .await
                    .expect("continuation producer owns the real session exclusion");
                let outcome = crate::server::acp_reconciler::install_rate_limit_continuation(
                    &producer_state,
                    &producer_id,
                    &submission,
                )
                .await
                .expect("known interrupted input remains available");
                assert!(matches!(
                    outcome,
                    crate::server::acp_reconciler::ContinuationOutcome::Stands
                ));
            });
            let (read_id, release) = tokio::time::timeout(Duration::from_secs(10), barrier.recv())
                .await
                .unwrap()
                .unwrap();
            let release = scope.hold_release(release);
            assert_eq!(read_id, id);
            assert!(persisted_pending_turn(&state, &id).await.is_none());
            let prompt = acp_prompt(
                State(state.clone()),
                Path(id.clone()),
                prompt_req("manual prompt B"),
            );
            tokio::pin!(prompt);
            assert!(futures_util::poll!(&mut prompt).is_pending());
            assert_eq!(claims.try_recv().unwrap(), id);
            assert!(tokio::time::timeout(Duration::from_millis(20), &mut prompt)
                .await
                .is_err());
            assert!(release.release());
            producer.await.unwrap();
            let response = tokio::time::timeout(Duration::from_secs(10), prompt)
                .await
                .unwrap()
                .into_response();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let request = live.next_prompt().await;
            assert_eq!(request["params"]["prompt"][0]["text"], "manual prompt B");
            assert!(persisted_pending_turn(&state, &id).await.is_none());
            live.finish_prompt(&request, None).await;
        })
    })
    .await;
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
#[serial_test::serial]
async fn sending_a_queued_plugin_row_now_keeps_its_stored_sender() {
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            let mut live =
                crate::server::test_support::managed_control_session(scope, "queue-plugin-sender")
                    .await;
            let owned_id = live.id().to_owned();
            let id = owned_id.as_str();
            let state = live.state.clone();
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
            let request = live.next_prompt().await;
            assert_eq!(request["params"]["prompt"][0]["text"], "PR #7 was merged");
            live.finish_prompt(&request, None).await;
        })
    })
    .await;
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
#[serial_test::serial]
async fn send_now_refuses_text_that_differs_from_the_stored_plugin_row() {
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            let live =
                crate::server::test_support::managed_control_session(scope, "queue-plugin-spoof")
                    .await;
            let owned_id = live.id().to_owned();
            let id = owned_id.as_str();
            let state = live.state.clone();
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
                .unwrap();

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
        })
    })
    .await;
}

/// A send naming a row must carry that row's text, so a 202 never covers text the daemon
/// does not hold (a queue edit racing Send now).
#[tokio::test]
#[serial_test::serial]
async fn send_now_refuses_text_that_differs_from_a_user_row_too() {
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            let live =
                crate::server::test_support::managed_control_session(scope, "queue-user-stale")
                    .await;
            let owned_id = live.id().to_owned();
            let id = owned_id.as_str();
            let state = live.state.clone();
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
        })
    })
    .await;
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

/// Send now delivers the bytes the daemon stored for the row. Request attachments, even a
/// valid different one of the same count, never replace them.
#[tokio::test]
#[serial_test::serial]
async fn send_now_delivers_the_rows_stored_attachments_not_the_requests() {
    use crate::acp::supervisor::test_support::managed::with_managed;
    use base64::Engine;
    with_managed(|scope| {
        Box::pin(async move {
            let mut live =
                crate::server::test_support::managed_control_session(scope, "queue-attachments")
                    .await;
            let owned_id = live.id().to_owned();
            let id = owned_id.as_str();
            let state = live.state.clone();
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
                .unwrap();
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
                .unwrap()
                .into_iter()
                .find_map(|stored| match Arc::unwrap_or_clone(stored.event) {
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
            let request = live.next_prompt().await;
            assert_eq!(
                request["params"]["prompt"][1]["data"],
                base64::engine::general_purpose::STANDARD.encode(b"stored"),
            );
            live.finish_prompt(&request, None).await;
        })
    })
    .await;
}

/// A row whose bytes are gone cannot be sent as if it never had attachments.
#[tokio::test]
#[serial_test::serial]
async fn send_now_keeps_a_row_whose_attachment_bytes_are_gone() {
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            let live = crate::server::test_support::managed_control_session(
                scope,
                "queue-attachment-expired",
            )
            .await;
            let owned_id = live.id().to_owned();
            let id = owned_id.as_str();
            let state = live.state.clone();
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
                .unwrap();

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
        })
    })
    .await;
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
#[serial_test::serial]
async fn send_now_behind_a_busy_agent_keeps_the_one_queued_row() {
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            let mut live =
                crate::server::test_support::managed_control_session(scope, "queue-send-now-busy")
                    .await;
            let owned_id = live.id().to_owned();
            let id = owned_id.as_str();
            let state = live.state.clone();
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
                .unwrap();
            let active = live.start_prompt("the user's turn").await;

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
            assert_eq!(
                live.native
                    .requests()
                    .iter()
                    .filter(|method| *method == "session/prompt")
                    .count(),
                1
            );
            live.finish_prompt(&active, None).await;
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn native_loss_and_legacy_refusal_precede_prompt_touch_queue_or_launch() {
    use crate::process::worker_registry::{self, WorkerRecord};
    {
        let _home = crate::session::test_support::isolate_app_dir();
        let id = "sess-legacy-owner";
        let (state, launches) = failing_start_state(id, false).await;
        {
            let mut rows = state.instances.write().await;
            rows[0].mark_idle_dormant();
            rows[0].snoozed_until = Some(chrono::Utc::now() + chrono::Duration::days(3));
            rows[0].pending_initial_turn = Some(crate::session::PendingInitialTurn {
                text: "existing continuation".into(),
                attachments: Vec::new(),
                synthesized: true,
            });
        }
        seed_native_row(&state).await;
        state
            .session_service
            .enqueue_prompt(
                id,
                "retained".into(),
                "existing queued intent".into(),
                vec![],
                None,
                None,
            )
            .await
            .unwrap();
        state.instances.write().await[0].last_error = Some("retained operation diagnostic".into());
        let socket = worker_registry::socket_path_for(id).unwrap();
        let mut record = WorkerRecord::new(
            id.into(),
            std::process::id(),
            socket.clone(),
            "retained-agent".into(),
            "claude".into(),
            std::env::temp_dir(),
            None,
            vec![],
            vec![],
            Some("retained-native".into()),
            Some("default".into()),
        )
        .with_generation(41);
        record.control_protocol_version = None;
        worker_registry::save(&record).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(
            crate::process::worker::control_socket_sibling(&socket),
        )
        .unwrap();
        listener.set_nonblocking(true).unwrap();
        let before = serde_json::to_value(&state.instances.read().await[0]).unwrap();
        let bytes = std::fs::read(worker_registry::record_path(id).unwrap()).unwrap();
        let queue_before =
            serde_json::to_value(state.session_service.queued_prompts_snapshot(id).await).unwrap();
        for diff_comments in [false, true] {
            let response = if diff_comments {
                acp_prompt_diff_comments(
                    State(state.clone()),
                    Path(id.into()),
                    diff_req("must not wake"),
                )
                .await
                .into_response()
            } else {
                acp_prompt(
                    State(state.clone()),
                    Path(id.into()),
                    prompt_req("must not wake"),
                )
                .await
                .into_response()
            };
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(
                serde_json::to_value(&state.instances.read().await[0]).unwrap(),
                before
            );
            assert_eq!(
                serde_json::to_value(state.session_service.queued_prompts_snapshot(id).await)
                    .unwrap(),
                queue_before
            );
            assert_eq!(
                state.instances.read().await[0].last_error.as_deref(),
                Some("retained operation diagnostic")
            );
            assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(
                std::fs::read(worker_registry::record_path(id).unwrap()).unwrap(),
                bytes
            );
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
            assert!(!published(&state, id, |event| matches!(
                event,
                Event::UserPromptSent { .. } | Event::AgentStartupError { .. }
            )));
        }
        state.session_service.wake_dormant_for_queue_drain(id).await;
        state.session_service.drain_pending_initial_turn(id).await;
        assert_eq!(
            serde_json::to_value(&state.instances.read().await[0]).unwrap(),
            before
        );
        assert_eq!(
            state.instances.read().await[0].last_error.as_deref(),
            Some("retained operation diagnostic")
        );
        assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(!published(&state, id, |event| matches!(
            event,
            Event::UserPromptSent { .. }
        )));
    }
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            let live =
                crate::server::test_support::managed_control_session(scope, "refused-native-loss")
                    .await;
            let id = live.id().to_owned();
            let state = live.state.clone();
            state
                .session_service
                .set_pending_initial_turn(&id, "existing continuation".into(), vec![])
                .await;
            state
                .session_service
                .enqueue_prompt(
                    &id,
                    "retained".into(),
                    "existing queued intent".into(),
                    vec![],
                    None,
                    None,
                )
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .status
                        == crate::session::Status::Idle
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("actual assignment reaches the status mirror before the refusal window");
            let _pause = live.client.pause_drain().await;
            let row = {
                let mut rows = state.instances.write().await;
                let row = rows.iter_mut().find(|row| row.id == id).unwrap();
                row.mark_idle_dormant();
                row.snoozed_until = Some(chrono::Utc::now() + chrono::Duration::days(3));
                row.last_error = Some("retained operation diagnostic".into());
                row.clone()
            };
            crate::session::Storage::open_unwatched(&row.source_profile)
                .unwrap()
                .update(|rows, _| {
                    *rows.iter_mut().find(|saved| saved.id == id).unwrap() = row.clone();
                    Ok(())
                })
                .unwrap();
            live.native.close_stdout().await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if state
                        .session_service
                        .prompt_native_unavailability(&id)
                        .await
                        .unwrap()
                        .is_some()
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("actual Native EOF is qualified before any gesture");
            let before = serde_json::to_value(&row).unwrap();
            let storage = crate::session::Storage::open_unwatched(&row.source_profile).unwrap();
            let disk_before = std::fs::read(storage.sessions_path()).unwrap();
            let registry = crate::process::worker_registry::record_path(&id).unwrap();
            let record_before = std::fs::read(&registry).ok();
            let methods_before = live.native.requests();
            let frontier = state.acp_event_store.highest_seq(&id);
            for diff_comments in [false, true] {
                let response = if diff_comments {
                    acp_prompt_diff_comments(
                        State(state.clone()),
                        Path(id.clone()),
                        diff_req("must not wake"),
                    )
                    .await
                    .into_response()
                } else {
                    acp_prompt(
                        State(state.clone()),
                        Path(id.clone()),
                        prompt_req("must not wake"),
                    )
                    .await
                    .into_response()
                };
                assert_eq!(response.status(), StatusCode::CONFLICT);
                assert_eq!(
                    serde_json::to_value(
                        state
                            .instances
                            .read()
                            .await
                            .iter()
                            .find(|row| row.id == id)
                            .unwrap()
                    )
                    .unwrap(),
                    before,
                );
                assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), disk_before);
                assert_eq!(std::fs::read(&registry).ok(), record_before);
                assert_eq!(live.native.requests(), methods_before);
            }
            state
                .session_service
                .wake_dormant_for_queue_drain(&id)
                .await;
            state.session_service.drain_pending_initial_turn(&id).await;
            assert_eq!(
                serde_json::to_value(
                    state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                )
                .unwrap(),
                before,
            );
            assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), disk_before);
            assert_eq!(std::fs::read(&registry).ok(), record_before);
            assert_eq!(live.native.requests(), methods_before);
            assert!(!state
                .acp_event_store
                .replay_from(&id, frontier)
                .unwrap()
                .iter()
                .any(|stored| matches!(
                    stored.event.as_ref(),
                    Event::UserPromptSent { .. } | Event::AgentStartupError { .. }
                )));
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn current_owned_native_receipt_needs_no_historical_ready_and_refreshes_before_fold() {
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            let mut live =
                crate::server::test_support::managed_control_session(scope, "current-native").await;
            let session_id = live.id().to_owned();
            let id = session_id.as_str();
            let state = live.state.clone();
            let row = state
                .instances
                .read()
                .await
                .iter()
                .find(|row| row.id == id)
                .unwrap()
                .clone();
            // Remove retained readiness history, not the physically established Current source.
            state
                .acp_event_store
                .test_conn()
                .execute("DELETE FROM acp_events WHERE session_id = ?1", [id])
                .unwrap();
            state.acp_control_cache.forget(id);
            assert!(state.acp_event_store.replay_from(id, 0).unwrap().is_empty());
            for _ in 0..2 {
                let folded = state.session_service.fold_control_state(id).await.unwrap();
                assert!(folded.connection_ready && folded.connection_unavailable.is_none());
                assert!(
                    state
                        .session_service
                        .native_control_projection(
                            id,
                            folded.native_availability,
                            folded.native_control_seq
                        )
                        .await
                        .ready
                );
            }
            let current = state
                .acp_supervisor
                .current_native_availability(id)
                .await
                .unwrap();
            let failure = Event::PromptRuntimeError {
                message: "unpersisted diagnostic".into(),
                evidence: None,
                native_subject: None,
            };
            state
                .session_service
                .acp_control_cache
                .record_failed_observation(
                    id,
                    1,
                    Some(crate::acp::state::NativeEventSubject::channel(
                        current.scope.context,
                        current.owner,
                        &failure,
                    )),
                );
            let failed = state.session_service.fold_control_state(id).await.unwrap();
            assert!(failed.connection_ready && !failed.turn_active);
            assert_eq!(failed.sidebar_status(), crate::session::Status::Error);
            assert!(
                state.acp_event_store.replay_from(id, 0).unwrap().is_empty(),
                "failed-append semantics need no fabricated durable event or duplicate message"
            );
            let accepted = live.start_prompt("next explicit accepted intent").await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if state
                        .session_service
                        .fold_control_state(id)
                        .await
                        .unwrap()
                        .turn_active
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the old append-error hint cannot override a newer accepted main turn");
            live.finish_prompt(&accepted, None).await;
            let carried = state
                .acp_supervisor
                .current_native_availability(id)
                .await
                .unwrap();
            let _pause = live.client.pause_drain().await;
            live.native.close_stdout().await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if state
                        .session_service
                        .prompt_native_unavailability(id)
                        .await
                        .unwrap()
                        .is_some()
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("physical EOF is latched before the public drain event");
            let projection = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match state.session_service.acp_control_cache.native_projection(
                        id,
                        None,
                        0,
                        Some((
                            row.source_profile.clone(),
                            row.created_at,
                            row.lifecycle_generation,
                        )),
                        Some(carried.clone()),
                    ) {
                        Ok(projection) => {
                            assert!(!projection.ready, "latched loss cannot become Ready");
                            if projection.unavailable.is_some() {
                                break projection;
                            }
                        }
                        Err(wait) => assert!(wait.settle().await),
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the captured source qualifies its actual EOF after contention");
            assert!(!projection.ready);
            assert_eq!(
                projection.unavailable.unwrap().cause,
                crate::acp::state::AcpConnectionCause::StdoutEof
            );
            let folded = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let folded = state.session_service.fold_control_state(id).await.unwrap();
                    assert!(
                        !folded.connection_ready,
                        "a warm fold cannot heal loss before its public event"
                    );
                    if folded.connection_unavailable.is_some() {
                        break folded;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the warm fold qualifies the same actual EOF");
            assert_eq!(
                folded.connection_unavailable.unwrap().cause,
                crate::acp::state::AcpConnectionCause::StdoutEof
            );
        })
    })
    .await;
}

#[tokio::test]
async fn readable_known_worker_down_keeps_explicit_queue_admission_without_native_health() {
    let _home = crate::session::test_support::isolate_app_dir();
    let id = "sess-known-worker-down";
    let state = structured_state(id, true).await;
    let projection = state
        .session_service
        .native_control_projection(id, None, 0)
        .await;
    assert!(!projection.ready && projection.unavailable.is_none());
    assert!(projection.prompt_wake_eligible);
    let response = acp_prompt(
        State(state.clone()),
        Path(id.into()),
        prompt_req("explicit queued intent"),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(
        state.session_service.queued_prompts_snapshot(id).await[0].text,
        "explicit queued intent"
    );
    assert!(!published(&state, id, |event| matches!(
        event,
        Event::UserPromptSent { .. } | Event::AgentStartupError { .. }
    )));
    let _starting = state.acp_supervisor.test_hold_starting(id);
    let starting = state
        .session_service
        .native_control_projection(id, None, 0)
        .await;
    assert!(
        starting.prompt_wake_eligible && !starting.ready,
        "an existing initial-start claim admits intent without claiming native health"
    );
    let before = serde_json::to_value(&state.instances.read().await[0]).unwrap();
    let path = crate::process::worker_registry::record_path(id).unwrap();
    crate::session::atomic_write(&path, b"privateTokenMarker malformed registry").unwrap();
    let projection = state
        .session_service
        .native_control_projection(id, None, 0)
        .await;
    assert!(!projection.ready && !projection.prompt_wake_eligible);
    assert_eq!(
        projection.unavailable.unwrap().cause,
        crate::acp::state::AcpConnectionCause::OwnershipUnconfirmed
    );
    let response = acp_prompt(
        State(state.clone()),
        Path(id.into()),
        prompt_req("must not become known absence"),
    )
    .await
    .into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(
        serde_json::to_value(&state.instances.read().await[0]).unwrap(),
        before
    );
    assert_eq!(
        state
            .session_service
            .queued_prompts_snapshot(id)
            .await
            .len(),
        1
    );
    assert_eq!(
        std::fs::read(path).unwrap(),
        b"privateTokenMarker malformed registry"
    );
    let public = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(!std::str::from_utf8(&public)
        .unwrap()
        .contains("privateTokenMarker"));
}

#[tokio::test]
#[serial_test::serial]
async fn native_loss_after_final_precheck_preserves_pending_and_named_queue_intent() {
    use crate::acp::supervisor::test_support::managed::with_managed;
    with_managed(|scope| {
        Box::pin(async move {
            for text in ["explicit next request", "/clear"] {
                let live = crate::server::test_support::managed_control_session(
                    scope,
                    if text == "/clear" {
                        "late-loss-reset"
                    } else {
                        "late-loss-prompt"
                    },
                )
                .await;
                let id = live.id().to_owned();
                let state = live.state.clone();
                state
                    .session_service
                    .set_pending_initial_turn(&id, "existing governed continuation".into(), vec![])
                    .await;
                let methods_before = live.native.requests();
                state
                    .session_service
                    .enqueue_prompt(&id, "retained".into(), text.into(), vec![], None, None)
                    .await
                    .unwrap();
                let _current = state
                    .acp_supervisor
                    .current_native_availability(&id)
                    .await
                    .expect("the captured Native actor remains installed");
                assert!(
                    state
                        .session_service
                        .native_control_projection(&id, None, 0)
                        .await
                        .ready
                );
                let (arrived, release) = state.session_service.arm_native_prompt_send_gate();
                let request = tokio::spawn({
                    let state = state.clone();
                    let id = id.clone();
                    async move {
                        acp_prompt(State(state), Path(id), send_now_req(text, "retained"))
                            .await
                            .into_response()
                    }
                });
                tokio::time::timeout(Duration::from_secs(5), arrived)
                    .await
                    .unwrap()
                    .unwrap();
                let _pause = live.client.pause_drain().await;
                live.native.close_stdout().await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if state
                            .session_service
                            .prompt_native_unavailability(&id)
                            .await
                            .unwrap()
                            .is_some()
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("physical EOF crosses the final precheck before forwarding");
                release.send(()).unwrap();
                let response = tokio::time::timeout(Duration::from_secs(5), request)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(response.status(), StatusCode::CONFLICT, "{text}");
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["disposition"], "unavailable");
                assert_eq!(body["unavailability"]["cause"], "stdout_eof");
                assert_eq!(
                    persisted_pending_turn(&state, &id).await.unwrap().text,
                    "existing governed continuation"
                );
                assert_eq!(
                    state
                        .session_service
                        .queued_prompts_snapshot(&id)
                        .await
                        .len(),
                    1
                );
                assert_eq!(
                    live.native.requests(),
                    methods_before,
                    "no native prompt/reset after loss"
                );
                assert_eq!(
                    state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .acp_session_id
                        .as_deref(),
                    Some(live.sid.as_str())
                );
            }
        })
    })
    .await;
}
