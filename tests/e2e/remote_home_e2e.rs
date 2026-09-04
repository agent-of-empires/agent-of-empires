//! Full-binary coverage for the remote daemon session picker.

use crate::harness::TuiTestHarness;
use axum::{routing::get, Json, Router};
use serial_test::parallel;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[parallel]
async fn remote_home_accepts_old_daemon_rows_and_keeps_structured_title_order() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route(
            "/api/sessions",
            get(|| async {
                Json(serde_json::json!({
                    "sessions": [
                        {
                            "id": "beta",
                            "title": "Beta review",
                            "project_path": "/tmp/beta",
                            "status": "Idle",
                            "view": "structured",
                            "context_resume": {
                                "state": "indeterminate",
                                "reason": "agent_handshake_required"
                            }
                        },
                        {
                            "id": "terminal",
                            "title": "Hidden terminal",
                            "project_path": "/tmp/terminal"
                        },
                        {
                            "id": "alpha",
                            "title": "Alpha review",
                            "project_path": "/tmp/alpha",
                            "view": "structured"
                        }
                    ],
                    "workspace_ordering": []
                }))
            }),
        )
        .route(
            "/api/plugins/ui-state",
            get(|| async { Json(serde_json::json!({ "entries": [], "notifications": [] })) }),
        );
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let mut harness = TuiTestHarness::new("remote_home_old_daemon");
    harness.set_env("AOE_DAEMON_URL", &format!("http://127.0.0.1:{port}"));
    harness.spawn_tui();
    harness.wait_for("Alpha review");
    harness.wait_for("Beta review");

    let screen = harness.capture_screen();
    assert!(!screen.contains("Hidden terminal"), "screen:\n{screen}");
    assert!(screen.contains("2 session(s)"), "screen:\n{screen}");
    assert!(
        screen.contains("ctx:?"),
        "screen:
{screen}"
    );
    assert!(
        screen.contains("ctx:check"),
        "screen:
{screen}"
    );
    assert!(
        screen.find("Alpha review") < screen.find("Beta review"),
        "screen:\n{screen}"
    );

    harness.send_keys("q");
    harness.wait_for_exit(std::time::Duration::from_secs(5));
    server.abort();
}
