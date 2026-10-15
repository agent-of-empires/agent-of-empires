//! E2E tests for `aoe update`.

use serial_test::serial;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

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
