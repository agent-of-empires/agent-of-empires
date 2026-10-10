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
    _env: agent_of_empires::server::test_support::RuntimeEnvGuard,
}

impl TempAppDir {
    fn new() -> Self {
        let mut env = agent_of_empires::server::test_support::RuntimeEnvGuard::read_lock();
        let dir = tempfile::tempdir().expect("temp app dir");
        env.bind(dir.path());
        Self {
            _dir: dir,
            _env: env,
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
    agent_of_empires::server::test_support::seed_instances_on_disk_for_test(
        "main",
        state.instances.read().await.clone(),
    );
    agent_of_empires::server::test_support::accept_runtime_read_cache_for_test(&state).await;

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
    agent_of_empires::server::test_support::accept_runtime_read_cache_for_test(&state).await;

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

/// Empty environment selections preserve the local profile and transport semantics.
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
    agent_of_empires::server::test_support::seed_instances_on_disk_for_test(
        "main",
        state.instances.read().await.clone(),
    );
    agent_of_empires::server::test_support::accept_runtime_read_cache_for_test(&state).await;

    // Whitespace-only values name profiles; only empty variables are unset.
    for (label, env_url, env_profile) in [
        ("absent", None, None),
        ("empty", Some(OsString::from("")), Some(OsString::from(""))),
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

/// Retraction returns discovery to local takeover, never an unserved socket.
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

/// Prefix symlinks are followed and their resolved directories validated.
#[tokio::test]
#[serial_test::serial]
async fn a_publication_under_a_symlinked_config_home_is_served() {
    let base = tempfile::tempdir().expect("temp base");
    let real = base.path().join("real");
    std::fs::create_dir_all(&real).expect("real config home");
    let link = base.path().join("link");
    std::os::unix::fs::symlink(&real, &link).expect("config home symlink");

    let _env = agent_of_empires::server::test_support::RuntimeEnvGuard::set(&link);

    let state = build_test_app_state_with_policy(Vec::new(), Vec::new(), Vec::new(), None);
    let server = RuntimeUdsTestServer::start_in(&link, state.clone())
        .unwrap_or_else(|reason| panic!("a symlinked config home is publishable: {reason}"));
    // An empty profile lets status succeed without a missing-profile refusal.
    agent_of_empires::session::create_profile("main").expect("create fixture profile");
    agent_of_empires::server::test_support::accept_runtime_read_cache_for_test(&state).await;

    // Publication is anchored to the resolved config directory.
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
    state.shutdown.cancel();
    server.join().await;
}

#[tokio::test]
#[serial_test::serial]
async fn retained_markers_without_a_live_owner_are_absent() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = TempAppDir::new();
    let state = build_test_app_state_with_policy(Vec::new(), Vec::new(), Vec::new(), None);
    let server = RuntimeUdsTestServer::start_in(fixture._dir.path(), state.clone()).unwrap();
    let app = server.app_dir();
    let markers = ["runtime.prebind.json", "runtime.postbind.json"].map(|name| {
        let mut marker: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join(name)).unwrap()).unwrap();
        let start = marker["process_start_identity"].as_str().unwrap();
        let (prefix, ticks) = start.rsplit_once(':').unwrap();
        marker["process_start_identity"] = serde_json::json!(format!(
            "{prefix}:{}",
            ticks.parse::<u64>().unwrap().checked_add(1).unwrap()
        ));
        (name, marker)
    });
    state.shutdown.cancel();
    server.join().await;
    for (name, marker) in markers {
        let path = app.join(name);
        std::fs::write(&path, serde_json::to_vec(&marker).unwrap()).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let read = tokio::time::timeout(
        Duration::from_secs(2),
        attempt_read(ScopedCommand::Profile, &local_source()),
    )
    .await
    .expect("retained dead markers are absence without the establishment wait");
    assert!(
        matches!(read, ScopedRead::NoLocalPublication(_)),
        "retained markers with no owner must hand back to the local store"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn an_unpublished_writable_namespace_falls_back_but_runtime_artifacts_refuse() {
    use std::os::unix::fs::PermissionsExt;

    for (base_mode, app_mode) in [(0o775, None), (0o775, Some(0o700)), (0o700, Some(0o775))] {
        let artifacts: &[Option<&str>] = if app_mode.is_some() {
            &[
                None,
                Some("runtime.prebind.json"),
                Some("runtime.postbind.json"),
                Some("runtime.sock"),
                Some("lifetime.lock"),
                Some("publisher.lock"),
                Some("runtime.prebind.json.tmp.partial"),
                Some("runtime.unrecognised"),
            ]
        } else {
            &[None]
        };
        for &artifact in artifacts {
            let namespace = TempAppDir::new();
            let app = namespace
                ._dir
                .path()
                .join(agent_of_empires::session::APP_DIR_NAME_XDG);
            std::fs::set_permissions(
                namespace._dir.path(),
                std::fs::Permissions::from_mode(base_mode),
            )
            .unwrap();
            if let Some(mode) = app_mode {
                std::fs::create_dir(&app).unwrap();
                std::fs::set_permissions(&app, std::fs::Permissions::from_mode(mode)).unwrap();
            }
            if let Some(name) = artifact {
                std::fs::write(app.join(name), b"untrusted runtime artifact").unwrap();
            }
            let result = attempt_read(ScopedCommand::Profile, &local_source()).await;
            match (artifact, result) {
                (None, ScopedRead::NoLocalPublication(None)) => {}
                (Some(_), ScopedRead::Answered(outcome)) => {
                    assert_eq!(outcome.exit, 2);
                    assert_eq!(outcome.stdout, None);
                    assert!(outcome.stderr.unwrap().contains("refused to read"));
                }
                (artifact, _) => {
                    panic!("unexpected admission for {base_mode:o}/{app_mode:?}, {artifact:?}")
                }
            }
        }
    }
}
