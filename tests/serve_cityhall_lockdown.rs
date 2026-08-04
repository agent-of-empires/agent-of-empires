//! Route-level coverage for CityHall client mode (#7).
//!
//! Drives the real `build_router` stack through `tower::ServiceExt::oneshot`
//! (no socket bind) against a test `AppState` with `cityhall_mode = true`, and
//! asserts that every sensitive route the locked-down client hides in the UI is
//! actually closed server-side: the handler returns 403 with the canonical
//! `{"error":"cityhall_mode"}` body before doing any work. Reachability is
//! enforced by the default-deny `cityhall_gate` middleware; the runtime
//! counterpart of the build-time `every_mutating_route_is_cityhall_classified`
//! audit in `src/server/mod.rs`. Loopback + a null token clears the
//! DNS-rebinding gate and auth, so the only 403 source under test is the
//! CityHall boundary (asserted via the body).

#![cfg(feature = "serve")]

use agent_of_empires::server::test_support::{
    build_router_for_test, build_test_app_state_cityhall,
};
use agent_of_empires::session::{Instance, View};
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use std::net::SocketAddr;
use tower::ServiceExt;

/// A structured session (the only kind CityHall creates), for seeding state.
fn structured_session(id: &str) -> Instance {
    let mut inst = Instance::new(id, "/tmp/aoe-cityhall-test");
    inst.id = id.to_string();
    inst.view = View::Structured;
    inst
}

/// A plain/terminal session (default view), standing in for one created by the
/// TUI or another client on the same daemon.
fn plain_session(id: &str) -> Instance {
    let mut inst = Instance::new(id, "/tmp/aoe-cityhall-test");
    inst.id = id.to_string();
    inst
}

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

/// Every sensitive route the locked-down client hides in the UI must be closed
/// server-side. One test over a table: each row is a `(method, uri, body)` the
/// CityHall boundary must refuse with the canonical 403 body. Rows needing
/// distinct seeded state (the workspace-delete owner-vs-sibling discrimination,
/// the positive allow-list check) stay as their own tests below.
#[tokio::test]
async fn sensitive_routes_are_blocked() {
    let cases: &[(Method, &str, &str)] = &[
        // Git clone plus the two path probes: write $HOME, fetch the network,
        // probe the filesystem.
        (
            Method::POST,
            "/api/git/clone",
            r#"{"url":"https://example.com/x.git"}"#,
        ),
        (Method::GET, "/api/git/branches?path=/tmp", ""),
        (Method::GET, "/api/git/is-repo?path=/tmp", ""),
        // Terminal pane content, and the Files pane reads (#3088): enumerating
        // the workspace tree and reading file contents are the same
        // code-inspection surface as the gated diff reads.
        (Method::GET, "/api/sessions/does-not-exist/output", ""),
        (Method::GET, "/api/sessions/x/file?path=Cargo.toml", ""),
        (Method::GET, "/api/sessions/x/acp/files", ""),
        // ACP worker and agent lifecycle plus config: admin surfaces with no
        // composer equivalent.
        (Method::POST, "/api/sessions/x/acp/spawn", "{}"),
        (Method::DELETE, "/api/sessions/x/acp", ""),
        (
            Method::POST,
            "/api/sessions/x/acp/mode",
            r#"{"mode_id":"plan"}"#,
        ),
        // Project, MCP, and plugin admin. A plugin install runs arbitrary code
        // from a client-supplied source, so it must never be reachable.
        (Method::POST, "/api/projects", r#"{"path":"/tmp"}"#),
        (
            Method::POST,
            "/api/mcp/servers/x/keep",
            r#"{"agent":"claude"}"#,
        ),
        (
            Method::POST,
            "/api/mcp/servers/x/drop",
            r#"{"agent":"claude"}"#,
        ),
        (Method::POST, "/api/skills", r#"{"directory":"review"}"#),
        (
            Method::PUT,
            "/api/skills/review",
            r#"{"content":"---\nname: review\ndescription: d\n---\n"}"#,
        ),
        (Method::DELETE, "/api/skills/review", ""),
        (Method::POST, "/api/skills/claude-user/review/adopt", "{}"),
        (Method::POST, "/api/skills/sync", "{}"),
        (
            Method::POST,
            "/api/plugins/install",
            r#"{"source":"gh:owner/repo","expected_fingerprint":"x"}"#,
        ),
        (
            Method::POST,
            "/api/plugins/x/enabled",
            r#"{"enabled":true}"#,
        ),
        // Session lifecycle on a foreign or unknown target. With no structured
        // session seeded, an enumerated / crafted id resolves to a
        // non-structured-or-unknown target, so a pre-existing plain/terminal
        // session cannot be respawned (re-running its stored command_override)
        // or destroyed.
        (Method::POST, "/api/sessions/foreign/ensure", ""),
        (Method::POST, "/api/sessions/foreign/start", ""),
        (Method::POST, "/api/sessions/foreign/stop", ""),
        (Method::DELETE, "/api/sessions/foreign", "{}"),
        // G3: workspace-ordering is a PUT carrying only a read_only guard and no
        // per-handler CityHall check. The default-deny middleware must still
        // refuse it, proving the boundary is method- and prefix-uniform.
        (Method::PUT, "/api/workspace-ordering", "{}"),
        // The profile-settings PATCH stays open for the curated trash toggles,
        // but an uncurated leaf must be refused before it reaches the
        // merge/write.
        (
            Method::PATCH,
            "/api/profiles/default/settings",
            r#"{"session":{"yolo_mode":true}}"#,
        ),
    ];
    for (method, uri, body) in cases {
        assert_cityhall_blocked(method.clone(), uri, Body::from(*body)).await;
    }
}

// F1: delete_workspace tears down EVERY id, so a workspace whose owner is a
// legit structured session but whose sibling is a foreign plain session must be
// refused. Seeding a real structured owner + plain sibling locks the
// owner-vs-sibling discrimination: an owner-only gate would let this through
// (the owner is structured), so this test fails on a revert to `first()`.
#[tokio::test]
async fn delete_workspace_with_structured_owner_and_foreign_sibling_is_blocked() {
    let state =
        build_test_app_state_cityhall(vec![structured_session("own"), plain_session("foreign")]);
    let app = build_router_for_test(state);
    let resp = app
        .oneshot(request(
            Method::DELETE,
            "/api/workspaces",
            Body::from(r#"{"session_ids":["own","foreign"]}"#),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&bytes).contains("cityhall_mode"),
        "a foreign plain sibling must trip the CityHall gate"
    );
}

// The discrimination's other side: a workspace of only structured sessions the
// mode owns clears the CityHall gate (it may fail later for unrelated reasons in
// the test harness, but not with the cityhall_mode 403).
#[tokio::test]
async fn delete_workspace_all_structured_is_not_cityhall_blocked() {
    let state =
        build_test_app_state_cityhall(vec![structured_session("own"), structured_session("sib")]);
    let app = build_router_for_test(state);
    let resp = app
        .oneshot(request(
            Method::DELETE,
            "/api/workspaces",
            Body::from(r#"{"session_ids":["own","sib"]}"#),
        ))
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&bytes).contains("cityhall_mode"),
        "an all-structured workspace must not be refused by the CityHall gate"
    );
}

// Positive boundary check: an ALLOW-listed mutating route stays reachable in
// CityHall (the `cityhall_gate` lets it through to the handler), so an
// accidental deletion of its allow entry is caught by a direct assertion, not
// only the classification audit.
#[tokio::test]
async fn allowlisted_route_stays_reachable() {
    let state = build_test_app_state_cityhall(Vec::new());
    let app = build_router_for_test(state);
    let resp = app
        .oneshot(request(
            Method::POST,
            "/api/telemetry/consent",
            Body::from(r#"{"consent":false}"#),
        ))
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&bytes).contains("cityhall_mode"),
        "an allowlisted route must not be refused by the CityHall gate (got: {})",
        String::from_utf8_lossy(&bytes)
    );
}
