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
    let release = serde_json::json!({
        "tag_name": format!("v{latest_version}"),
        "body": "fixture release notes",
        "published_at": "2026-04-29T00:00:00Z",
    });
    spawn_fixture_with(release.clone(), serde_json::json!([release]))
}

/// Serves `latest` for `releases/latest` and `releases` for the list endpoint.
fn spawn_fixture_with(latest: serde_json::Value, releases: serde_json::Value) -> FixtureServer {
    let (port_tx, port_rx) = mpsc::channel::<u16>();
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>();
    let latest = latest.to_string();
    let releases_value = releases.clone();
    let releases = releases.to_string();

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

            let app = axum::Router::new()
                .route(
                    "/repos/agent-of-empires/agent-of-empires/releases/latest",
                    axum::routing::get(move || {
                        let body = latest.clone();
                        async move {
                            (
                                [(axum::http::header::CONTENT_TYPE, "application/json")],
                                body,
                            )
                        }
                    }),
                )
                .route(
                    "/repos/agent-of-empires/agent-of-empires/releases/tags/{tag}",
                    axum::routing::get(
                        move |axum::extract::Path(tag): axum::extract::Path<String>| {
                            let found = releases_value
                                .as_array()
                                .and_then(|all| all.iter().find(|r| r["tag_name"] == tag.as_str()))
                                .map(|r| r.to_string());
                            async move {
                                match found {
                                    Some(body) => (
                                        axum::http::StatusCode::OK,
                                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                                        body,
                                    ),
                                    None => (
                                        axum::http::StatusCode::NOT_FOUND,
                                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                                        "{}".to_string(),
                                    ),
                                }
                            }
                        },
                    ),
                )
                .route(
                    "/repos/agent-of-empires/agent-of-empires/releases",
                    axum::routing::get(move || {
                        let body = releases.clone();
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

fn stable_and_nightly_fixture() -> FixtureServer {
    let stable = env!("CARGO_PKG_VERSION");
    let release = |tag: &str, name: &str, prerelease: bool| {
        serde_json::json!({
            "tag_name": tag,
            "name": name,
            "body": "",
            "published_at": "2026-04-29T00:00:00Z",
            "prerelease": prerelease,
        })
    };
    let stable_release = release(&format!("v{stable}"), &format!("v{stable}"), false);
    spawn_fixture_with(
        stable_release.clone(),
        serde_json::json!([
            release(
                "nightly-29990102",
                "Agent of Empires v999.0.0-nightly.29990102",
                true
            ),
            stable_release,
            release(
                "nightly-29990101",
                "Agent of Empires v999.0.0-nightly.29990101",
                true
            ),
        ]),
    )
}

fn run_update(fixture: &FixtureServer, args: &[&str]) -> std::process::Output {
    let tmp = tempfile::TempDir::new().unwrap();
    Command::new(aoe_binary())
        .arg("update")
        .args(args)
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("AOE_UPDATE_API_BASE", &fixture.base_url)
        .output()
        .expect("running aoe update")
}

/// A stable build's `--check` hints at a newer nightly on stderr and leaves stdout alone.
#[test]
#[serial_test::parallel]
fn update_check_hints_newer_nightly_on_stderr_only() {
    let fixture = stable_and_nightly_fixture();
    let output = run_update(&fixture, &["--check"]);

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stdout.contains("available: false"), "stdout was: {stdout}");
    assert!(!stdout.contains("nightly"), "stdout was: {stdout}");
    assert!(
        stderr.contains("v999.0.0-nightly.29990102") && stderr.contains("--channel nightly"),
        "stderr was: {stderr}"
    );
}

/// `--channel` and `--version` resolve the target from the release list.
#[test]
#[serial_test::parallel]
fn update_check_resolves_explicit_targets() {
    let fixture = stable_and_nightly_fixture();
    let stable = env!("CARGO_PKG_VERSION");
    for (args, latest, available) in [
        (
            vec!["--check", "--channel", "nightly"],
            "999.0.0-nightly.29990102",
            true,
        ),
        (
            vec!["--check", "--version", "nightly-29990101"],
            "999.0.0-nightly.29990101",
            true,
        ),
        (
            vec!["--check", "--version", &format!("v{stable}")],
            stable,
            false,
        ),
        (vec!["--check", "--version", stable], stable, false),
        (
            vec!["--check", "--version", "999.0.0-nightly.29990101"],
            "999.0.0-nightly.29990101",
            true,
        ),
        (vec!["--check", "--channel", "stable"], stable, false),
    ] {
        let output = run_update(&fixture, &args);
        assert!(output.status.success(), "{args:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains(&format!("latest:  {latest}"))
                && stdout.contains(&format!("available: {available}")),
            "{args:?}: {stdout}"
        );
    }
}

/// A full nightly version must match the release on its date, not just the date.
#[test]
#[serial_test::parallel]
fn update_explicit_nightly_version_with_wrong_prefix_is_rejected() {
    let fixture = stable_and_nightly_fixture();
    let output = run_update(
        &fixture,
        &["--check", "--version", "1.99.9-nightly.29990101"],
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("no release found"), "stderr was: {stderr}");
}

/// An explicit target on an install that is not an install-script tarball is refused before
/// any release lookup, so the refusal holds offline.
#[test]
#[serial_test::parallel]
fn update_explicit_target_refuses_non_tarball_install_without_network() {
    let tmp = tempfile::TempDir::new().unwrap();
    let output = Command::new(aoe_binary())
        .args(["update", "--channel", "nightly", "-y"])
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("AOE_UPDATE_API_BASE", "http://127.0.0.1:1")
        .output()
        .expect("running aoe update");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("tarball"), "stderr was: {stderr}");
}

/// With update checks off, `aoe update --check` makes no nightly lookup and prints no hint.
#[test]
#[serial_test::parallel]
fn update_check_hint_respects_disabled_update_checks() {
    let fixture = stable_and_nightly_fixture();
    let tmp = tempfile::TempDir::new().unwrap();
    let config_dir = tmp
        .path()
        .join(".config")
        .join(agent_of_empires::session::APP_DIR_NAME_XDG);
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[updates]\nupdate_check_mode = \"off\"\n",
    )
    .unwrap();
    let output = Command::new(aoe_binary())
        .args(["update", "--check"])
        .env("HOME", tmp.path())
        .env("XDG_CONFIG_HOME", tmp.path().join(".config"))
        .env("AOE_UPDATE_API_BASE", &fixture.base_url)
        .output()
        .expect("running aoe update");

    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("nightly"), "stderr was: {stderr}");
}
