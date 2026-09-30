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
        ScopedRead::NoLocalPublication(notice) => {
            panic!("a live daemon publication must answer, not a local take-over ({notice:?})")
        }
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
        matches!(read, ScopedRead::NoLocalPublication(_)),
        "an app dir no daemon published into must not read as an error"
    );
}

/// An empty selection in the environment is one decision, in both variables,
/// and a live daemon must answer it the way the local
/// command path does: `aoe list` reads the default profile and prints the
/// table. Before, the empty profile came back as `profile_missing` (exit 4)
/// from the daemon while the very same command succeeded with no daemon, and
/// an empty `AOE_DAEMON_URL` stranded every read instead of selecting the
/// local transport.
#[tokio::test]
#[serial_test::serial]
async fn an_empty_environment_selection_is_answered_not_refused() {
    let mut instance = Instance::new("s1", "/repo");
    instance.source_profile = "main".into();
    instance.title = "Session".into();
    instance.tool = "claude".into();
    instance.id = "s1".into();
    let state = build_test_app_state_with_policy(vec![instance], Vec::new(), Vec::new(), None);
    let server = RuntimeUdsTestServer::start(state.clone())
        .unwrap_or_else(|reason| panic!("the local read must be publishable: {reason}"));
    agent_of_empires::session::create_profile("main").expect("create fixture profile");

    for (label, env_url, env_profile) in [
        ("absent", None, None),
        ("empty", Some(OsString::from("")), Some(OsString::from(""))),
        // A blank variable is the default profile, because the local path
        // trims a variable before it resolves one. This row and the explicit
        // flag case below are the two halves of that: the same three spaces
        // are unset in one input and a profile name in the other.
        (
            "whitespace",
            Some(OsString::from("   ")),
            Some(OsString::from("  ")),
        ),
    ] {
        let source = ReadRequestSource {
            explicit_url: None,
            env_url,
            token: None,
            explicit_profile: None,
            env_profile,
        };
        let cli = Cli::try_parse_from(["aoe", "list"]).expect("list parses");
        let command = classify(cli.command.as_ref()).expect("list is a scoped read");
        let outcome = match attempt_read(command, &source).await {
            ScopedRead::Answered(outcome) => outcome,
            ScopedRead::NoLocalPublication(notice) => {
                panic!("{label}: a live daemon must answer, not a local take-over ({notice:?})")
            }
        };
        assert_eq!(outcome.exit, 0, "{label}: {:?}", outcome.stderr);
        assert!(
            outcome
                .stdout
                .as_deref()
                .is_some_and(|out| out.contains("s1")),
            "{label}: the default profile's session must be listed: {:?}",
            outcome.stdout
        );
    }
    // The same three spaces as an explicit flag are a profile *name* on both
    // halves, because `resolve_existing_profile` does not trim what it is
    // given. The local command refuses it by name, so the served half must
    // too rather than quietly reading the default.
    let cli = Cli::try_parse_from(["aoe", "list", "-p", "   "]).expect("list parses");
    let command = classify(cli.command.as_ref()).expect("list is a scoped read");
    let source = ReadRequestSource {
        explicit_url: None,
        env_url: None,
        token: None,
        explicit_profile: Some("   ".into()),
        env_profile: None,
    };
    let outcome = match attempt_read(command, &source).await {
        ScopedRead::Answered(outcome) => outcome,
        ScopedRead::NoLocalPublication(notice) => {
            panic!("a live daemon must answer, not a local take-over ({notice:?})")
        }
    };
    assert_eq!(outcome.exit, 1, "{:?}", outcome.stderr);
    assert!(
        outcome
            .stderr
            .as_deref()
            .unwrap_or_default()
            .contains("Profile '   ' does not exist"),
        "the refusal names the profile the user typed: {:?}",
        outcome.stderr
    );
    assert!(
        outcome.stdout.as_deref().unwrap_or_default().is_empty(),
        "a refusal prints no rows: {:?}",
        outcome.stdout
    );

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
    let read = attempt_read(ScopedCommand::Profile, &local_source()).await;
    assert!(
        matches!(read, ScopedRead::NoLocalPublication(_)),
        "a retracted publication must not serve a read"
    );
}

/// A daemon that republishes while the client is admitting it leaves the
/// markers describing a process that is no longer the one holding the socket.
/// That is `marker_identity`, true for a state that lasts microseconds, so
/// the client re-admits inside the establishment budget instead of refusing a
/// read that a daemon restart or a fresh publication raced.
#[tokio::test]
#[serial_test::serial]
async fn a_republication_raced_mid_admission_still_serves_the_read() {
    let state = build_test_app_state_with_policy(Vec::new(), Vec::new(), Vec::new(), None);
    let server = RuntimeUdsTestServer::start(state.clone())
        .unwrap_or_else(|reason| panic!("the local read must be publishable: {reason}"));
    let marker = server.app_dir().join("runtime.postbind.json");
    let published = std::fs::read(&marker).expect("the live postbind marker");

    // A marker naming a pid that no longer exists: what the client reads in the
    // window between a republication's unlink and its rename.
    let mut stale: serde_json::Value =
        serde_json::from_slice(&published).expect("the marker is json");
    stale["pid"] = serde_json::json!(4_194_303);
    std::fs::write(&marker, serde_json::to_vec(&stale).expect("stale marker")).expect("rewrite");

    let restore = {
        let marker = marker.clone();
        let published = published.clone();
        tokio::spawn(async move {
            std::fs::write(&marker, published).expect("the republication lands");
        })
    };
    let outcome = read(ScopedCommand::Profile).await;
    restore.await.expect("the restore task joins");

    assert_eq!(
        outcome.exit, 0,
        "a raced republication must not fail the read: {:?}",
        outcome.stderr
    );
    assert!(outcome.stdout.is_some());
    state.shutdown.cancel();
    server.join().await;
}

/// A home reached through a symlink is an ordinary setup, `/tmp` → `/private/tmp`
/// on macOS and a linked `$HOME` on Linux, and the publisher used to refuse it
/// outright, so a daemon under a symlinked `XDG_CONFIG_HOME` published nothing
/// and every read fell back to the local store. The walk follows a prefix
/// symlink and verifies what it resolves to by descriptor, so this is served.
#[tokio::test]
#[serial_test::serial]
async fn a_publication_under_a_symlinked_config_home_is_served() {
    let base = tempfile::tempdir().expect("temp base");
    let real = base.path().join("real");
    std::fs::create_dir_all(&real).expect("real config home");
    let link = base.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("config home symlink");

    let previous = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", &link);
    let _restore = EnvRestore(previous);

    let state = build_test_app_state_with_policy(Vec::new(), Vec::new(), Vec::new(), None);
    let server = RuntimeUdsTestServer::start_in(&link, state.clone())
        .unwrap_or_else(|reason| panic!("a symlinked config home is publishable: {reason}"));
    // A profile that holds nothing: what is under test is which transport
    // answers, not the missing-profile refusal the renderer would otherwise
    // return for an empty state.
    agent_of_empires::session::create_profile("main").expect("create fixture profile");

    // All four artifacts exist: a publisher that walked the chain and then
    // published by name would have produced the same two marker files, so the
    // count is the difference between "published" and "published once".
    for name in [
        "lifetime.lock",
        "runtime.prebind.json",
        "runtime.postbind.json",
        "runtime.sock",
    ] {
        assert!(
            server.app_dir().join(name).exists(),
            "{name} must be published under a symlinked config home"
        );
    }

    let cli = Cli::try_parse_from(["aoe", "status"]).expect("status parses");
    let command = classify(cli.command.as_ref()).expect("status is a scoped read");
    let outcome = read(command).await;
    assert_eq!(
        outcome.exit, 0,
        "the read must be served: {:?}",
        outcome.stderr
    );
    assert!(
        outcome.stdout.as_deref().is_some_and(|out| !out.is_empty()),
        "a served read prints something"
    );
    state.shutdown.cancel();
    server.join().await;
}

/// A daemon that was killed outright leaves its three artifacts behind, and the
/// client has to say so at once. Reporting that as a retryable identity fault
/// would spend the whole 15-second establishment budget before producing the
/// same answer, so the read is handed straight back to the local command path.
#[tokio::test]
#[serial_test::serial]
async fn a_dead_publisher_hands_the_read_back_at_once() {
    let state = build_test_app_state_with_policy(Vec::new(), Vec::new(), Vec::new(), None);
    let server = RuntimeUdsTestServer::start(state.clone())
        .unwrap_or_else(|reason| panic!("the local read must be publishable: {reason}"));

    // Exactly what a `kill -9` leaves: the artifacts on disk, the recorded pid
    // of a process that is provably not running, and no listener answering.
    for name in ["runtime.prebind.json", "runtime.postbind.json"] {
        let path = server.app_dir().join(name);
        let published = std::fs::read(&path).expect("the live marker");
        let mut dead: serde_json::Value =
            serde_json::from_slice(&published).expect("the marker is json");
        dead["pid"] = serde_json::json!(4_194_302);
        dead["process_start_identity"] = serde_json::json!(format!(
            "linux:v1:{}:1",
            std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .expect("boot id")
                .trim()
        ));
        std::fs::write(&path, serde_json::to_vec(&dead).expect("dead marker")).expect("rewrite");
    }

    let started = std::time::Instant::now();
    let read = attempt_read(ScopedCommand::Profile, &local_source()).await;
    let elapsed = started.elapsed();
    assert!(
        matches!(read, ScopedRead::NoLocalPublication(_)),
        "a dead publisher is an absence, not a refusal"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the read was handed back in {elapsed:?}, which is the whole establishment budget again"
    );
    state.shutdown.cancel();
    server.join().await;
}

/// Restores `XDG_CONFIG_HOME` when the test ends, however it ends.
struct EnvRestore(Option<OsString>);

impl Drop for EnvRestore {
    fn drop(&mut self) {
        match &self.0 {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}
