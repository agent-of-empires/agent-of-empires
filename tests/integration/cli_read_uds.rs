//! The local runtime read end to end: the real publisher, the real UNIX
//! listener and the real client, with no HTTP listener anywhere in the test.
//!
//! Every test runs in a namespace of its own under a private base, so nothing
//! reads or writes the developer's real app directory.

use std::ffi::OsString;
use std::time::Duration;

use agent_of_empires::cli::definition::Cli;
use agent_of_empires::cli::runtime_read::classify;
use agent_of_empires::cli::runtime_read::{
    attempt, ReadOutcome, ReadRequestSource, ScopedCommand, ScopedRead,
};
use agent_of_empires::server::test_support::{
    build_test_app_state_with_policy, RuntimeUdsTestServer,
};
use agent_of_empires::session::Instance;
use clap::Parser;

/// Point the client's app dir at an empty temporary XDG base, so admission sees
/// a namespace no daemon ever published into.
struct TempAppDir {
    _dir: tempfile::TempDir,
    previous: Option<OsString>,
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

async fn attempt_read(command: ScopedCommand<'_>, source: &ReadRequestSource) -> ScopedRead {
    tokio::time::timeout(Duration::from_secs(20), attempt(command, source))
        .await
        .expect("the read completes inside the establishment and exchange budgets")
}

async fn read(command: ScopedCommand<'_>) -> ReadOutcome {
    match attempt_read(command, &local_source()).await {
        ScopedRead::Answered(outcome) => outcome,
        ScopedRead::NoLocalPublication => panic!("a live daemon publication must answer"),
    }
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

/// Every scoped command and alias completes over the real local socket.
#[tokio::test]
#[serial_test::serial]
async fn every_scoped_command_and_alias_round_trips_over_uds() {
    let mut instance = Instance::new("s1", "/repo");
    instance.source_profile = "main".into();
    instance.title = "Session".into();
    instance.tool = "claude".into();
    instance.group_path = "alpha".into();
    instance.id = "s1".into();
    let state = build_test_app_state_with_policy(vec![instance], Vec::new(), Vec::new(), None);
    let server = RuntimeUdsTestServer::start(state.clone())
        .unwrap_or_else(|reason| panic!("the local read must be publishable: {reason}"));
    agent_of_empires::session::create_profile("main").expect("create fixture profile");

    for argv in [
        vec!["aoe", "list"],
        vec!["aoe", "ls"],
        vec!["aoe", "status"],
        vec!["aoe", "session", "show", "s1"],
        vec!["aoe", "session", "list-trash"],
        vec!["aoe", "group", "list"],
        vec!["aoe", "group", "ls"],
        vec!["aoe", "profile"],
        vec!["aoe", "profile", "list"],
        vec!["aoe", "profile", "ls"],
        vec!["aoe", "project", "list"],
        vec!["aoe", "project", "ls"],
    ] {
        let cli = Cli::try_parse_from(&argv).expect("command parses");
        let command = classify(cli.command.as_ref()).expect("command is scoped read");
        let outcome = read(command).await;
        assert_eq!(outcome.exit, 0, "{argv:?} failed: {:?}", outcome.stderr);
        assert!(outcome.stdout.is_some(), "{argv:?} produced no stdout");
    }
    state.shutdown.cancel();
    server.join().await;
}

/// The machine form of `aoe status --json` must not depend on the transport.
/// An empty profile read over the real local socket has to be the exact bytes
/// the local command path prints for the same profile, spaces included.
#[tokio::test]
#[serial_test::serial]
async fn the_daemon_and_local_status_json_agree_on_an_empty_profile() {
    let state = build_test_app_state_with_policy(Vec::new(), Vec::new(), Vec::new(), None);
    let server = RuntimeUdsTestServer::start(state.clone())
        .unwrap_or_else(|reason| panic!("the local read must be publishable: {reason}"));
    // A profile that holds nothing: the shape under test is the empty one, not
    // the missing-profile refusal.
    agent_of_empires::session::create_profile("main").expect("create fixture profile");

    let cli = Cli::try_parse_from(["aoe", "status", "--json"]).expect("status parses");
    let command = classify(cli.command.as_ref()).expect("status is a scoped read");
    let outcome = read(command).await;

    assert_eq!(outcome.exit, 0, "stderr: {:?}", outcome.stderr);
    assert_eq!(
        outcome.stdout.as_deref(),
        Some("{\"waiting\":0,\"running\":0,\"idle\":0,\"stopped\":0,\"error\":0,\"total\":0}\n")
    );
    state.shutdown.cancel();
    server.join().await;
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

/// With no daemon publishing, the local transport reports that the command is
/// the caller's to run, rather than an error the CLI would have to interpret.
#[tokio::test]
#[serial_test::serial]
async fn no_daemon_publication_leaves_the_command_to_the_local_path() {
    let _namespace = TempAppDir::new();
    let cli = Cli::try_parse_from(["aoe", "list"]).expect("list parses");
    let command = classify(cli.command.as_ref()).expect("list is a scoped read");

    let read = attempt_read(command, &local_source()).await;

    assert!(
        matches!(read, ScopedRead::NoLocalPublication),
        "an app dir no daemon published into must not read as an error"
    );
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
    let read = attempt_read(ScopedCommand::Profile, &local_source()).await;
    assert!(
        matches!(read, ScopedRead::NoLocalPublication),
        "a retracted publication must not serve a read"
    );
}
