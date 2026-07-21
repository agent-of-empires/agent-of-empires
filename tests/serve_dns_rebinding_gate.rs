//! Integration coverage for the DNS-rebinding gate (#2735, Tier 1).
//!
//! Drives the real `build_router` stack through `tower::ServiceExt::oneshot`
//! (no socket bind) against a test `AppState` seeded with an allowlist and a
//! token, and asserts the `access_policy` middleware behavior end-to-end: a
//! hostile `Host` is rejected with 403 before auth, while a listed host (e.g.
//! the auto-injected tunnel host) clears the gate and reaches auth (401 for an
//! unauthenticated, non-loopback request). Mirrors the harness style of
//! `serve_disk_reload_helper_equivalence.rs` via `build_test_app_state*`.

#![cfg(feature = "serve")]

use agent_of_empires::server::test_support::{
    build_router_for_test, build_test_app_state_with_policy,
};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use std::net::SocketAddr;
use tower::ServiceExt;

fn vecs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn remote_peer() -> SocketAddr {
    "203.0.113.7:5555".parse().unwrap()
}

#[tokio::test]
async fn hostile_host_is_rejected_with_403() {
    let state = build_test_app_state_with_policy(
        Vec::new(),
        vecs(&["localhost", "127.0.0.1", "::1", "x.trycloudflare.com"]),
        vecs(&["https://x.trycloudflare.com"]),
        Some("secret-token".to_string()),
    );
    let app = build_router_for_test(state);
    let mut req = Request::builder()
        .uri("/api/sessions")
        .header("host", "attacker.example.com")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(remote_peer()));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn injected_tunnel_host_passes_the_gate() {
    let state = build_test_app_state_with_policy(
        Vec::new(),
        vecs(&["localhost", "127.0.0.1", "::1", "x.trycloudflare.com"]),
        vecs(&["https://x.trycloudflare.com"]),
        Some("secret-token".to_string()),
    );
    let app = build_router_for_test(state);
    let mut req = Request::builder()
        .uri("/api/sessions")
        .header("host", "x.trycloudflare.com")
        .header("origin", "https://x.trycloudflare.com")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(remote_peer()));
    let resp = app.oneshot(req).await.unwrap();
    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "auto-injected tunnel host + origin must clear the DNS-rebinding gate"
    );
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "past the gate, the unauthenticated request reaches auth and gets 401"
    );
}
