use super::{ApiErrorCode, DaemonClient, DaemonClientError, ERROR_CODE_HEADER};
use axum::{http::StatusCode as Code, routing::post, Json, Router};
use reqwest::StatusCode;

async fn serve(app: Router) -> (DaemonClient, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (DaemonClient::new(&base, None).unwrap(), server)
}

#[tokio::test]
async fn a_batch_start_is_one_post_answered_per_item() {
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route(
        "/api/suggestions/start-batch",
        post(move |Json(body): Json<serde_json::Value>| async move {
            sent.send(body).unwrap();
            Json(serde_json::json!({ "results": [
                { "provider_id": "gh", "id": "1", "status": 201, "body": { "session_id": "s" } },
                { "provider_id": "gh", "id": "2", "status": 400, "body": { "error": "no_project" } },
            ] }))
        }),
    );
    let (client, server) = serve(app).await;
    let body = serde_json::json!({ "items": [
        { "provider_id": "gh", "id": "1" }, { "provider_id": "gh", "id": "2" },
    ] });
    let answers = client.start_suggestions(&body, 2).await.unwrap();
    server.abort();
    assert_eq!(received.recv().await.unwrap(), body);
    let seen: Vec<(&str, &str, u16)> = answers
        .iter()
        .map(|a| (a.provider_id.as_str(), a.id.as_str(), a.status))
        .collect();
    assert_eq!(seen, [("gh", "1", 201), ("gh", "2", 400)]);
    assert_eq!(answers[1].body["error"], "no_project");
}

#[tokio::test]
async fn pre_trust_is_patched_through_the_settings_api() {
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route(
        "/api/settings",
        axum::routing::patch(move |Json(body): Json<serde_json::Value>| async move {
            sent.send(body).unwrap();
            Json(serde_json::json!({}))
        }),
    );
    let (client, server) = serve(app).await;
    let patch = serde_json::json!({ "session": { "pre_trust_agent_folders": true } });
    client.patch_settings(&patch).await.unwrap();
    server.abort();
    assert_eq!(received.recv().await.unwrap(), patch);
}

#[tokio::test]
async fn the_home_group_pref_is_patched_through_the_suggestions_api() {
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let app = Router::new().route(
        "/api/suggestions/prefs",
        axum::routing::patch(move |Json(body): Json<serde_json::Value>| async move {
            sent.send(body).unwrap();
            Json(serde_json::json!({}))
        }),
    );
    let (client, server) = serve(app).await;
    let patch = serde_json::json!({ "sidebar": true });
    client.set_suggestion_prefs(&patch).await.unwrap();
    server.abort();
    assert_eq!(received.recv().await.unwrap(), patch);
}

#[tokio::test]
async fn item_changes_name_the_profile_they_were_chosen_under() {
    let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
    let record = |sent: tokio::sync::mpsc::UnboundedSender<serde_json::Value>| {
        post(move |Json(body): Json<serde_json::Value>| async move {
            sent.send(body).unwrap();
            Json(serde_json::json!({ "title": "T", "initial_prompt": "", "tool": "claude" }))
        })
    };
    let app = Router::new()
        .route("/api/suggestions/refresh", record(sent.clone()))
        .route("/api/suggestions/dismiss", record(sent.clone()))
        .route("/api/suggestions/prepare", record(sent));
    let (client, server) = serve(app).await;
    client.refresh_suggestions(Some("work")).await.unwrap();
    client
        .set_suggestion_dismissed(Some("work"), "gh", "1", true)
        .await
        .unwrap();
    client
        .prepare_suggestion(Some("work"), "gh", "1")
        .await
        .unwrap();
    server.abort();
    for _ in 0..3 {
        assert_eq!(received.recv().await.unwrap()["expected_profile"], "work");
    }
}

#[tokio::test]
async fn a_start_returns_the_typed_outcome_of_a_409_and_posts_the_approval() {
    let app = Router::new().route(
        "/api/suggestions/start",
        post(|Json(body): Json<serde_json::Value>| async move {
            (
                Code::CONFLICT,
                Json(serde_json::json!({ "error": "hooks_need_trust", "echo": body })),
            )
        }),
    );
    let (client, server) = serve(app).await;
    let body = serde_json::json!({ "provider_id": "gh", "id": "1", "approve_hooks": true });
    let (status, answer) = client.start_suggestion(&body).await.unwrap();
    assert_eq!(status, 409);
    assert_eq!(answer["error"], "hooks_need_trust");
    assert_eq!(answer["echo"], body);
    server.abort();
}

#[tokio::test]
async fn a_prepare_refusal_keeps_the_daemons_explanation_or_its_typed_code() {
    let explained = Router::new().route(
        "/api/suggestions/prepare",
        post(|| async {
            (
                Code::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "validation_failed",
                    "message": "The expanded prompt exceeds 10 bytes"
                })),
            )
        }),
    );
    let coded = Router::new().route(
        "/api/suggestions/prepare",
        post(|| async {
            (
                Code::INTERNAL_SERVER_ERROR,
                [(ERROR_CODE_HEADER, "start_failed")],
                Json(serde_json::json!({ "error": "start_failed" })),
            )
        }),
    );
    let plain = Router::new().route(
        "/api/suggestions/prepare",
        post(|| async { (Code::BAD_GATEWAY, "upstream down") }),
    );
    for (app, status, code, expected) in [
        (
            explained,
            StatusCode::BAD_REQUEST,
            None,
            "The expanded prompt exceeds 10 bytes",
        ),
        (
            coded,
            StatusCode::INTERNAL_SERVER_ERROR,
            Some(ApiErrorCode::StartFailed),
            "",
        ),
        (plain, StatusCode::BAD_GATEWAY, None, ""),
    ] {
        let (client, server) = serve(app).await;
        let error = client
            .prepare_suggestion(None, "gh", "1")
            .await
            .unwrap_err();
        server.abort();
        assert!(
            matches!(&error, DaemonClientError::Status { status: s, code: c, body, .. }
                if *s == status && *c == code && body == expected),
            "{error:?}"
        );
    }
}
