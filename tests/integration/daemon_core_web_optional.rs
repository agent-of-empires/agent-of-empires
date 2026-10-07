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

/// The REST surface is unconditional, so a Node-free `cargo build` still
/// answers it. The SPA fallback and `/assets/*` live behind `web`: with it a
/// browser path reaches the embedded `index.html`, without it every dashboard
/// path must 404 rather than serve a stale or empty shell.
#[tokio::test]
#[serial_test::parallel]
async fn only_dashboard_routes_depend_on_the_web_feature() {
    let dashboard = if cfg!(feature = "web") {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };
    let mut cases = vec![
        ("/api/sessions", StatusCode::OK),
        ("/", dashboard),
        ("/sessions", dashboard),
    ];
    if !cfg!(feature = "web") {
        cases.extend([
            ("/assets/index-abc123.js", StatusCode::NOT_FOUND),
            ("/sw.js", StatusCode::NOT_FOUND),
        ]);
    }
    for (uri, expected) in cases {
        assert_eq!(status_for(uri).await, expected, "{uri}");
    }
}
