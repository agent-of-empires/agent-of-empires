use super::environment::EnvGuard;
use crate::acp::runner_lifecycle::{
    ExecutionAdmission, ExecutionJob, LifecycleTable, NativeResume, PreparationAuthorization,
    ResumeKind,
};
use crate::session::runner_journal::{capture_unique_origin, ManagedLaunch};
use crate::session::{Instance, Storage};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) struct RunnerLaunchFixture {
    storage: Storage,
    admission: ExecutionAdmission,
    job: RefCell<Option<ExecutionJob>>,
    launch: RefCell<Option<ManagedLaunch>>,
    retired: tokio::sync::watch::Receiver<Option<bool>>,
    generation: u64,
    pub(crate) nonce: uuid::Uuid,
    _env: EnvGuard,
}

impl RunnerLaunchFixture {
    pub(crate) fn new(home: &Path, xdg: &Path, profile: &str, id: &str) -> Self {
        assert!(
            std::env::var_os("AOE_ISOLATED_RUNNER_TEST").is_some(),
            "native fixture needs an isolated scenario process"
        );
        let env = EnvGuard::new(&["HOME", "XDG_CONFIG_HOME"])
            .and_set("HOME", home)
            .and_set("XDG_CONFIG_HOME", xdg);
        crate::session::get_app_dir().expect("isolated app directory");
        crate::migrations::run_migrations().expect("migrate original fixture metadata");
        let storage = Storage::new_unwatched(profile).expect("original fixture profile");
        storage
            .update(|rows, _| {
                if !rows.iter().any(|row| row.id == id) {
                    let mut row =
                        Instance::new("runner fixture", home.to_str().expect("fixture home UTF-8"));
                    row.id = id.to_owned();
                    row.view = crate::session::View::Structured;
                    rows.push(row);
                }
                Ok(())
            })
            .expect("canonical fixture owner");
        let original = capture_unique_origin(id).expect("original fixture scope");
        let table = Mutex::new(LifecycleTable::new(1));
        let lease = table
            .lock()
            .unwrap()
            .admit(id, ResumeKind::Spawn)
            .expect("native admission");
        let admission = table.lock().unwrap().execution_admission(&lease);
        admission
            .set_origin(original.clone())
            .expect("original admission");
        let job = admission.begin_job();
        let (prepared, custody) = original
            .prepare(&NativeResume::Spawn, &admission, |commit| {
                PreparationAuthorization::acquire(
                    table.lock().unwrap(),
                    &lease,
                    &original,
                    false,
                    commit,
                )
            })
            .expect("actual preparation ACK");
        let generation = prepared.generation();
        let retired = custody.retirement().clone();
        admission
            .set_prepared_origin(prepared, custody)
            .expect("prepared original custody");
        let launch = ManagedLaunch::new(
            crate::session::deletion::SessionPathOwner {
                profile,
                session_id: id,
            },
            generation,
        )
        .expect("original issuer");
        let nonce = launch.nonce();
        Self {
            storage,
            admission,
            job: RefCell::new(Some(job)),
            launch: RefCell::new(Some(launch)),
            retired,
            generation,
            nonce,
            _env: env,
        }
    }

    pub(crate) fn original_storage(&self) -> &Storage {
        &self.storage
    }

    pub(crate) fn command(&self) -> Command {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(super::aoe_binary());
        command.arg("__acp-runner").process_group(0);
        self.launch
            .borrow()
            .as_ref()
            .expect("unconsumed original issuer")
            .configure(&mut command);
        command.arg("--generation").arg(self.generation.to_string());
        command
    }

    pub(crate) fn spawn(&self, command: &mut Command) -> std::io::Result<Child> {
        let launch = self
            .launch
            .borrow_mut()
            .take()
            .expect("single original launch");
        let result = launch.spawn_owned(&self.storage, command, &self.admission);
        drop(self.job.borrow_mut().take());
        let result = result.map_err(std::io::Error::other);
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.retired.borrow().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut child = result?;
        if *self.retired.borrow() != Some(true) {
            if child.try_wait()?.is_none() {
                crate::process::kill_process_tree(child.id());
            }
            let _ = child.wait();
            return Err(std::io::Error::other(
                "original preparation retirement has no successful canonical ACK",
            ));
        }
        Ok(child)
    }

    pub(crate) fn produced_origin(
        &self,
    ) -> std::sync::Arc<crate::session::runner_journal::LaunchOrigin> {
        self.admission.origin().expect("original producer output")
    }
}

#[cfg(debug_assertions)]
use super::shim::{shim_node, shim_path};

#[cfg(debug_assertions)]
pub(super) async fn spawn_runner_with_shim(
    session_id: &str,
    env: &[(&str, String)],
) -> (PathBuf, RunnerGuard) {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let home = temp.path().join("home");
    let xdg = temp.path().join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    // The control handshake verifies the announced session id.
    let socket_path = temp.path().join(format!("{session_id}.sock"));
    let control = temp.path().join(format!("{session_id}.control.sock"));

    let launch = RunnerLaunchFixture::new(&home, &xdg, "main", session_id);
    let mut cmd = launch.command();
    cmd.args([
        "--socket",
        socket_path.to_str().unwrap(),
        "--session-id",
        session_id,
        "--agent-name",
        "shim",
        "--cwd",
        home.to_str().unwrap(),
        "--",
        shim_node().expect("shim prerequisite").to_str().unwrap(),
        shim_path().to_str().unwrap(),
    ])
    .env("HOME", &home)
    .env("XDG_CONFIG_HOME", &xdg);
    for (k, v) in env {
        cmd.env(k, v);
    }
    // Preseeded sessions need one initial load before testing a later attach.
    if env.iter().any(|(key, _)| *key == "SHIM_PRESEED_SESSION_ID") {
        cmd.env("SHIM_LOAD_SESSION", "1");
    }
    let child = launch.spawn(&mut cmd).expect("spawn original acp runner");

    // The runner binds the control socket before spawning the agent, so its
    // appearance is the readiness signal the daemon's own probe uses.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !control.exists() {
        assert!(
            Instant::now() < deadline,
            "runner never bound {}",
            control.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Resume attaches to an established runner, not merely a preseeded agent.
    // Prime the runner cache exactly as the original daemon would have done.
    {
        use crate::acp::control_protocol::{self, ControlBody};
        let mut initial = tokio::net::UnixStream::connect(&control).await.unwrap();
        assert!(matches!(
            control_protocol::read_frame(&mut initial).await.unwrap(),
            Some(ControlBody::Hello { .. })
        ));
        control_protocol::write_frame(
            &mut initial,
            &ControlBody::Attach {
                control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
            },
        )
        .await
        .unwrap();
        control_protocol::write_frame(
            &mut initial,
            &ControlBody::Initialize {
                request: serde_json::json!({"protocolVersion": 1}),
            },
        )
        .await
        .unwrap();
        loop {
            match control_protocol::read_frame(&mut initial).await.unwrap() {
                Some(ControlBody::Initialized { .. }) => break,
                Some(ControlBody::Notify { .. }) => {}
                frame => panic!("initial initialize failed: {frame:?}"),
            }
        }
        let preseed = env
            .iter()
            .find(|(key, _)| *key == "SHIM_PRESEED_SESSION_ID");
        let (method, request) = match preseed {
            Some((_, id)) => (
                "session/load",
                serde_json::json!({"sessionId": id, "cwd": home, "mcpServers": []}),
            ),
            None => (
                "session/new",
                serde_json::json!({"cwd": home, "mcpServers": []}),
            ),
        };
        control_protocol::write_frame(
            &mut initial,
            &ControlBody::EstablishSession {
                method: method.into(),
                request,
            },
        )
        .await
        .unwrap();
        loop {
            match control_protocol::read_frame(&mut initial).await.unwrap() {
                Some(ControlBody::SessionReady { .. }) => break,
                Some(ControlBody::Notify { .. }) => {}
                frame => panic!("initial session establishment failed: {frame:?}"),
            }
        }
    }

    (
        socket_path,
        RunnerGuard {
            _child: child,
            _temp: temp,
            nonce: launch.nonce,
            _fixture: launch,
        },
    )
}

#[cfg(debug_assertions)]
pub(super) struct RunnerGuard {
    _child: Child,
    _fixture: RunnerLaunchFixture,
    _temp: tempfile::TempDir,
    pub(super) nonce: uuid::Uuid,
}
#[cfg(debug_assertions)]
impl Drop for RunnerGuard {
    fn drop(&mut self) {
        if self._child.try_wait().is_ok_and(|status| status.is_none()) {
            crate::process::kill_process_tree(self._child.id());
        }
        let _ = self._child.wait();
    }
}
