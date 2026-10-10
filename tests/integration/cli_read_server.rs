//! Real runtime router and client coverage.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::process::Command;
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

/// Isolate app directories from user profiles.
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
        ScopedRead::NoLocalPublication(notice) => {
            panic!("a named endpoint must answer, not a local take-over ({notice:?})")
        }
    }
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

#[tokio::test]
#[serial_test::parallel]
async fn missing_credentials_are_refused_by_the_daemon_policy() {
    let state =
        build_test_app_state_with_policy(Vec::new(), hosts(), Vec::new(), Some(TOKEN.to_string()));
    let address = serve(state).await;

    let outcome: ReadOutcome = execute(ScopedCommand::Profile, &read_source(address, None)).await;
    assert_eq!(outcome.exit, 4);
    assert_eq!(outcome.stdout, None);
    assert_eq!(
        outcome.stderr.as_deref(),
        Some("daemon read: unauthorized\n")
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

#[tokio::test]
#[serial_test::serial]
async fn the_route_shares_the_daemon_credential_gate() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let _app = TempAppDir::new();
    for (configured_token, cookie, bearer) in [
        (Some(TOKEN), Some(TOKEN), None),
        (Some(TOKEN), Some(TOKEN), Some("wrong-token")),
        (None, None, None),
    ] {
        let state = build_test_app_state_with_policy(
            Vec::new(),
            hosts(),
            Vec::new(),
            configured_token.map(str::to_owned),
        );
        let address = serve(state).await;
        let mut request = format!("ws://{address}/api/runtime/ws")
            .into_client_request()
            .unwrap();
        if let Some(cookie) = cookie {
            request
                .headers_mut()
                .insert("Cookie", format!("aoe_token={cookie}").parse().unwrap());
        }
        if let Some(bearer) = bearer {
            request
                .headers_mut()
                .insert("Authorization", format!("Bearer {bearer}").parse().unwrap());
        }
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("the credential accepted by daemon auth must admit the runtime read");
        let hello = socket.next().await.unwrap().unwrap().into_text().unwrap();
        let hello: serde_json::Value = serde_json::from_str(&hello).unwrap();
        assert_eq!(hello["data"]["protocol_version"], 4);
        assert_eq!(hello["data"]["local_owner"], false);
        assert_eq!(hello["data"]["owner"]["uid"], serde_json::Value::Null);
        socket.close(None).await.unwrap();
        if configured_token.is_none() {
            let outcome = execute(ScopedCommand::Profile, &read_source(address, None)).await;
            assert_eq!(outcome.exit, 0, "{:?}", outcome.stderr);
            assert_eq!(
                outcome.stdout.as_deref(),
                Some("No profiles found.\nRun 'aoe' to create the first profile automatically.\n")
            );
        }
    }
}

/// A served read still reports unknown configuration through the shared preflight.
#[tokio::test]
#[serial_test::serial]
async fn a_served_read_still_runs_the_shared_preflight() {
    use agent_of_empires::session::APP_DIR_NAME_XDG;

    let base = TempAppDir::new();
    let home = std::path::PathBuf::from(base._dir.path());
    let xdg = home.join(".config");
    let app_dir = xdg.join(APP_DIR_NAME_XDG);
    std::fs::create_dir_all(&app_dir).expect("app dir");
    std::fs::write(
        app_dir.join("config.toml"),
        // Disable update checks while exercising unknown-key diagnostics.
        "not_a_real_key = 3\n\n[updates]\nupdate_check_mode = \"off\"\n",
    )
    .expect("config");

    let state =
        build_test_app_state_with_policy(Vec::new(), hosts(), Vec::new(), Some(TOKEN.to_string()));
    let address = serve(state).await;

    // Keep the in-process HTTP task schedulable while the child runs.
    let served = tokio::task::spawn_blocking({
        let (home, xdg) = (home.clone(), xdg.clone());
        move || run_cli(&home, &xdg, address, &["profile"])
    })
    .await
    .expect("the command task joins");

    assert_eq!(served.exit, 0, "served read failed: {}", served.stderr);
    assert!(
        served.stderr.contains("not_a_real_key"),
        "a served read must still report an unrecognized config key: {:?}",
        served.stderr
    );
}

struct CliRun {
    exit: i32,
    stderr: String,
}

/// Run the CLI against the owned endpoint and child configuration root.
fn run_cli(
    home: &std::path::Path,
    xdg: &std::path::Path,
    address: SocketAddr,
    args: &[&str],
) -> CliRun {
    let output = Command::new(env!("CARGO_BIN_EXE_aoe"))
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", xdg)
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("AOE_DAEMON_TOKEN", TOKEN)
        .env_remove("AOE_DAEMON_URL")
        .env_remove("DO_NOT_TRACK")
        .env_remove("AGENT_OF_EMPIRES_PROFILE")
        .arg(format!("--daemon-url=http://{address}"))
        .args(args)
        .output()
        .expect("the aoe binary runs");
    CliRun {
        exit: output.status.code().expect("normal child exit"),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn a_failed_host_hook_does_not_poison_runtime_reads() {
    use agent_of_empires::cli::Cli;
    use agent_of_empires::server::test_support;
    use agent_of_empires::session::{Instance, Status};
    use clap::Parser;
    let home = TempAppDir::new();
    let _shell = crate::common::EnvGuard::new(&["SHELL"])
        .and_set("SHELL", which::which("sh").expect("host hooks require sh"));
    agent_of_empires::session::create_profile("main").unwrap();
    agent_of_empires::session::create_profile("healthy").unwrap();
    let failure = agent_of_empires::session::config::repo_config::run_before_session_hooks(
        &["sh -c 'printf \"hook failure evidence\\n\" >&2; exit 7'".into()],
        home._dir.path(),
        &[],
        &[],
    )
    .unwrap_err()
    .to_string();
    assert!(failure.contains("\nstderr:\nhook failure evidence"));
    let mut failed = Instance::new("failed hook row", home._dir.path().to_str().unwrap());
    failed.id = "failed-hook".into();
    failed.tool = "claude".into();
    failed.source_profile = "main".into();
    failed.status = Status::Error;
    failed.last_error = Some(failure.clone());
    let mut healthy = Instance::new("healthy independent row", "/healthy-repo");
    healthy.id = "healthy-row".into();
    healthy.tool = "claude".into();
    healthy.source_profile = "healthy".into();
    test_support::seed_instances_on_disk_for_test("main", vec![failed.clone()]);
    test_support::seed_instances_on_disk_for_test("healthy", vec![healthy]);
    let state =
        build_test_app_state_with_policy(vec![failed], hosts(), Vec::new(), Some(TOKEN.into()));
    test_support::accept_runtime_read_cache_for_test(&state).await;
    {
        let mut rows = state.instances.write().await;
        let failed = rows.iter_mut().find(|row| row.id == "failed-hook").unwrap();
        failed.status = Status::Error;
        failed.last_error = Some(failure.clone());
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = build_router_for_test(state.clone());
    let shutdown = state.shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .unwrap();
    });
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        let mut request = format!("ws://{address}/api/runtime/ws")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let hello = socket.next().await.unwrap().unwrap();
        let hello: serde_json::Value = serde_json::from_str(hello.to_text().unwrap()).unwrap();
        assert_eq!(hello["kind"], "hello");
        let snapshot = socket.next().await.unwrap().unwrap();
        let snapshot: serde_json::Value =
            serde_json::from_str(snapshot.to_text().unwrap()).unwrap();
        assert_eq!(snapshot["kind"], "snapshot");
        let failed = snapshot["data"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == "failed-hook")
            .unwrap();
        assert_eq!(failed["last_error"], failure);
        socket.close(None).await.unwrap();
        while let Some(message) = socket.next().await {
            if matches!(message.unwrap(), Message::Close(_)) {
                break;
            }
        }
        snapshot
    })
    .await
    .unwrap();
    let mut source = read_source(address, Some(TOKEN));
    source.explicit_profile = Some("main".into());
    let cases: &[&[&str]] = &[
        &["aoe", "list", "--json"],
        &["aoe", "status", "--json"],
        &["aoe", "session", "show", "failed-hook", "--json"],
        &["aoe", "session", "list-trash"],
        &["aoe", "group", "list"],
        &["aoe", "profile"],
        &["aoe", "project", "list"],
    ];
    for argv in cases {
        let cli = Cli::parse_from(*argv);
        let outcome = execute(
            agent_of_empires::cli::runtime_read::classify(cli.command.as_ref()).unwrap(),
            &source,
        )
        .await;
        assert_eq!(outcome.exit, 0, "{argv:?}: {:?}", outcome.stderr);
        if argv.get(2) == Some(&"show") {
            let shown: serde_json::Value =
                serde_json::from_str(outcome.stdout.as_deref().unwrap()).unwrap();
            assert_eq!(shown["id"], "failed-hook");
            assert_eq!(shown["status"], "error");
        } else if argv.get(1) == Some(&"list") {
            let listed: serde_json::Value =
                serde_json::from_str(outcome.stdout.as_deref().unwrap()).unwrap();
            assert_eq!(listed[0]["id"], "failed-hook");
        } else if argv.get(1) == Some(&"profile") {
            let profiles = outcome.stdout.unwrap();
            assert!(profiles.contains("main") && profiles.contains("healthy"));
        }
    }
    source.explicit_profile = Some("healthy".into());
    let cli = Cli::parse_from(["aoe", "list", "--json"]);
    let outcome = execute(
        agent_of_empires::cli::runtime_read::classify(cli.command.as_ref()).unwrap(),
        &source,
    )
    .await;
    assert_eq!(outcome.exit, 0, "{:?}", outcome.stderr);
    let listed: serde_json::Value =
        serde_json::from_str(outcome.stdout.as_deref().unwrap()).unwrap();
    assert_eq!(listed[0]["id"], "healthy-row");
    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/cli-read/snapshot.schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .unwrap()
        .validate(&snapshot["data"])
        .unwrap();
    state.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}
#[tokio::test]
#[serial_test::serial]
async fn registered_project_text_and_native_paths_survive_runtime_reads() {
    use agent_of_empires::cli::Cli;
    use agent_of_empires::server::test_support;
    use agent_of_empires::session::{projects, Instance, Project, ProjectScope};
    use clap::Parser;
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};

    let schema: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/cli-read/snapshot.schema.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let validator = jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .build(&schema)
        .unwrap();
    for (directory, name, branch) in [
        ("team\twork", "native-path", None),
        ("name-project", "display\n\tname", None),
        ("branch-project", "branch-text", Some("feature\nbranch")),
    ] {
        let home = TempAppDir::new();
        agent_of_empires::session::create_profile("main").unwrap();
        agent_of_empires::session::create_profile("healthy").unwrap();
        let path = home._dir.path().join(directory);
        std::fs::create_dir(&path).unwrap();
        let path = path.canonicalize().unwrap();
        let project = projects::add(
            "main",
            ProjectScope::Global,
            Project::new(name, path.to_str().unwrap(), ProjectScope::Global)
                .with_base_branch(branch.map(str::to_owned)),
            false,
        )
        .unwrap();
        let mut main = Instance::new("main row", home._dir.path().to_str().unwrap());
        main.id = "main-row".into();
        main.tool = "claude".into();
        main.source_profile = "main".into();
        let mut healthy = Instance::new("independent healthy row", "/healthy-repo");
        healthy.id = "healthy-row".into();
        healthy.tool = "claude".into();
        healthy.source_profile = "healthy".into();
        test_support::seed_instances_on_disk_for_test("main", vec![main.clone()]);
        test_support::seed_instances_on_disk_for_test("healthy", vec![healthy]);
        let state =
            build_test_app_state_with_policy(vec![main], hosts(), Vec::new(), Some(TOKEN.into()));
        test_support::accept_runtime_read_cache_for_test(&state).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = build_router_for_test(state.clone());
        let shutdown = state.shutdown.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .unwrap();
        });
        let mut source = read_source(address, Some(TOKEN));
        source.explicit_profile = Some("main".into());
        let cli = Cli::parse_from(["aoe", "project", "list", "--scope", "global", "--json"]);
        let outcome = execute(
            agent_of_empires::cli::runtime_read::classify(cli.command.as_ref()).unwrap(),
            &source,
        )
        .await;
        assert_eq!(outcome.exit, 0, "{directory:?}: {:?}", outcome.stderr);
        let listed: serde_json::Value =
            serde_json::from_str(outcome.stdout.as_deref().unwrap()).unwrap();
        assert_eq!(
            listed[0]["name"], project.name,
            "registered project missing for {directory:?}"
        );
        assert_eq!(listed[0]["path"], project.path);
        assert_eq!(
            listed[0]["default_base_branch"],
            serde_json::to_value(branch).unwrap()
        );

        state
            .instances
            .write()
            .await
            .iter_mut()
            .find(|row| row.id == "main-row")
            .unwrap()
            .project_path = project.path.clone();
        let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
            let mut request = format!("ws://{address}/api/runtime/ws")
                .into_client_request()
                .unwrap();
            request
                .headers_mut()
                .insert("Authorization", format!("Bearer {TOKEN}").parse().unwrap());
            let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
            let hello = socket.next().await.unwrap().unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(hello.to_text().unwrap()).unwrap()
                    ["kind"],
                "hello"
            );
            let snapshot = socket.next().await.unwrap().unwrap();
            let snapshot: serde_json::Value =
                serde_json::from_str(snapshot.to_text().unwrap()).unwrap();
            assert_eq!(snapshot["kind"], "snapshot");
            let row = snapshot["data"]["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["id"] == "main-row")
                .unwrap();
            assert_eq!(row["project_path"], project.path);
            socket.close(None).await.unwrap();
            while let Some(message) = socket.next().await {
                if matches!(message.unwrap(), Message::Close(_)) {
                    break;
                }
            }
            snapshot
        })
        .await
        .unwrap();
        validator.validate(&snapshot["data"]).unwrap();
        let cases: &[&[&str]] = &[
            &["aoe", "list", "--json"],
            &["aoe", "status", "--json"],
            &["aoe", "session", "show", "main-row", "--json"],
            &["aoe", "session", "list-trash"],
            &["aoe", "group", "list"],
            &["aoe", "profile"],
            &["aoe", "project", "list"],
        ];
        for argv in cases {
            let cli = Cli::parse_from(*argv);
            let outcome = execute(
                agent_of_empires::cli::runtime_read::classify(cli.command.as_ref()).unwrap(),
                &source,
            )
            .await;
            assert_eq!(
                outcome.exit, 0,
                "{directory:?} {argv:?}: {:?}",
                outcome.stderr
            );
            if argv.get(1) == Some(&"list") {
                let rows: serde_json::Value =
                    serde_json::from_str(outcome.stdout.as_deref().unwrap()).unwrap();
                assert_eq!(rows[0]["id"], "main-row");
            }
        }
        source.explicit_profile = Some("healthy".into());
        let cli = Cli::parse_from(["aoe", "list", "--json"]);
        let outcome = execute(
            agent_of_empires::cli::runtime_read::classify(cli.command.as_ref()).unwrap(),
            &source,
        )
        .await;
        assert_eq!(outcome.exit, 0, "{directory:?}: {:?}", outcome.stderr);
        let rows: serde_json::Value =
            serde_json::from_str(outcome.stdout.as_deref().unwrap()).unwrap();
        assert_eq!(rows[0]["id"], "healthy-row");
        state.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
#[serial_test::serial]
async fn archive_and_trash_after_a_clock_correction_remain_readable() {
    use agent_of_empires::cli::Cli;
    use agent_of_empires::server::test_support;
    use agent_of_empires::session::Instance;
    use clap::Parser;

    let _home = TempAppDir::new();
    agent_of_empires::session::create_profile("main").unwrap();
    agent_of_empires::session::create_profile("healthy").unwrap();
    let future = chrono::DateTime::parse_from_rfc3339("9999-12-31T23:59:59Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let mut live = Instance::new("clock live", "/repo");
    live.id = "live-row".into();
    live.source_profile = "main".into();
    let mut archived = Instance::new("clock archive", "/repo");
    archived.id = "archived-row".into();
    archived.source_profile = "main".into();
    archived.created_at = future;
    archived.archive();
    let mut trashed = Instance::new("clock trash", "/repo");
    trashed.id = "trashed-row".into();
    trashed.source_profile = "main".into();
    trashed.created_at = future;
    trashed.trash();
    assert!(archived.archived_at.unwrap() < future);
    assert!(trashed.trashed_at.unwrap() < future);
    let archived_at = serde_json::to_value(archived.archived_at).unwrap();
    let trashed_at = serde_json::to_value(trashed.trashed_at).unwrap();
    let mut healthy = Instance::new("independent healthy row", "/healthy-repo");
    healthy.id = "healthy-row".into();
    healthy.source_profile = "healthy".into();
    let main = vec![live, archived, trashed];
    test_support::seed_instances_on_disk_for_test("main", main.clone());
    test_support::seed_instances_on_disk_for_test("healthy", vec![healthy.clone()]);
    let mut instances = main;
    instances.push(healthy);
    let state =
        build_test_app_state_with_policy(instances, hosts(), Vec::new(), Some(TOKEN.into()));
    test_support::accept_runtime_read_cache_for_test(&state).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let router = build_router_for_test(state.clone());
    let shutdown = state.shutdown.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await
        .unwrap();
    });
    let mut source = read_source(address, Some(TOKEN));
    source.explicit_profile = Some("main".into());
    let cases: &[&[&str]] = &[
        &["aoe", "list", "--json"],
        &["aoe", "status", "--json"],
        &["aoe", "session", "show", "live-row", "--json"],
        &["aoe", "session", "list-trash"],
        &["aoe", "group", "list"],
        &["aoe", "profile"],
        &["aoe", "project", "list"],
    ];
    for argv in cases {
        let cli = Cli::parse_from(*argv);
        let outcome = execute(
            agent_of_empires::cli::runtime_read::classify(cli.command.as_ref()).unwrap(),
            &source,
        )
        .await;
        assert_eq!(outcome.exit, 0, "{argv:?}: {:?}", outcome.stderr);
        if argv.get(1) == Some(&"list") {
            let rows: serde_json::Value =
                serde_json::from_str(outcome.stdout.as_deref().unwrap()).unwrap();
            let rows = rows.as_array().unwrap();
            let ids: std::collections::BTreeSet<_> =
                rows.iter().map(|row| row["id"].as_str().unwrap()).collect();
            assert_eq!(
                ids,
                ["live-row", "archived-row", "trashed-row"]
                    .into_iter()
                    .collect()
            );
            for (id, field, timestamp, bucket) in [
                ("archived-row", "archived_at", &archived_at, "archived"),
                ("trashed-row", "trashed_at", &trashed_at, "trashed"),
            ] {
                let row = rows.iter().find(|row| row["id"] == id).unwrap();
                assert_eq!(&row[field], timestamp);
                assert_eq!(row["created_at"], serde_json::to_value(future).unwrap());
                assert_eq!(row["state"], bucket);
            }
        }
        if argv.get(2) == Some(&"list-trash") {
            assert!(outcome.stdout.as_deref().unwrap().contains("clock trash"));
        }
    }
    source.explicit_profile = Some("healthy".into());
    let cli = Cli::parse_from(["aoe", "list", "--json"]);
    let outcome = execute(
        agent_of_empires::cli::runtime_read::classify(cli.command.as_ref()).unwrap(),
        &source,
    )
    .await;
    assert_eq!(outcome.exit, 0, "{:?}", outcome.stderr);
    let rows: serde_json::Value = serde_json::from_str(outcome.stdout.as_deref().unwrap()).unwrap();
    assert_eq!(rows[0]["id"], "healthy-row");
    state.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}
