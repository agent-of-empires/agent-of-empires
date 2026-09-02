//! The daemon is core; only the dashboard bundle is optional (#3619).
//!
//! Drives the real `build_router` stack through `tower::ServiceExt::oneshot`
//! (no socket bind) and asserts the split holds in whichever feature corner
//! the suite is compiled for: the API is reachable either way, while the
//! dashboard routes exist only under `web`. Without that assertion a stray
//! `#[cfg(feature = "web")]` around an API route, or a dashboard route that
//! leaked out of the gate, compiles clean and ships wrong.

use agent_of_empires::server::test_support::{
    build_router_for_test, build_test_app_state_with_policy,
};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use std::net::SocketAddr;
use tower::ServiceExt;

fn loopback_peer() -> SocketAddr {
    "127.0.0.1:5555".parse().unwrap()
}

/// Status for `uri` through the full router stack. The host allowlist is
/// seeded and no token is set, so `access_policy` and auth both pass and the
/// status reflects routing alone. Without the allowlist every path answers
/// 403 and the assertions below would prove nothing.
async fn status_for(uri: &str) -> StatusCode {
    let allowed: Vec<String> = ["localhost", "127.0.0.1", "::1"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let state = build_test_app_state_with_policy(Vec::new(), allowed, Vec::new(), None);
    let app = build_router_for_test(state);
    let mut req = Request::builder()
        .uri(uri)
        .header("host", "localhost")
        .body(Body::empty())
        .unwrap();
    req.extensions_mut().insert(ConnectInfo(loopback_peer()));
    app.oneshot(req).await.unwrap().status()
}

/// The REST surface is unconditional: a Node-free `cargo build` still answers
/// it. This is the whole point of making the daemon core, so it is asserted in
/// both feature corners rather than only under `web`.
#[tokio::test]
async fn api_routes_are_served_without_the_dashboard_bundle() {
    assert_eq!(
        status_for("/api/sessions").await,
        StatusCode::OK,
        "the daemon API must not depend on the dashboard bundle"
    );
}

/// The SPA fallback and the Vite output live behind `web`. Without it a
/// browser path must 404 rather than serve a stale or empty shell.
#[cfg(not(feature = "web"))]
#[tokio::test]
async fn dashboard_routes_are_absent_without_web() {
    for uri in ["/", "/sessions", "/assets/index-abc123.js", "/sw.js"] {
        assert_eq!(
            status_for(uri).await,
            StatusCode::NOT_FOUND,
            "{uri} must 404 in a build without the dashboard bundle"
        );
    }
}

/// With `web` the same paths resolve: `/assets/*` and the SPA fallback are
/// registered, so a browser path reaches the embedded `index.html` instead of
/// falling through to axum's default 404.
#[cfg(feature = "web")]
#[tokio::test]
async fn spa_fallback_serves_the_bundle_with_web() {
    assert_eq!(
        status_for("/").await,
        StatusCode::OK,
        "the SPA fallback must serve index.html when the bundle is embedded"
    );
    assert_eq!(
        status_for("/sessions").await,
        StatusCode::OK,
        "an unknown browser path must fall back to index.html, not 404"
    );
}
