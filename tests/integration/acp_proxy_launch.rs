//! A plugin proxy launched ahead of the adapter: the runner passes the proxy's
//! argv through intact and hands it an owner-only launch config that is gone
//! once the session ends. The isolated child uses the production runner CLI
//! arguments and runner over `acp-worker/test-shim/fake-proxy.mjs`.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::{getpgid, Pid};
use serial_test::serial;

use super::{arm_selected, managed_runner};
use crate::acp::supervisor::test_support::managed::{shim_node, shim_path, shim_ready};
use crate::process::{process_birth_identity, worker_registry, CapturedProcess};
use crate::session::test_support::isolate_app_dir_at;
use crate::session::{Instance, ResumeIntent, Status, Storage, View};

const PROXY_SPEC_JSON: &str = r#"{"plugin_id":"fake-proxy","command":"p","args":[],"failure_policy":"neutral_boot","display_name":"Fake","compat":{"agent_names":[],"version_req":null}}"#;

fn runner_command(
    test_name: &str,
    cwd: &Path,
    session_id: &str,
    append_args: impl FnOnce(&mut Command),
) -> Command {
    let storage = Storage::new_unwatched("default").unwrap();
    let mut row = Instance::new("proxy launch", cwd.to_str().unwrap());
    row.id = session_id.into();
    row.source_profile = storage.profile().into();
    row.agent_name = Some("codex".into());
    row.view = View::Structured;
    row.status = Status::Idle;
    row.lifecycle_generation = 5;
    row.resume_intent = ResumeIntent::Use(format!("retained-{session_id}"));
    storage
        .update(|rows, _| {
            rows.push(row);
            Ok(())
        })
        .unwrap();
    let producer = uuid::Uuid::new_v4();
    let attempt = arm_selected(&storage, session_id, producer, None);
    let (launch, _) = managed_runner::RunnerInspection::prepare(cwd, test_name);
    let mut args = Command::new(launch.executable());
    args.args(["__acp-runner", "--socket"])
        .arg(worker_registry::socket_path_for(session_id).unwrap())
        .args(["--session-id", session_id, "--agent-name", "shim"])
        .args(["--agent-key", "codex", "--cwd"])
        .arg(cwd)
        .args(["--source-profile", storage.profile()])
        .args(["--lifecycle-generation", "5", "--establishment-attempt"])
        .arg(attempt.id.to_string())
        .arg("--establishment-producer")
        .arg(producer.to_string());
    append_args(&mut args);
    let mut runner = launch.command(args.get_args()).unwrap();
    runner.process_group(0);
    runner
}

struct OwnedRunner {
    child: Child,
    reaped: bool,
}

impl OwnedRunner {
    fn spawn(command: &mut Command) -> Self {
        Self {
            child: command.spawn().expect("spawn ACP runner"),
            reaped: false,
        }
    }

    fn pid(&self) -> Pid {
        Pid::from_raw(self.child.id() as i32)
    }

    fn terminate(&mut self) {
        kill(self.pid(), Signal::SIGTERM).unwrap();
    }

    fn wait_for_exit(&mut self, what: &str) {
        wait_until(what, || {
            let Some(status) = self.child.try_wait().unwrap() else {
                return false;
            };
            self.reaped = true;
            assert_eq!(
                status.signal(),
                Some(libc::SIGKILL),
                "runner must finish by retiring its group: {status}"
            );
            true
        });
    }

    fn capture_member(&self, path: &Path) -> OwnedMember {
        let pid = std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        let birth = process_birth_identity(pid)
            .unwrap()
            .expect("live group member");
        let member = CapturedProcess::capture(pid, &birth)
            .unwrap()
            .expect("capture live group member");
        assert_eq!(
            getpgid(Some(Pid::from_raw(pid as i32))).unwrap(),
            self.pid()
        );
        OwnedMember(member)
    }
}

impl Drop for OwnedRunner {
    fn drop(&mut self) {
        if !self.reaped {
            // The unreaped child still reserves the group leader's PID.
            let _ = killpg(self.pid(), Signal::SIGKILL);
            let _ = self.child.wait();
        }
    }
}

struct OwnedMember(CapturedProcess);

impl Drop for OwnedMember {
    fn drop(&mut self) {
        if matches!(self.0.exited(), Ok(false)) {
            let _ = self.0.signal(libc::SIGKILL);
        }
    }
}

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{what} never appeared");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test]
#[serial]
async fn proxy_receives_argv_and_owner_only_launch_config_removed_at_session_end() {
    let name = std::thread::current().name().unwrap().to_owned();
    if managed_runner::child_if_requested(&name).await {
        return;
    }
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _home = isolate_app_dir_at(temp.path());
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
    let mut command = runner_command(&name, temp.path(), session_id, |args| {
        args.args(["--proxy", PROXY_SPEC_JSON, "--degraded", "Fake"])
            .arg("--proxy-launch-config")
            .arg(&launch_config)
            .arg("--")
            .arg(node)
            .arg(proxy)
            .args(["--flag", "a b", "it's \"quoted\"", "--"])
            .arg(node)
            .arg(shim_path());
    });
    let mut runner = OwnedRunner::spawn(
        command
            .env("FAKE_PROXY_RECORD_DIR", &record_dir)
            .env("AOE_PROXY_LAUNCH_CONFIG", &launch_config),
    );

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
    let proxy = runner.capture_member(&record_dir.join("pid"));
    proxy.0.signal(libc::SIGTERM).unwrap();
    runner.wait_for_exit("runner did not exit after its proxy did");
    assert!(
        !launch_config.exists(),
        "config must be removed at session end"
    );
}

/// A request-supplied env var must not choose what the runner unlinks at exit.
#[tokio::test]
#[serial]
async fn runner_exit_ignores_a_launch_config_env_it_was_not_given_by_flag() {
    let name = std::thread::current().name().unwrap().to_owned();
    if managed_runner::child_if_requested(&name).await {
        return;
    }
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _home = isolate_app_dir_at(temp.path());
    let victim = temp.path().join("victim");
    std::fs::write(&victim, b"keep").unwrap();

    let session_id = "launch-env-victim";
    let node = shim_node().expect("shim prerequisite");
    let mut command = runner_command(&name, temp.path(), session_id, |args| {
        args.arg("--").arg(node).arg(shim_path());
    });
    let mut runner = OwnedRunner::spawn(command.env("AOE_PROXY_LAUNCH_CONFIG", &victim));

    wait_until("runner record never appeared", || {
        worker_registry::load(session_id).unwrap().is_some()
    });
    runner.terminate();
    runner.wait_for_exit("runner did not exit");
    assert!(victim.exists(), "the runner unlinked an env-named path");
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
fn assert_no_orphans(
    test_name: &str,
    session_id: &str,
    proxy_mode: &str,
    end: impl FnOnce(&mut OwnedRunner, &CapturedProcess),
) {
    let temp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _home = isolate_app_dir_at(temp.path());
    let record_dir = temp.path().join("record");
    std::fs::create_dir_all(&record_dir).unwrap();

    let node = shim_node().expect("shim prerequisite");
    let proxy = shim_path().with_file_name("fake-proxy.mjs");
    let mut command = runner_command(test_name, temp.path(), session_id, |args| {
        args.arg("--")
            .arg(node)
            .arg(proxy)
            .args(["--", "sleep", "600"]);
    });
    let mut runner = OwnedRunner::spawn(
        command
            .env("FAKE_PROXY_RECORD_DIR", &record_dir)
            .env("FAKE_PROXY_MODE", proxy_mode),
    );

    wait_for(&record_dir.join("adapter_pid"), "adapter pid record");
    let proxy = runner.capture_member(&record_dir.join("pid"));
    let adapter = runner.capture_member(&record_dir.join("adapter_pid"));

    end(&mut runner, &proxy.0);

    runner.wait_for_exit("runner did not exit");
    wait_until("proxy outlived the runner", || proxy.0.exited().unwrap());
    wait_until("adapter outlived the runner", || {
        adapter.0.exited().unwrap()
    });
}

/// An inherited fd keeps its number through the proxy even when the fds below
/// it are closed (`--env-file /dev/fd/9` names the number).
#[tokio::test]
#[serial]
async fn proxy_keeps_inherited_fd_numbers_across_closed_slots() {
    let name = std::thread::current().name().unwrap().to_owned();
    if managed_runner::child_if_requested(&name).await {
        return;
    }
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let _home = isolate_app_dir_at(temp.path());
    let record_dir = temp.path().join("record");
    std::fs::create_dir_all(&record_dir).unwrap();
    let env_file = temp.path().join("env-file");
    std::fs::write(&env_file, "SECRET=1").unwrap();
    let out = record_dir.join("fd9");

    let node = shim_node().expect("shim prerequisite");
    let proxy = shim_path().with_file_name("fake-proxy.mjs");
    let file = std::fs::File::open(&env_file).unwrap();
    let mut cmd = runner_command(&name, temp.path(), "proxy-fd9", |args| {
        args.arg("--")
            .arg(node)
            .arg(proxy)
            .args(["--", "sh", "-c", "cat /dev/fd/9 > \"$0\"; exec sleep 600"])
            .arg(&out);
    });
    cmd.env("FAKE_PROXY_RECORD_DIR", &record_dir);
    let src = std::os::fd::AsRawFd::as_raw_fd(&file);
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(src, 9) < 0 || libc::fcntl(9, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            for fd in 3..9 {
                // Keep the exec error pipe available until exec itself.
                if libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EBADF) {
                        return Err(error);
                    }
                }
            }
            Ok(())
        });
    }
    let mut runner = OwnedRunner::spawn(&mut cmd);
    drop(file);

    wait_until("adapter never read fd 9", || {
        std::fs::read(&out).is_ok_and(|b| !b.is_empty())
    });
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "SECRET=1");

    runner.terminate();
    runner.wait_for_exit("runner did not exit");
}

#[tokio::test]
#[serial]
async fn runner_shutdown_leaves_no_orphan_proxy_or_adapter() {
    let name = std::thread::current().name().unwrap().to_owned();
    if managed_runner::child_if_requested(&name).await {
        return;
    }
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    assert_no_orphans(&name, "proxy-orphan-term", "orphan-on-term", |runner, _| {
        runner.terminate();
    });
}

#[tokio::test]
#[serial]
async fn proxy_exit_leaves_no_orphan_adapter() {
    let name = std::thread::current().name().unwrap().to_owned();
    if managed_runner::child_if_requested(&name).await {
        return;
    }
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    assert_no_orphans(&name, "proxy-orphan-exit", "ok", |_, proxy| {
        proxy.signal(libc::SIGKILL).unwrap();
    });
}
