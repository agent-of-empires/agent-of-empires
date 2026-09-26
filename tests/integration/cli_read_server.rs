//! End-to-end coverage for the read-only runtime read: a real `axum` listener
//! serving the real router, driven by the real client.
//!
//! The client is not mocked here, so this is the only place the whole path is
//! observed at once: bearer authentication, the WebSocket upgrade, the one
//! Hello plus one Snapshot, the clean close, and the exit code the CLI returns.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use agent_of_empires::cli::runtime_read::{
    attempt, ReadOutcome, ReadRequestSource, ScopedCommand, ScopedRead,
};
use agent_of_empires::server::test_support::{
    build_router_for_test, build_test_app_state_cityhall, build_test_app_state_with_policy,
};
use agent_of_empires::server::AppState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const TOKEN: &str = "server-token-valid";

/// Point the daemon's app dir at an empty temporary XDG base, so the snapshot a
/// read observes is built from nothing rather than the host's real profiles.
struct TempAppDir {
    _dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

impl TempAppDir {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp app dir");
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        Self {
            _dir: dir,
            previous,
        }
    }
}

impl Drop for TempAppDir {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}

fn hosts() -> Vec<String> {
    ["127.0.0.1", "localhost", "::1"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Serve the router on an ephemeral loopback port until the test ends.
async fn serve(state: Arc<AppState>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("local addr");
    let app = build_router_for_test(state);
    tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    address
}

fn read_source(address: SocketAddr, token: Option<&str>) -> ReadRequestSource {
    ReadRequestSource {
        explicit_url: Some(format!("http://{address}")),
        env_url: None,
        token: token.map(OsString::from),
        explicit_profile: None,
        env_profile: None,
    }
}

/// Every read here names its endpoint, so a daemon always answers and the
/// local fallback never comes into play.
async fn execute(command: ScopedCommand<'_>, source: &ReadRequestSource) -> ReadOutcome {
    match attempt(command, source).await {
        ScopedRead::Answered(outcome) => outcome,
        ScopedRead::NoLocalPublication => panic!("a named endpoint must answer"),
    }
}

/// The daemon is reached, the two application frames are exchanged, and the
/// command renders.
#[tokio::test]
#[serial_test::serial]
async fn an_authenticated_read_renders_from_the_live_server() {
    let _home = TempAppDir::new();
    let state =
        build_test_app_state_with_policy(Vec::new(), hosts(), Vec::new(), Some(TOKEN.to_string()));
    let address = serve(state).await;

    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        execute(ScopedCommand::Profile, &read_source(address, Some(TOKEN))),
    )
    .await
    .expect("the read completes inside the exchange budget");

    assert_eq!(outcome.exit, 0, "stderr: {:?}", outcome.stderr);
    assert_eq!(outcome.stderr, None);
    assert_eq!(
        outcome.stdout.as_deref(),
        Some("No profiles found.\nRun 'aoe' to create the first profile automatically.\n")
    );
}

/// A bearer the daemon does not know is refused before the upgrade, and the
/// client reports it as unauthorized rather than a transport failure.
#[tokio::test]
#[serial_test::parallel]
async fn an_unknown_bearer_is_unauthorized() {
    let state =
        build_test_app_state_with_policy(Vec::new(), hosts(), Vec::new(), Some(TOKEN.to_string()));
    let address = serve(state).await;

    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        execute(
            ScopedCommand::Profile,
            &read_source(address, Some("client-token-invalid")),
        ),
    )
    .await
    .expect("the refusal arrives inside the establishment budget");

    assert_eq!(outcome.exit, 4);
    assert_eq!(outcome.stdout, None);
    assert_eq!(
        outcome.stderr.as_deref(),
        Some("daemon read: unauthorized\n")
    );
}

/// The CityHall lockdown answers the route directly, and the client folds that
/// refusal into the same unauthorized classification.
#[tokio::test]
#[serial_test::parallel]
async fn a_locked_down_daemon_is_unauthorized() {
    let state = build_test_app_state_cityhall(Vec::new());
    let address = serve(state).await;

    let outcome = tokio::time::timeout(
        Duration::from_secs(20),
        execute(ScopedCommand::Profile, &read_source(address, Some(TOKEN))),
    )
    .await
    .expect("the refusal arrives inside the establishment budget");

    assert_eq!(outcome.exit, 4);
    assert_eq!(
        outcome.stderr.as_deref(),
        Some("daemon read: unauthorized\n")
    );
}

/// Without any credential the client never opens a socket: a missing token is a
/// pre-transport failure, not an unauthorized one.
#[tokio::test]
#[serial_test::parallel]
async fn a_missing_token_never_reaches_the_daemon() {
    let state =
        build_test_app_state_with_policy(Vec::new(), hosts(), Vec::new(), Some(TOKEN.to_string()));
    let address = serve(state).await;

    let outcome: ReadOutcome = execute(ScopedCommand::Profile, &read_source(address, None)).await;
    assert_eq!(outcome.exit, 2);
    assert_eq!(outcome.stdout, None);
    assert_eq!(
        outcome.stderr.as_deref(),
        Some("daemon read: invalid_token\n")
    );
}

/// The route answers a raw, unauthenticated upgrade request with a 401 and no
/// WebSocket frames: the gate is the router's, ahead of the handler.
#[tokio::test]
#[serial_test::parallel]
async fn the_route_refuses_an_unauthenticated_upgrade_with_401() {
    let state =
        build_test_app_state_with_policy(Vec::new(), hosts(), Vec::new(), Some(TOKEN.to_string()));
    let address = serve(state).await;

    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    stream
        .write_all(
            b"GET /api/runtime/ws HTTP/1.1\r\n\
              Host: 127.0.0.1\r\n\
              Upgrade: websocket\r\n\
              Connection: Upgrade\r\n\
              Sec-WebSocket-Key: AAECAwQFBgcICQoLDA0ODw==\r\n\
              Sec-WebSocket-Version: 13\r\n\r\n",
        )
        .await
        .expect("write request");
    let mut head = [0u8; 4096];
    let read = stream.read(&mut head).await.expect("read response");
    let response = String::from_utf8_lossy(&head[..read]).into_owned();

    assert!(
        response.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "unexpected response: {response}"
    );
    assert!(
        !response.contains("101 Switching Protocols"),
        "an unauthenticated caller must not be upgraded: {response}"
    );
}

/// A valid browser cookie is not a runtime credential. The daemon-wide auth
/// middleware accepts cookies for the dashboard, but this read-only route is
/// deliberately narrower and requires one Authorization: Bearer header.
#[tokio::test]
#[serial_test::parallel]
async fn the_route_rejects_a_valid_dashboard_cookie() {
    let state =
        build_test_app_state_with_policy(Vec::new(), hosts(), Vec::new(), Some(TOKEN.to_string()));
    let address = serve(state).await;
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect");
    stream
        .write_all(
            b"GET /api/runtime/ws HTTP/1.1\r\n\
              Host: 127.0.0.1\r\n\
              Upgrade: websocket\r\n\
              Connection: Upgrade\r\n\
              Sec-WebSocket-Key: AAECAwQFBgcICQoLDA0ODw==\r\n\
              Sec-WebSocket-Version: 13\r\n\
              Cookie: aoe_token=server-token-valid\r\n\r\n",
        )
        .await
        .expect("write request");
    let mut head = [0u8; 4096];
    let read = stream.read(&mut head).await.expect("read response");
    let response = String::from_utf8_lossy(&head[..read]).into_owned();
    assert!(
        response.starts_with("HTTP/1.1 401 Unauthorized\r\n"),
        "unexpected response: {response}"
    );
    assert!(
        !response.contains("101 Switching Protocols"),
        "cookie must not upgrade: {response}"
    );
}
