//! A plugin proxy launched ahead of the adapter: the runner passes the proxy's
//! argv through intact and hands it an owner-only launch config that is gone
//! once the session ends. Spawns a real `aoe __acp-runner` over the fake proxy
//! (`acp-worker/test-shim/fake-proxy.mjs`) wrapping the Node shim.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use agent_of_empires::process::worker_registry;
use serial_test::serial;

use crate::common::{set_temp_home, shim_node, shim_path, shim_ready};

const PROXY_SPEC_JSON: &str = r#"{"plugin_id":"fake-proxy","command":"p","args":[],"failure_policy":"neutral_boot","display_name":"Fake","compat":{"agent_names":[],"version_req":null}}"#;

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{what} never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[serial]
fn proxy_receives_argv_and_owner_only_launch_config_removed_at_session_end() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _home = set_temp_home(temp.path());
    let record_dir = temp.path().join("record");
    std::fs::create_dir_all(&record_dir).unwrap();

    let session_id = "proxy-launch";
    let launch_config = worker_registry::write_proxy_launch_config(session_id, b"{}").unwrap();
    assert_eq!(
        std::fs::metadata(&launch_config)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    let node = shim_node().expect("shim prerequisite");
    let proxy = shim_path().with_file_name("fake-proxy.mjs");
    let socket = worker_registry::socket_path_for(session_id).unwrap();
    let mut runner = Command::new(env!("CARGO_BIN_EXE_aoe"))
        .args(["__acp-runner", "--socket"])
        .arg(&socket)
        .args(["--session-id", session_id, "--agent-name", "shim", "--cwd"])
        .arg(temp.path())
        .args(["--proxy", PROXY_SPEC_JSON, "--degraded", "Fake"])
        .arg("--proxy-launch-config")
        .arg(&launch_config)
        .args(["--", node.to_str().unwrap(), proxy.to_str().unwrap()])
        .args(["--flag", "a b", "it's \"quoted\"", "--"])
        .args([node.to_str().unwrap(), shim_path().to_str().unwrap()])
        .env("FAKE_PROXY_RECORD_DIR", &record_dir)
        .env("AOE_PROXY_LAUNCH_CONFIG", &launch_config)
        .spawn()
        .expect("spawn acp runner");

    wait_for(&record_dir.join("env.json"), "proxy env record");
    let argv: Vec<String> =
        serde_json::from_slice(&std::fs::read(record_dir.join("argv.json")).unwrap()).unwrap();
    assert_eq!(
        argv,
        [
            "--flag",
            "a b",
            "it's \"quoted\"",
            "--",
            node.to_str().unwrap(),
            shim_path().to_str().unwrap()
        ]
    );
    let env: serde_json::Value =
        serde_json::from_slice(&std::fs::read(record_dir.join("env.json")).unwrap()).unwrap();
    assert_eq!(
        env["AOE_PROXY_LAUNCH_CONFIG"],
        launch_config.to_str().unwrap()
    );
    assert!(launch_config.exists(), "config must outlive launch");
    let record = worker_registry::load(session_id)
        .unwrap()
        .expect("runner record");
    assert_eq!(
        record.proxy.expect("proxy persisted").plugin_id,
        "fake-proxy"
    );
    assert_eq!(record.degraded.as_deref(), Some("Fake"));
    assert_eq!(
        std::fs::metadata(&launch_config)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // The proxy exiting ends the session.
    let pid = std::fs::read_to_string(record_dir.join("pid")).unwrap();
    assert!(Command::new("kill")
        .args(["-TERM", pid.trim()])
        .status()
        .unwrap()
        .success());
    let deadline = Instant::now() + Duration::from_secs(10);
    while runner.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            let _ = runner.kill();
            panic!("runner did not exit after its proxy did");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !launch_config.exists(),
        "config must be removed at session end"
    );
}

/// A request-supplied env var must not choose what the runner unlinks at exit.
#[test]
#[serial]
fn runner_exit_ignores_a_launch_config_env_it_was_not_given_by_flag() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _home = set_temp_home(temp.path());
    let victim = temp.path().join("victim");
    std::fs::write(&victim, b"keep").unwrap();

    let session_id = "launch-env-victim";
    let node = shim_node().expect("shim prerequisite");
    let socket = worker_registry::socket_path_for(session_id).unwrap();
    let mut runner = Command::new(env!("CARGO_BIN_EXE_aoe"))
        .args(["__acp-runner", "--socket"])
        .arg(&socket)
        .args(["--session-id", session_id, "--agent-name", "shim", "--cwd"])
        .arg(temp.path())
        .args(["--", node.to_str().unwrap(), shim_path().to_str().unwrap()])
        .env("AOE_PROXY_LAUNCH_CONFIG", &victim)
        .spawn()
        .expect("spawn acp runner");

    wait_until("runner record never appeared", || {
        worker_registry::load(session_id).unwrap().is_some()
    });
    signal("-TERM", &runner.id().to_string());
    wait_until("runner did not exit", || {
        runner.try_wait().unwrap().is_some()
    });
    assert!(victim.exists(), "the runner unlinked an env-named path");
}

fn pid_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Runs a runner (its own group leader, as the daemon spawns it) over the fake
/// proxy and a `sleep` adapter (unlike the shim, it ignores stdin EOF), ends it
/// with `end`, and asserts neither the proxy nor the adapter outlives it.
fn assert_no_orphans(session_id: &str, proxy_mode: &str, end: impl FnOnce(&str, &str)) {
    let temp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _home = set_temp_home(temp.path());
    let record_dir = temp.path().join("record");
    std::fs::create_dir_all(&record_dir).unwrap();

    let node = shim_node().expect("shim prerequisite");
    let proxy = shim_path().with_file_name("fake-proxy.mjs");
    let socket = worker_registry::socket_path_for(session_id).unwrap();
    let mut runner = Command::new(env!("CARGO_BIN_EXE_aoe"))
        .args(["__acp-runner", "--socket"])
        .arg(&socket)
        .args(["--session-id", session_id, "--agent-name", "shim", "--cwd"])
        .arg(temp.path())
        .args(["--", node.to_str().unwrap(), proxy.to_str().unwrap()])
        .args(["--", "sleep", "600"])
        .env("FAKE_PROXY_RECORD_DIR", &record_dir)
        .env("FAKE_PROXY_MODE", proxy_mode)
        .process_group(0)
        .spawn()
        .expect("spawn acp runner");

    wait_for(&record_dir.join("adapter_pid"), "adapter pid record");
    let read = |name: &str| std::fs::read_to_string(record_dir.join(name)).unwrap();
    let (proxy_pid, adapter_pid) = (read("pid"), read("adapter_pid"));
    assert!(pid_alive(&proxy_pid) && pid_alive(&adapter_pid));

    end(&runner.id().to_string(), proxy_pid.trim());

    wait_until("runner did not exit", || {
        runner.try_wait().unwrap().is_some()
    });
    wait_until("proxy outlived the runner", || !pid_alive(&proxy_pid));
    wait_until("adapter outlived the runner", || !pid_alive(&adapter_pid));
}

/// An inherited fd keeps its number through the proxy even when the fds below
/// it are closed (`--env-file /dev/fd/9` names the number).
#[test]
#[serial]
fn proxy_keeps_inherited_fd_numbers_across_closed_slots() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _home = set_temp_home(temp.path());
    let record_dir = temp.path().join("record");
    std::fs::create_dir_all(&record_dir).unwrap();
    let env_file = temp.path().join("env-file");
    std::fs::write(&env_file, "SECRET=1").unwrap();
    let out = record_dir.join("fd9");

    let node = shim_node().expect("shim prerequisite");
    let proxy = shim_path().with_file_name("fake-proxy.mjs");
    let socket = worker_registry::socket_path_for("proxy-fd9").unwrap();
    let file = std::fs::File::open(&env_file).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aoe"));
    cmd.args(["__acp-runner", "--socket"])
        .arg(&socket)
        .args(["--session-id", "proxy-fd9", "--agent-name", "shim", "--cwd"])
        .arg(temp.path())
        .args(["--", node.to_str().unwrap(), proxy.to_str().unwrap()])
        .args(["--", "sh", "-c", "cat /dev/fd/9 > \"$0\"; exec sleep 600"])
        .arg(&out)
        .env("FAKE_PROXY_RECORD_DIR", &record_dir)
        .process_group(0);
    let src = std::os::fd::AsRawFd::as_raw_fd(&file);
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(src, 9) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in 3..9 {
                libc::close(fd);
            }
            Ok(())
        });
    }
    let mut runner = cmd.spawn().expect("spawn acp runner");
    drop(file);

    wait_until("adapter never read fd 9", || {
        std::fs::read(&out).is_ok_and(|b| !b.is_empty())
    });
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "SECRET=1");

    signal("-TERM", &runner.id().to_string());
    wait_until("runner did not exit", || {
        runner.try_wait().unwrap().is_some()
    });
}

fn signal(sig: &str, pid: &str) {
    assert!(Command::new("kill")
        .args([sig, pid])
        .status()
        .unwrap()
        .success());
}

#[test]
#[serial]
fn runner_shutdown_leaves_no_orphan_proxy_or_adapter() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    assert_no_orphans("proxy-orphan-term", "orphan-on-term", |runner, _| {
        signal("-TERM", runner)
    });
}

#[test]
#[serial]
fn proxy_exit_leaves_no_orphan_adapter() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    assert_no_orphans("proxy-orphan-exit", "ok", |_, proxy| signal("-KILL", proxy));
}
