//! An orphaned or superseded runner must retire its actual native child.
//! These runners deliberately inherit the test's process group, exercising
//! non-group-leader teardown without signalling unrelated test processes.

use crate::process::runner::test_support::{arm_selected, managed_runner};
use crate::process::{worker_registry, CapturedProcess};
use crate::session::{Instance, ResumeIntent, Status, Storage, View};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

struct OwnedRunner {
    child: Child,
    native: Option<CapturedProcess>,
    _directory: tempfile::TempDir,
}

impl Drop for OwnedRunner {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            // This unreaped Child still reserves the original runner PID.
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(self.child.id() as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if let Some(native) = &self.native {
            if !native.exited().unwrap_or(false) {
                let _ = native.signal(libc::SIGKILL);
            }
        }
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// Commit a real retained selection before the production runner's birth fence.
/// No session establishment result or readiness is supplied by this fixture.
fn spawn_runner_and_wait_for_record(test_name: &str, session_id: &str) -> (OwnedRunner, PathBuf) {
    let home = crate::session::get_app_dir().unwrap();
    let storage = Storage::new_unwatched("default").unwrap();
    let mut row = Instance::new("orphan runner", home.to_str().unwrap());
    row.id = session_id.into();
    row.source_profile = storage.profile().into();
    row.agent_name = Some("codex".into());
    row.view = View::Structured;
    row.status = Status::Idle;
    row.lifecycle_generation = 5;
    row.resume_intent = ResumeIntent::Use(format!("retained-{session_id}"));
    storage
        .update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })
        .unwrap();
    let producer = uuid::Uuid::new_v4();
    let attempt = arm_selected(&storage, session_id, producer, None);
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let (executable, inspection) =
        managed_runner::RunnerInspection::prepare(directory.path(), test_name);
    let socket = worker_registry::socket_path_for(session_id).unwrap();
    let record_path = worker_registry::workers_dir()
        .unwrap()
        .join(format!("{session_id}.json"));
    let child = Command::new(executable)
        .arg("__acp-runner")
        .arg("--socket")
        .arg(&socket)
        .arg("--session-id")
        .arg(session_id)
        .args(["--agent-name", "cat", "--agent-key", "codex"])
        .arg("--cwd")
        .arg(&home)
        .args(["--source-profile", storage.profile()])
        .args(["--generation", "71", "--lifecycle-generation", "5"])
        .arg("--establishment-attempt")
        .arg(attempt.id.to_string())
        .arg("--establishment-producer")
        .arg(producer.to_string())
        .args(["--", "cat"])
        .env("AOE_ACP_WATCHDOG_POLL_MS", "150")
        .spawn()
        .expect("spawn actual acp runner without setsid");
    let mut owner = OwnedRunner {
        child,
        native: None,
        _directory: directory,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            owner.child.try_wait().unwrap().is_none(),
            "runner exited before native birth"
        );
        if let Some(record) = worker_registry::load_strict(session_id).unwrap() {
            if let Some(proof) = inspection
                .native_spawn_proof_for_child(&owner.child, session_id, producer)
                .unwrap()
            {
                assert_eq!(
                    proof.runner,
                    crate::acp::runner_lifecycle::RunnerIdentity::from_record(&record)
                );
                owner.native = Some(
                    CapturedProcess::capture(proof.pid, &proof.birth)
                        .unwrap()
                        .expect("actual parent's original Native remains live"),
                );
                assert_ne!(
                    nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(owner.child.id() as i32)))
                        .unwrap()
                        .as_raw(),
                    owner.child.id() as i32,
                    "exercise non-group-leader teardown"
                );
                return (owner, record_path);
            }
        }
        assert!(
            Instant::now() < deadline,
            "runner did not publish its actual Native birth"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_exits_within(owner: &mut OwnedRunner, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while owner.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while !owner.native.as_ref().unwrap().exited().unwrap() {
        assert!(
            Instant::now() < deadline,
            "actual Native child survived its runner"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[tokio::test]
#[serial_test::serial]
async fn orphaned_runner_self_terminates_when_record_deleted() {
    let name = std::thread::current().name().unwrap().to_owned();
    if managed_runner::child_if_requested(&name).await {
        return;
    }
    let _home = crate::session::test_support::isolate_app_dir();
    let (mut owner, record) = spawn_runner_and_wait_for_record(&name, "s1921");
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        assert!(
            owner.child.try_wait().unwrap().is_none(),
            "runner exited while its record still existed"
        );
        assert!(!owner.native.as_ref().unwrap().exited().unwrap());
        std::thread::sleep(Duration::from_millis(10));
    }
    std::fs::remove_file(&record).unwrap();
    assert_exits_within(
        &mut owner,
        "orphaned runner did not self-terminate after record deletion",
    );
}

#[tokio::test]
#[serial_test::serial]
async fn superseded_runner_exits_without_deleting_replacement_record() {
    let name = std::thread::current().name().unwrap().to_owned();
    if managed_runner::child_if_requested(&name).await {
        return;
    }
    let _home = crate::session::test_support::isolate_app_dir();
    let (mut owner, record) = spawn_runner_and_wait_for_record(&name, "s1922");
    let mut replacement: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    replacement["pid"] = serde_json::json!(owner.child.id().wrapping_add(1_000_000));
    let replacement = serde_json::to_vec(&replacement).unwrap();
    std::fs::write(&record, &replacement).unwrap();
    assert_exits_within(
        &mut owner,
        "superseded runner did not self-terminate after takeover",
    );
    assert_eq!(
        std::fs::read(&record).unwrap(),
        replacement,
        "superseded runner changed the replacement's registry record"
    );
}
