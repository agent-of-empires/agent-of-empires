use std::time::Duration;

use super::*;
use crate::acp::state::Event;

fn structured_state(id: &str, idle: bool) -> Arc<AppState> {
    let mut inst = crate::session::Instance::new(id, &format!("/tmp/aoe-{id}"));
    inst.id = id.to_string();
    inst.view = crate::session::View::Structured;
    if idle {
        inst.status = crate::session::Status::Idle;
    }
    crate::server::test_support::build_test_app_state(vec![inst])
}

fn prompt_req(text: &str) -> Result<Json<PromptRequest>, axum::extract::rejection::JsonRejection> {
    Ok(Json(PromptRequest {
        text: text.to_string(),
        attachments: Vec::new(),
        prompt_id: None,
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

/// #3688: an exhausted rate-limit park is sendable at the shared decision point.
#[tokio::test]
async fn exhausted_rate_limit_park_is_sendable_at_the_shared_decision_point() {
    let id = "sess-3688-shared".to_string();
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
    );
    park_on_exhausted_rate_limit(&state, &id);
    assert_eq!(decide().await, PromptDispatch::Sent);
}

/// #3688: a prompt or review on an exhausted park drives a resume instead of
/// queueing or refusing. No reservation is held: one would make `is_running`
/// true and skip the park probe; the failed spawn's `AgentStartupError`
/// proves the resume ran.
#[tokio::test]
async fn turn_on_an_exhausted_park_resumes_instead_of_queueing() {
    for diff_comments in [false, true] {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let id = format!("sess-3688-{diff_comments}");
        let state = structured_state(&id, false);
        park_on_exhausted_rate_limit(&state, &id);

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
                prompt_req("start a fresh retry budget"),
            )
            .await
            .into_response()
        };

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(state
            .session_service
            .queued_prompts_snapshot(&id)
            .await
            .is_empty());
        assert!(published(&state, &id, |e| matches!(
            e,
            Event::AgentStartupError { .. }
        )));
        assert!(!published(&state, &id, |e| matches!(
            e,
            Event::UserPromptSent { .. } | Event::UserDiffCommentsPrompt { .. }
        )));
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
