//! E2E tests for `aoe update`.

use serial_test::serial;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn aoe_binary() -> &'static str {
    env!("CARGO_BIN_EXE_aoe")
}

/// Test that `aoe update --dry-run` invokes `brew list aoe` during installation detection.
///
/// Requires: GitHub API access (check_for_update is called with force=true) and a newer
/// version than 1.4.6 to be published. In CI where both are available, the test validates
/// that the brew detection probe runs regardless of dry-run mode. Marked #[ignore] because
/// it depends on GitHub being reachable and a newer release being available.
#[test]
#[ignore]
#[serial]
fn update_calls_brew_when_method_is_homebrew() {
    // Set up a temp dir as PATH-shimmed `brew` that records argv.
    let shim_dir = tempfile::tempdir().unwrap();
    let brew_log = shim_dir.path().join("brew.log");
    let brew_shim = shim_dir.path().join("brew");
    fs::write(
        &brew_shim,
        format!(
            "#!/bin/sh\necho \"$@\" >> {}\nif [ \"$1\" = \"list\" ]; then\n  echo /usr/local/bin/aoe\n  exit 0\nfi\nexit 0\n",
            brew_log.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&brew_shim, fs::Permissions::from_mode(0o755)).unwrap();

    // Create an isolated config dir with a fake update cache so check_for_update
    // thinks there's a newer version available (without hitting GitHub).
    let config_home = tempfile::tempdir().unwrap();
    // The binary under test is built in debug mode, so it resolves the app
    // dir to the `-dev` namespace.
    let app_dir = config_home
        .path()
        .join(agent_of_empires::session::APP_DIR_NAME_XDG);
    fs::create_dir_all(&app_dir).unwrap();

    let cache = serde_json::json!({
        "checked_at": "2026-04-27T00:00:00Z",
        "latest_version": "1.4.7",
        "releases": []
    });
    fs::write(
        app_dir.join("update_cache.json"),
        serde_json::to_string_pretty(&cache).unwrap(),
    )
    .unwrap();

    // Run `aoe update --dry-run` with the shim on PATH and isolated XDG_CONFIG_HOME.
    // The detection path probes brew via `brew list aoe` regardless of dry-run.
    let path = format!(
        "{}:{}",
        shim_dir.path().display(),
        std::env::var("PATH").unwrap()
    );

    let output = Command::new(aoe_binary())
        .args(["update", "--dry-run"])
        .env("PATH", &path)
        .env("XDG_CONFIG_HOME", config_home.path())
        .output()
        .expect("running aoe update --dry-run");

    // The detection probe ran `brew list aoe`, so the log must exist.
    let log = fs::read_to_string(&brew_log).unwrap_or_default();
    assert!(
        log.contains("list aoe"),
        "expected `brew list aoe` to be invoked; log was: {log:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[tokio::test]
#[serial]
async fn update_via_tarball_replaces_binary_at_target_path() {
    // Build a minimal tar.gz containing a dummy `aoe-{platform}` script
    // that prints the expected version when run with --version.
    let workdir = tempfile::tempdir().unwrap();
    let platform = agent_of_empires::update::install::current_platform_string().unwrap();
    let dummy = workdir.path().join(format!("aoe-{platform}"));
    fs::write(&dummy, "#!/bin/sh\necho 'aoe 99.99.99'\n").unwrap();
    fs::set_permissions(&dummy, fs::Permissions::from_mode(0o755)).unwrap();

    let tarball = workdir.path().join(format!("aoe-{platform}.tar.gz"));
    let tar_status = Command::new("tar")
        .args([
            "czf",
            tarball.to_str().unwrap(),
            "-C",
            workdir.path().to_str().unwrap(),
            &format!("aoe-{platform}"),
        ])
        .status()
        .unwrap();
    assert!(tar_status.success(), "tar czf failed");

    // Spin up an axum server that serves the tarball.
    use std::net::TcpListener as StdListener;
    let std_listener = StdListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let port = std_listener.local_addr().unwrap().port();
    let tarball_bytes = fs::read(&tarball).unwrap();
    let path_for_route = format!("/v99.99.99/aoe-{platform}.tar.gz");
    let app = axum::Router::new().route(
        &path_for_route,
        axum::routing::get(move || {
            let body = tarball_bytes.clone();
            async move { axum::body::Bytes::from(body) }
        }),
    );
    let listener = tokio::net::TcpListener::from_std(std_listener).unwrap();
    let server_handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Pre-place a target binary at a writable path.
    let install_dir = tempfile::tempdir().unwrap();
    let target = install_dir.path().join("aoe");
    fs::write(&target, "old binary").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();

    // Run update_via_tarball pointed at our local server.
    // SAFETY: tests run with #[serial], so concurrent env mutation is safe here.
    unsafe {
        std::env::set_var("AOE_UPDATE_BASE_URL", format!("http://127.0.0.1:{port}"));
    }
    let result =
        agent_of_empires::update::install::update_via_tarball(&target, "99.99.99", None).await;
    unsafe {
        std::env::remove_var("AOE_UPDATE_BASE_URL");
    }
    server_handle.abort();
    result.expect("update_via_tarball should succeed");

    // Assert the target was replaced.
    let output = Command::new(&target).arg("--version").output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("99.99.99"), "stdout: {stdout}");
}
