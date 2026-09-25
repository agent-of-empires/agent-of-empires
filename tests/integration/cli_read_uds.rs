//! The local runtime read end to end: the real publisher, the real UNIX
//! listener and the real client, with no HTTP listener anywhere in the test.
//!
//! Every test runs in a namespace of its own under a private base, so nothing
//! reads or writes the developer's real app directory.

use std::time::Duration;

use agent_of_empires::cli::runtime_read::{execute, ReadOutcome, ReadRequestSource, ScopedCommand};
use agent_of_empires::server::test_support::{
    build_test_app_state_with_policy, RuntimeUdsTestServer,
};

/// No explicit endpoint: the client must find the daemon on its own socket.
fn local_source() -> ReadRequestSource {
    ReadRequestSource {
        explicit_url: None,
        env_url: None,
        token: None,
        explicit_profile: None,
        env_profile: None,
    }
}

async fn read(command: ScopedCommand<'_>) -> ReadOutcome {
    tokio::time::timeout(Duration::from_secs(20), execute(command, &local_source()))
        .await
        .expect("the read completes inside the establishment and exchange budgets")
}

/// A live daemon on its own socket serves the read, and the rendered output
/// comes from the snapshot that daemon published. There is no TCP listener in
/// this test, so success can only have come over the UNIX socket.
#[tokio::test]
#[serial_test::serial]
async fn a_live_daemon_socket_serves_the_read() {
    let state = build_test_app_state_with_policy(Vec::new(), Vec::new(), Vec::new(), None);
    let server = RuntimeUdsTestServer::start(state.clone())
        .unwrap_or_else(|reason| panic!("the local read must be publishable: {reason}"));

    let outcome = read(ScopedCommand::Profile).await;

    assert_eq!(outcome.exit, 0, "stderr: {:?}", outcome.stderr);
    assert_eq!(outcome.stderr, None);
    assert_eq!(
        outcome.stdout.as_deref(),
        Some("No profiles found.\nRun 'aoe' to create the first profile automatically.\n")
    );
    drop(server);
}

/// The same publication, read twice: one snapshot per connection, both
/// complete, and the second read is not a replay of a cached frame.
#[tokio::test]
#[serial_test::serial]
async fn every_connection_gets_its_own_complete_exchange() {
    let state = build_test_app_state_with_policy(Vec::new(), Vec::new(), Vec::new(), None);
    let server = RuntimeUdsTestServer::start(state.clone())
        .unwrap_or_else(|reason| panic!("the local read must be publishable: {reason}"));

    for attempt in 0..3 {
        let outcome = read(ScopedCommand::Profile).await;
        assert_eq!(
            outcome.exit, 0,
            "attempt {attempt} failed: {:?}",
            outcome.stderr
        );
    }
    state.shutdown.cancel();
    server.join().await;
}

/// Shutdown retracts the artifacts, so a later read fails closed at admission
/// instead of reaching a socket that no daemon is serving.
#[tokio::test]
#[serial_test::serial]
async fn shutdown_retracts_the_socket_and_closes_admission() {
    let state = build_test_app_state_with_policy(Vec::new(), Vec::new(), Vec::new(), None);
    let server = RuntimeUdsTestServer::start(state.clone())
        .unwrap_or_else(|reason| panic!("the local read must be publishable: {reason}"));
    let app_dir = server.app_dir();

    assert_eq!(read(ScopedCommand::Profile).await.exit, 0);
    for name in [
        "runtime.sock",
        "runtime.prebind.json",
        "runtime.postbind.json",
    ] {
        assert!(
            app_dir.join(name).exists(),
            "{name} must be published while live"
        );
    }

    state.shutdown.cancel();
    server.join().await;

    for name in [
        "runtime.sock",
        "runtime.prebind.json",
        "runtime.postbind.json",
    ] {
        assert!(!app_dir.join(name).exists(), "{name} survived shutdown");
    }
    let outcome = read(ScopedCommand::Profile).await;
    assert_ne!(
        outcome.exit, 0,
        "a retracted publication must not serve a read"
    );
    assert_eq!(outcome.stdout, None);
}
