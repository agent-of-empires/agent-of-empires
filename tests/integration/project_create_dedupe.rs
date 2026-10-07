//! `POST /api/projects` without a `name` derives one from the path basename; two paths sharing a
//! basename must both register (see `projects::unique_name`) instead of the second getting a 409.

use agent_of_empires::server::test_support::{build_router_for_test, build_test_app_state};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use std::net::SocketAddr;
use tower::ServiceExt;

fn loopback() -> SocketAddr {
    "127.0.0.1:5557".parse().unwrap()
}

fn request(method: Method, uri: &str, body: Body) -> Request<Body> {
    // An IP-literal Host clears the DNS-rebinding gate without an allowlist
    // entry, so the request reaches the handler.
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "127.0.0.1")
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(loopback()));
    req
}

async fn create_project(
    app: &axum::Router,
    path: &std::path::Path,
) -> (StatusCode, serde_json::Value) {
    let body = serde_json::json!({ "path": path.to_string_lossy() });
    let resp = app
        .clone()
        .oneshot(request(
            Method::POST,
            "/api/projects",
            Body::from(body.to_string()),
        ))
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    (status, json)
}

#[tokio::test]
#[serial_test::serial]
async fn create_project_dedupes_auto_derived_name_on_basename_collision() {
    let home = crate::common::setup_temp_home();

    // Two distinct repos whose basename is the same ("demo"), so the
    // request-omitted name derives to the identical string for both.
    let parent_a = home.path().join("a");
    let parent_b = home.path().join("b");
    let repo_a = parent_a.join("demo");
    let repo_b = parent_b.join("demo");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();

    let state = build_test_app_state(Vec::new());
    let app = build_router_for_test(state);

    let (status_a, body_a) = create_project(&app, &repo_a).await;
    assert_eq!(
        status_a,
        StatusCode::CREATED,
        "first create failed: {body_a}"
    );
    assert_eq!(body_a["name"], "demo");

    let (status_b, body_b) = create_project(&app, &repo_b).await;
    assert_eq!(
        status_b,
        StatusCode::CREATED,
        "second create with colliding basename must auto-dedupe, not 409: {body_b}"
    );
    assert_eq!(body_b["name"], "demo-2");
}
