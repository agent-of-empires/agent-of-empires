//! Route-level coverage for CityHall client mode (#7).
//!
//! Drives the real `build_router` stack through `tower::ServiceExt::oneshot`
//! (no socket bind) against a test `AppState` with `cityhall_mode = true`, and
//! asserts that every sensitive route the locked-down client hides in the UI is
//! actually closed server-side: the handler returns 403 with the canonical
//! `{"error":"cityhall_mode"}` body before doing any work. This is the runtime
//! counterpart to the static `every_cityhall_gated_handler_has_guard` audit in
//! `src/server/api/mod.rs`; together they stop a reachable route from slipping
//! back in. Loopback + a null token clears the DNS-rebinding gate and auth, so
//! the only 403 source under test is the CityHall guard (asserted via the body).

#![cfg(feature = "serve")]

use agent_of_empires::server::test_support::{
    build_router_for_test, build_test_app_state_cityhall,
};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use std::net::SocketAddr;
use tower::ServiceExt;

fn loopback() -> SocketAddr {
    "127.0.0.1:5555".parse().unwrap()
}

fn request(method: Method, uri: &str, body: Body) -> Request<Body> {
    // An IP-literal Host clears the DNS-rebinding gate without an allowlist
    // entry (it cannot be rebound), so the request reaches the handler.
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

/// Send one request through the full router against a CityHall `AppState` and
/// assert it is refused with the canonical CityHall 403 (status + body), so a
/// coincidental 403 from the host gate or auth cannot mask a missing guard.
async fn assert_cityhall_blocked(method: Method, uri: &str, body: Body) {
    let state = build_test_app_state_cityhall(Vec::new());
    let app = build_router_for_test(state);
    let resp = app.oneshot(request(method, uri, body)).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "{uri} must be forbidden in CityHall mode"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("cityhall_mode"),
        "{uri} returned 403 but not the CityHall body (got: {text})"
    );
}

#[tokio::test]
async fn clone_repo_is_blocked() {
    assert_cityhall_blocked(
        Method::POST,
        "/api/git/clone",
        Body::from(r#"{"url":"https://example.com/x.git"}"#),
    )
    .await;
}

#[tokio::test]
async fn git_branches_is_blocked() {
    assert_cityhall_blocked(Method::GET, "/api/git/branches?path=/tmp", Body::empty()).await;
}

#[tokio::test]
async fn git_is_repo_is_blocked() {
    assert_cityhall_blocked(Method::GET, "/api/git/is-repo?path=/tmp", Body::empty()).await;
}

#[tokio::test]
async fn read_output_is_blocked() {
    assert_cityhall_blocked(
        Method::GET,
        "/api/sessions/does-not-exist/output",
        Body::empty(),
    )
    .await;
}

#[tokio::test]
async fn acp_spawn_is_blocked() {
    assert_cityhall_blocked(Method::POST, "/api/sessions/x/acp/spawn", Body::from("{}")).await;
}

#[tokio::test]
async fn acp_shutdown_is_blocked() {
    assert_cityhall_blocked(Method::DELETE, "/api/sessions/x/acp", Body::empty()).await;
}

#[tokio::test]
async fn acp_set_mode_is_blocked() {
    assert_cityhall_blocked(
        Method::POST,
        "/api/sessions/x/acp/mode",
        Body::from(r#"{"mode_id":"plan"}"#),
    )
    .await;
}

#[tokio::test]
async fn create_project_is_blocked() {
    assert_cityhall_blocked(
        Method::POST,
        "/api/projects",
        Body::from(r#"{"path":"/tmp"}"#),
    )
    .await;
}

#[tokio::test]
async fn mcp_keep_is_blocked() {
    assert_cityhall_blocked(
        Method::POST,
        "/api/mcp/servers/x/keep",
        Body::from(r#"{"agent":"claude"}"#),
    )
    .await;
}

#[tokio::test]
async fn mcp_drop_is_blocked() {
    assert_cityhall_blocked(
        Method::POST,
        "/api/mcp/servers/x/drop",
        Body::from(r#"{"agent":"claude"}"#),
    )
    .await;
}

#[tokio::test]
async fn plugin_install_is_blocked() {
    assert_cityhall_blocked(
        Method::POST,
        "/api/plugins/install",
        Body::from(r#"{"source":"gh:owner/repo","expected_fingerprint":"x"}"#),
    )
    .await;
}

#[tokio::test]
async fn plugin_set_enabled_is_blocked() {
    assert_cityhall_blocked(
        Method::POST,
        "/api/plugins/x/enabled",
        Body::from(r#"{"enabled":true}"#),
    )
    .await;
}

#[tokio::test]
async fn uncurated_profile_setting_is_blocked() {
    // The profile-settings PATCH stays open for the curated trash toggles, but
    // an uncurated leaf must be refused before it reaches the merge/write.
    assert_cityhall_blocked(
        Method::PATCH,
        "/api/profiles/default/settings",
        Body::from(r#"{"session":{"yolo_mode":true}}"#),
    )
    .await;
}
