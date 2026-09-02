//! Full-binary coverage for the remote daemon session picker.

use crate::harness::TuiTestHarness;
use axum::{http::HeaderMap, routing::get, Json, Router};
use serial_test::parallel;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[parallel]
async fn remote_home_negotiates_attach_and_keeps_structured_title_order() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let saw_capability = Arc::new(AtomicBool::new(false));
    let saw_capability_for_route = Arc::clone(&saw_capability);
    let app = Router::new()
        .route(
            "/api/sessions",
            get(move |headers: HeaderMap| {
                let saw_capability = Arc::clone(&saw_capability_for_route);
                async move {
                    saw_capability.store(
                        headers
                            .get("x-aoe-client-capabilities")
                            .is_some_and(|value| value == "acp_ws_v1"),
                        Ordering::SeqCst,
                    );
                    Json(serde_json::json!({
                        "sessions": [
                            {
                                "id": "beta",
                                "title": "Beta review",
                                "project_path": "/tmp/beta",
                                "status": "Idle",
                                "view": "structured",
                                "context_resume": { "state": "available" }
                            },
                            {
                                "id": "terminal",
                                "title": "Hidden terminal",
                                "project_path": "/tmp/terminal",
                                "status": "Idle",
                                "context_resume": { "state": "available" }
                            },
                            {
                                "id": "alpha",
                                "title": "Alpha review",
                                "project_path": "/tmp/alpha",
                                "status": "Idle",
                                "view": "structured",
                                "context_resume": {
                                    "state": "unavailable",
                                    "reason": "no_target"
                                }
                            }
                        ],
                        "workspace_ordering": [],
                        "session_attach": {
                            "alpha": { "state": "available", "transport": "acp_websocket_v1" },
                            "beta": { "state": "available", "transport": "acp_websocket_v1" },
                            "terminal": { "state": "unavailable", "reason": "client_missing_transport" }
                        }
                    }))
                }
            }),
        )
        .route(
            "/api/plugins/ui-state",
            get(|| async { Json(serde_json::json!({ "entries": [], "notifications": [] })) }),
        );
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let mut harness = TuiTestHarness::new("remote_home_capabilities");
    harness.set_env("AOE_DAEMON_URL", &format!("http://127.0.0.1:{port}"));
    harness.spawn_tui();
    harness.wait_for("Alpha review");
    harness.wait_for("Beta review");

    let screen = harness.capture_screen();
    assert!(saw_capability.load(Ordering::SeqCst));
    assert!(!screen.contains("Hidden terminal"), "screen:\n{screen}");
    assert!(screen.contains("2 session(s)"), "screen:\n{screen}");
    assert!(screen.contains("ctx:no"), "screen:\n{screen}");
    assert!(screen.contains("ctx:yes"), "screen:\n{screen}");
    assert!(
        screen.find("Alpha review") < screen.find("Beta review"),
        "screen:\n{screen}"
    );

    harness.send_keys("q");
    harness.wait_for_exit(std::time::Duration::from_secs(5));
    server.abort();
}
