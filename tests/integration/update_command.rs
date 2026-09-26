//! Smoke test for `aoe update --check`.
//!
//! These spawn a small axum fixture server that serves canned GitHub
//! releases JSON, then run the `aoe` binary as a subprocess pointed at
//! the fixture via `AOE_UPDATE_API_BASE`. Hermetic — never touches the
//! real GitHub API, so they pass on rate-limited CI runners.
//!
//! The fixture lives on its own thread with a dedicated tokio runtime
//! so the outer `#[test]` (sync) can drive it without async plumbing.

use std::process::Command;
use std::sync::mpsc;
use std::thread;

fn aoe_binary() -> &'static str {
    env!("CARGO_BIN_EXE_aoe")
}

/// Start an axum server that returns canned JSON for the two GitHub
/// release endpoints aoe queries. Returns the base URL plus a shutdown
/// channel; dropping the sender doesn't matter (the thread exits when
/// the process does), the channel is here so a future test can stop
/// the server early if needed.
struct FixtureServer {
    base_url: String,
    _shutdown: mpsc::Sender<()>,
}

fn spawn_fixture(latest_version: &str) -> FixtureServer {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let version = latest_version.to_string();

    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fixture runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind fixture port");
            let port = listener.local_addr().expect("local_addr").port();
            port_tx.send(port).expect("port channel");

            let release_json = move || {
                serde_json::json!({
                    "tag_name": format!("v{version}"),
                    "body": "fixture release notes",
                    "published_at": "2026-04-29T00:00:00Z",
                })
                .to_string()
            };
            let releases_json = release_json.clone();

            let app = axum::Router::new()
                .route(
                    "/repos/agent-of-empires/agent-of-empires/releases/latest",
                    axum::routing::get(move || {
                        let body = release_json();
                        async move {
                            (
                                [(axum::http::header::CONTENT_TYPE, "application/json")],
                                body,
                            )
                        }
                    }),
                )
                .route(
                    "/repos/agent-of-empires/agent-of-empires/releases",
                    axum::routing::get(move || {
                        let body = format!("[{}]", releases_json());
                        async move {
                            (
                                [(axum::http::header::CONTENT_TYPE, "application/json")],
                                body,
                            )
                        }
                    }),
                );

            let serve = axum::serve(listener, app);
            tokio::select! {
                _ = serve => {}
                _ = tokio::task::spawn_blocking(move || {
                    let _ = shutdown_rx.recv();
                }) => {}
            }
        });
    });

    let port = port_rx.recv().expect("port from fixture");
    FixtureServer {
        base_url: format!("http://127.0.0.1:{port}"),
        _shutdown: shutdown_tx,
    }
}

/// `--check` prints the current and latest versions and whether an update is
/// available, for a newer release and for the running version.
#[test]
#[serial_test::parallel]
fn update_check_reports_availability_and_exits_zero() {
    for (latest, available) in [("999.0.0", true), (env!("CARGO_PKG_VERSION"), false)] {
        let fixture = spawn_fixture(latest);
        let tmp = tempfile::TempDir::new().unwrap();

        let output = Command::new(aoe_binary())
            .args(["update", "--check"])
            .env("HOME", tmp.path())
            .env("XDG_CONFIG_HOME", tmp.path())
            .env("AOE_UPDATE_API_BASE", &fixture.base_url)
            .output()
            .expect("running aoe update --check");

        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("current:"), "stdout was: {stdout}");
        assert!(
            stdout.contains(&format!("latest:  {latest}")),
            "stdout was: {stdout}"
        );
        assert!(
            stdout.contains(&format!("available: {available}")),
            "stdout was: {stdout}"
        );
    }
}
