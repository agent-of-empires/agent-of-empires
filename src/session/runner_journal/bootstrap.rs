use super::{ExecutionPlan, LaunchOrigin, LaunchPlan, NativeBirthKey, RegistryWitness};
use crate::acp::runner_lifecycle::{ExecutionAdmission as RunnerAdmission, RunnerIdentity};
use crate::process::worker_registry::{self, BoundEndpoint, WorkerRecord};
use crate::session::storage::{DirectoryIdentity, StorageFlock};
use crate::session::Storage;
use anyhow::{Context, Result};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fs::File;
use std::io::{IoSlice, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Serialize)]
struct OriginalBootstrap<'a> {
    profile: &'a str,
    execution: &'a ExecutionPlan,
    pre_trash_project_path: Option<&'a str>,
    scratch: bool,
    births: &'a [NativeBirthKey],
    born: RunnerIdentity,
    creations: &'a [super::native_create::CreateExecution],
    create_coverage: super::CreationCoverage,
    fences: [DirectoryIdentity; 3],
}

#[derive(Deserialize)]
struct ReceivedBootstrap {
    profile: String,
    execution: ExecutionPlan,
    #[serde(deserialize_with = "Option::deserialize")]
    pre_trash_project_path: Option<String>,
    scratch: bool,
    births: Vec<NativeBirthKey>,
    born: RunnerIdentity,
    creations: Vec<super::native_create::CreateExecution>,
    create_coverage: super::CreationCoverage,
    fences: [DirectoryIdentity; 3],
}

#[derive(Serialize, Deserialize)]
pub(super) struct NatalPublication {
    born: RunnerIdentity,
    endpoint: worker_registry::SocketEndpointIdentity,
    registry: RegistryWitness,
}

/// Inherited channel custody, started before the runtime and passed unchanged to receive.
pub struct RunnerNatalGuard {
    channel: UnixStream,
    guard: ChildNatalGuard,
}

impl RunnerNatalGuard {
    pub fn from_inherited_channel() -> Result<Self> {
        let input = unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) }.try_clone_to_owned()?;
        let channel = UnixStream::from(input);
        channel
            .peer_addr()
            .context("ManagedLaunch stdin is not its inherited socket")?;
        let guard = ChildNatalGuard::start(&channel, Instant::now() + NATAL_LIFETIME)?;
        let natal = Self { channel, guard };
        #[cfg(debug_assertions)]
        natal.hold_for_hosted_proof("pre-receive")?;
        Ok(natal)
    }

    /// A disposable-hosted-only stall; never reads or authorizes the natal channel.
    #[cfg(debug_assertions)]
    pub fn hold_for_hosted_proof(&self, stage: &str) -> Result<()> {
        hold_for_hosted_proof(stage)
    }
}

#[cfg(debug_assertions)]
pub(crate) fn hold_for_hosted_proof(stage: &str) -> Result<()> {
    if std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true")
        || std::env::var("RUNNER_ENVIRONMENT").as_deref() != Ok("github-hosted")
        || !std::env::var("AOE_HOSTED_NATAL_STAGE")
            .is_ok_and(|stages| stages.split(',').any(|selected| selected == stage))
    {
        return Ok(());
    }
    let path = std::env::var_os("AOE_HOSTED_NATAL_BARRIER")
        .context("hosted natal barrier path is absent")?;
    let mut barrier = UnixStream::connect(Path::new(&path))?;
    write_frame(
        &mut barrier,
        &(
            stage,
            crate::process::process_incarnation(std::process::id())?,
        ),
    )?;
    // EOF never authorizes the target.
    let mut release = [0];
    barrier.read_exact(&mut release)?;
    anyhow::ensure!(release == [1], "hosted natal barrier refused");
    Ok(())
}

pub(crate) struct LaunchBootstrap {
    origin: Arc<LaunchOrigin>,
    born: RunnerIdentity,
    // These duplicates share the issuer's OFDs. Closing must not explicitly unlock them.
    fences: Option<[File; 3]>,
    guard: ChildNatalGuard,
    channel: UnixStream,
    publication: Option<NatalPublication>,
}

pub(super) fn publish_original(
    channel: &mut UnixStream,
    original: &LaunchOrigin,
    born: RunnerIdentity,
    fences: [&StorageFlock; 3],
) -> Result<()> {
    send_descriptors(
        channel,
        [
            original.storage().original_profile_fd()?,
            fences[0].as_fd(),
            fences[1].as_fd(),
            fences[2].as_fd(),
        ],
    )?;
    write_frame(
        channel,
        &OriginalBootstrap {
            profile: original.profile(),
            execution: &original.plan.execution,
            pre_trash_project_path: original.plan.pre_trash_project_path.as_deref(),
            scratch: original.plan.scratch,
            births: &original.births,
            born,
            creations: &original.creations,
            create_coverage: original.create_coverage,
            fences: [
                fences[0].file_identity()?,
                fences[1].file_identity()?,
                fences[2].file_identity()?,
            ],
        },
    )?;
    #[cfg(test)]
    if let Some(marker) = std::env::var_os("AOE_HOSTED_NATAL_TRANSFERRED") {
        // Evidence only: the production sender has queued every actual OFD and
        // the complete original goal before the observer may kill the issuer.
        std::fs::write(marker, b"actual descriptors and complete frame sent")?;
    }
    let publication: std::result::Result<NatalPublication, String> = read_frame(channel)?;
    let publication = publication.map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        publication.born == born,
        "natal ACK replaced the issued birth"
    );
    original.storage().verify_profile_identity()?;
    let row = original
        .storage()
        .load_strict_for_worktree_ownership_locked()?
        .into_iter()
        .find(|row| row.id == original.session_id())
        .context("natal ACK's original session disappeared")?;
    original.validate_row(&row)?;
    anyhow::ensure!(
        row.runner_journal.launches().iter().any(|launch| {
            Some(Uuid::from_bytes(launch.nonce)) == born.launch_nonce
                && Some(launch.boot) == born.boot
                && launch.generation == born.generation
                && launch.incarnation == born.incarnation
                && launch.profile_identity == born.profile_identity
                && launch.stop_endpoint == Some(publication.endpoint)
                && launch.registry.as_ref() == Some(&publication.registry)
        }),
        "producer ACK differs from its canonical natal publication"
    );
    Ok(())
}

impl LaunchBootstrap {
    pub(crate) fn receive(
        profile: &str,
        id: &str,
        nonce: Uuid,
        generation: u64,
        natal: RunnerNatalGuard,
    ) -> Result<Self> {
        // Stdin stays open through producer exit, not as native death or vnode authority.
        let RunnerNatalGuard { mut channel, guard } = natal;
        let [directory, workspace, identity, lifecycle] =
            crate::process::receive_bootstrap_descriptors(&channel)?;
        let received: ReceivedBootstrap = read_frame(&mut channel)?;
        guard.frame_consumed();
        anyhow::ensure!(
            received.profile == profile
                && received.execution.session_id == id
                && received.born.launch_nonce == Some(nonce)
                && received.born.generation == generation
                && received.born.pid == std::process::id()
                && received.born.birth_is_complete()
                && received
                    .born
                    .incarnation
                    .is_some_and(|birth| birth.group == received.born.pid)
                && received.born.boot == super::current_boot()
                && received.born.incarnation
                    == crate::process::process_incarnation(std::process::id())?,
            "bootstrap differs from this issued native runner"
        );
        let _fences = [
            File::from(workspace),
            File::from(identity),
            File::from(lifecycle),
        ];
        for (file, expected) in _fences.iter().zip(received.fences) {
            let metadata = file.metadata()?;
            anyhow::ensure!(
                metadata.is_file() && DirectoryIdentity::from_metadata(&metadata) == expected,
                "bootstrap fence differs from its actual original FD"
            );
        }
        let storage = Storage::adopt_original_profile(
            received.profile,
            File::from(directory),
            received
                .born
                .profile_identity
                .context("issued profile is absent")?,
        )?;
        super::ensure_unique_owner(&storage, id)?;
        let origin = Arc::new(LaunchOrigin {
            plan: Arc::new(LaunchPlan {
                storage: Arc::new(storage),
                execution: received.execution,
                pre_trash_project_path: received.pre_trash_project_path,
                scratch: received.scratch,
            }),
            generation,
            births: received.births.into(),
            creations: received.creations.into(),
            create_coverage: received.create_coverage,
        });
        #[cfg(debug_assertions)]
        hold_for_hosted_proof("storage")?;
        let row = origin
            .storage()
            .load_strict_for_worktree_ownership_locked()?
            .into_iter()
            .find(|row| row.id == id)
            .context("issued session disappeared")?;
        origin.validate_row(&row)?;
        anyhow::ensure!(
            row.runner_journal.launches().iter().any(|launch| {
                launch.nonce == *nonce.as_bytes()
                    && launch.generation == generation
                    && Some(launch.boot) == received.born.boot
                    && launch.incarnation == received.born.incarnation
                    && launch.profile_identity == received.born.profile_identity
            }),
            "bootstrap lacks its canonical original native birth"
        );
        Ok(Self {
            origin,
            born: received.born,
            fences: Some(_fences),
            guard,
            channel,
            publication: None,
        })
    }

    pub(crate) fn origin(&self) -> Arc<LaunchOrigin> {
        self.origin.clone()
    }

    pub(crate) fn identity(&self) -> RunnerIdentity {
        self.born
    }

    pub(crate) fn publish(
        &mut self,
        stop: &BoundEndpoint,
        record: &mut WorkerRecord,
        control_path: &Path,
    ) -> Result<BoundEndpoint> {
        let result = (|| {
            let endpoint = stop.identity();
            anyhow::ensure!(
                endpoint.is_durable(),
                "natal stop endpoint has no durable birth"
            );
            super::record_stop_endpoint_under_original_fences(&self.origin, self.born, endpoint)?;
            let control =
                super::publish_registry_under_original_fences(&self.origin, record, |record| {
                    worker_registry::publish_control_listener(record, control_path)
                })?;
            self.publication = Some(NatalPublication {
                born: self.born,
                endpoint,
                registry: RegistryWitness {
                    record_file_identity: record
                        .record_file_identity
                        .context("natal writer FD is absent")?,
                    control_file_identity: control.identity(),
                    socket_path: record.socket_path.clone(),
                },
            });
            Ok(control)
        })();
        if let Err(error) = &result {
            let refusal: std::result::Result<NatalPublication, String> = Err(format!("{error:#}"));
            let _ = write_frame(&mut self.channel, &refusal);
        }
        result
    }

    pub(crate) async fn await_authorization(mut self) -> Result<()> {
        let publication = self
            .publication
            .as_ref()
            .context("native resources were not published")?;
        write_frame(&mut self.channel, &Ok::<_, String>(publication))?;
        self.guard
            .authorization
            .take()
            .context("authorization reader is absent")?
            .await
            .context("authorization guard closed")??;
        drop(self.fences.take());
        self.guard.disarm();
        Ok(())
    }
}

const BOOTSTRAP_FRAME_LIMIT: usize = 1024 * 1024;
pub(super) const NATAL_LIFETIME: Duration = Duration::from_secs(30);

impl Drop for LaunchBootstrap {
    fn drop(&mut self) {
        drop(self.fences.take());
        self.guard.disarm();
    }
}

pub(super) fn write_frame<T: Serialize>(channel: &mut UnixStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    anyhow::ensure!(
        bytes.len() <= BOOTSTRAP_FRAME_LIMIT,
        "native bootstrap exceeds frame limit"
    );
    let length = u32::try_from(bytes.len()).context("native bootstrap exceeds wire length")?;
    channel.write_all(&length.to_le_bytes())?;
    channel.write_all(&bytes)?;
    Ok(())
}

pub(super) fn read_frame<T: DeserializeOwned>(channel: &mut UnixStream) -> Result<T> {
    let mut length = [0; 4];
    channel.read_exact(&mut length)?;
    let length = u32::from_le_bytes(length) as usize;
    anyhow::ensure!(
        length <= BOOTSTRAP_FRAME_LIMIT,
        "native bootstrap exceeds frame limit"
    );
    let mut bytes = vec![0; length];
    channel.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(super) fn send_descriptors<const N: usize>(
    channel: &UnixStream,
    descriptors: [BorrowedFd<'_>; N],
) -> Result<()> {
    let descriptors = descriptors.map(|fd| fd.as_raw_fd());
    let marker = [IoSlice::new(&[1])];
    let rights = [ControlMessage::ScmRights(&descriptors)];
    let sent = sendmsg::<()>(
        channel.as_raw_fd(),
        &marker,
        &rights,
        MsgFlags::empty(),
        None,
    )?;
    anyhow::ensure!(sent == 1, "original descriptor transfer was incomplete");
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ParentNatalState {
    Pending,
    Finished,
    Interrupted,
}

pub(super) struct ParentNatalGuard {
    state: Arc<(Mutex<ParentNatalState>, Condvar)>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ParentNatalGuard {
    pub(super) fn start(
        channel: &UnixStream,
        admission: RunnerAdmission,
        deadline: Instant,
    ) -> Result<Self> {
        let channel = channel.try_clone()?;
        let state = Arc::new((Mutex::new(ParentNatalState::Pending), Condvar::new()));
        let watched = state.clone();
        let thread = std::thread::Builder::new()
            .name("natal-issuer".into())
            .spawn(move || {
                let (lock, changed) = &*watched;
                let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
                while *state == ParentNatalState::Pending {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() || admission.cancellation_observed() {
                        *state = ParentNatalState::Interrupted;
                        let _ = channel.shutdown(std::net::Shutdown::Both);
                        break;
                    }
                    state = changed
                        .wait_timeout(state, remaining.min(Duration::from_millis(25)))
                        .unwrap_or_else(|error| error.into_inner())
                        .0;
                }
            })?;
        Ok(Self {
            state,
            thread: Some(thread),
        })
    }

    pub(super) fn finish(&mut self) -> Result<()> {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            *state != ParentNatalState::Interrupted,
            "natal deadline or cancellation interrupted startup"
        );
        *state = ParentNatalState::Finished;
        changed.notify_all();
        Ok(())
    }
}

impl Drop for ParentNatalGuard {
    fn drop(&mut self) {
        let _ = self.finish();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Default)]
struct ChildNatalState {
    frame_consumed: bool,
    released: bool,
}

struct ChildNatalGuard {
    state: Arc<(Mutex<ChildNatalState>, Condvar)>,
    channel: UnixStream,
    authorization: Option<tokio::sync::oneshot::Receiver<Result<()>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ChildNatalGuard {
    fn start(channel: &UnixStream, deadline: Instant) -> Result<Self> {
        let reader = channel.try_clone()?;
        let channel = channel.try_clone()?;
        let state = Arc::new((Mutex::new(ChildNatalState::default()), Condvar::new()));
        let watched = state.clone();
        let (send, authorization) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("natal-child".into())
            .spawn(move || {
                let (lock, changed) = &*watched;
                let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
                while !state.frame_consumed && !state.released {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        unsafe { libc::_exit(1) }
                    }
                    state = changed
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|error| error.into_inner())
                        .0;
                }
                if state.released {
                    return;
                }
                drop(state);
                let authorized = (|| -> Result<()> {
                    let byte = crate::process::receive_natal_authorization_until(
                        &reader,
                        deadline,
                        || {
                            lock.lock()
                                .unwrap_or_else(|error| error.into_inner())
                                .released
                        },
                    )
                    .map_err(|error| match error.kind() {
                        std::io::ErrorKind::TimedOut => {
                            anyhow::anyhow!("natal authorization deadline expired")
                        }
                        std::io::ErrorKind::Interrupted => {
                            anyhow::anyhow!("natal bootstrap was released")
                        }
                        _ => anyhow::Error::new(error).context("reading execution authorization"),
                    })?;
                    match byte {
                        Some(1) => Ok(()),
                        None => anyhow::bail!("execution authorization closed"),
                        Some(_) => anyhow::bail!("execution authorization was refused"),
                    }
                })();
                let _ = send.send(authorized);
                let mut state = lock.lock().unwrap_or_else(|error| error.into_inner());
                while !state.released {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        unsafe { libc::_exit(1) }
                    }
                    state = changed
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|error| error.into_inner())
                        .0;
                }
            })?;
        Ok(Self {
            state,
            channel,
            authorization: Some(authorization),
            thread: Some(thread),
        })
    }

    fn frame_consumed(&self) {
        let (lock, changed) = &*self.state;
        lock.lock()
            .unwrap_or_else(|error| error.into_inner())
            .frame_consumed = true;
        changed.notify_all();
    }

    fn disarm(&mut self) {
        let (lock, changed) = &*self.state;
        lock.lock()
            .unwrap_or_else(|error| error.into_inner())
            .released = true;
        changed.notify_all();
        let _ = self.channel.shutdown(std::net::Shutdown::Read);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ChildNatalGuard {
    fn drop(&mut self) {
        if !self
            .state
            .0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .released
        {
            // Armed custody cannot drop while queued SCM_RIGHTS may retain issuer flocks.
            unsafe { libc::_exit(1) }
        }
        self.disarm();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
    #[test]
    #[ignore = "actual ManagedLaunch parent-loss proof; disposable hosted Linux/macOS only"]
    #[serial_test::serial]
    fn hosted_managed_launch_parent_loss_before_ack_releases_only_after_original_death(
    ) -> Result<()> {
        use crate::acp::runner_lifecycle::{
            LifecycleTable, NativeResume, PreparationAuthorization, ResumeKind,
        };
        use crate::session::runner_journal::{capture_unique_origin, ManagedLaunch};
        use std::os::unix::net::UnixListener;
        use std::process::{Command, Stdio};

        const EXACT: &str = "session::runner_journal::bootstrap::tests::hosted_managed_launch_parent_loss_before_ack_releases_only_after_original_death";
        let executable = crate::session::test_support::require_hosted_creating_native();
        if let Some(directory) = std::env::var_os("AOE_HOSTED_NATAL_PRODUCER") {
            let directory = std::path::PathBuf::from(directory);
            let id = std::env::var("AOE_HOSTED_NATAL_ID")?;
            let storage = Storage::new_unwatched("default")?;
            let original = capture_unique_origin(&id)?;
            let table = Mutex::new(LifecycleTable::new(1));
            let lease = table.lock().unwrap().admit(&id, ResumeKind::Spawn)?;
            let admission = table.lock().unwrap().execution_admission(&lease);
            admission.set_origin(original.clone())?;
            let job = admission.begin_job();
            let (prepared, custody) =
                original.prepare(&NativeResume::Spawn, &admission, |commit| {
                    PreparationAuthorization::acquire(
                        table.lock().unwrap(),
                        &lease,
                        &original,
                        false,
                        commit,
                    )
                })?;
            let generation = prepared.generation();
            admission.set_prepared_origin(prepared, custody)?;
            let launch = ManagedLaunch::new(
                crate::session::deletion::SessionPathOwner {
                    profile: "default",
                    session_id: &id,
                },
                generation,
            )?;
            let mut command = Command::new(executable);
            command
                .args(["--profile", "default", "__acp-runner", "--socket"])
                .arg(directory.join("worker.sock"))
                .args(["--session-id", &id, "--agent-name", "/bin/sh", "--cwd"])
                .arg(&directory)
                .arg("--generation")
                .arg(generation.to_string());
            launch.configure(&mut command);
            command
                .args(["--", "/bin/sh", "-c", "printf forbidden > target-executed"])
                .env("AOE_HOSTED_NATAL_BARRIER", directory.join("barrier.sock"))
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            crate::process::configure_process_group(&mut command);
            let hard_wait = std::env::var("AOE_HOSTED_NATAL_HARD_WAIT").as_deref() == Ok("1");
            if hard_wait {
                crate::process::ignore_child_reaping_for_hosted_probe()?;
            }
            let mut observation = None;
            let mut evidence = UnixStream::connect(directory.join("issuer.sock"))?;
            let launched = launch.spawn_child(
                &storage,
                Some(&admission),
                |input| {
                    command.stdin(input);
                    let child = command.spawn()?;
                    command.stdin(Stdio::null());
                    Ok((Some(child.id()), child))
                },
                |child| {
                    std::fs::write(directory.join("retiring"), b"issuer deadline fired")?;
                    hosted_wait_until(
                        || directory.join("retire-release").exists(),
                        Duration::from_secs(10),
                    )?;
                    let waited = (|| -> std::io::Result<_> {
                        if child.try_wait()?.is_none() {
                            child.kill()?;
                        }
                        child.wait()
                    })();
                    if hard_wait {
                        let error = waited.as_ref().expect_err(
                            "SIGCHLD ignore must cause a real original Child wait error",
                        );
                        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
                        std::fs::write(directory.join("root-wait-error"), format!("{error:?}"))?;
                    }
                    let status = waited?;
                    std::fs::write(directory.join("root-reaped"), format!("{status:?}"))?;
                    Ok(())
                },
                |born| {
                    if hard_wait {
                        observation = Some(
                            crate::process::OriginalRootDeathObservation::bind(
                                born.incarnation.unwrap(),
                            )
                            .unwrap(),
                        );
                    }
                    let issued = admission.origin().unwrap();
                    write_frame(
                        &mut evidence,
                        &(
                            born,
                            &issued.plan.execution,
                            issued.births.as_ref(),
                            issued.creations.as_ref(),
                            issued.create_coverage,
                        ),
                    )
                    .unwrap();
                },
            );
            if hard_wait {
                let error = match launched {
                    Err(error) => error,
                    Ok(_) => {
                        anyhow::bail!("original hard wait error unexpectedly returned a child")
                    }
                };
                anyhow::ensure!(
                    directory.join("root-wait-error").exists()
                        && format!("{error:#}").contains("original root retirement unproven"),
                    "actual ECHILD was not retained by ManagedLaunch: {error:#}"
                );
                let observation =
                    observation.context("original observer was not bound before publication")?;
                hosted_wait_until(|| observation.exited().unwrap(), Duration::from_secs(5))?;
                std::fs::write(
                    directory.join("hard-wait-returned"),
                    format!(
                        "{error:#}
{admission:?}"
                    ),
                )?;
                hosted_wait_until(
                    || directory.join("issuer-release").exists(),
                    Duration::from_secs(10),
                )?;
                drop(job);
                return Ok(());
            }
            let (_, _, published) = launched?;
            assert!(
                published.is_err(),
                "a stalled pre-ACK child cannot receive target approval"
            );
            drop(job);
            return Ok(());
        }

        for (stages, kill_issuer, hard_wait) in [
            ("pre-receive,runner-logging", true, false),
            ("main-logging", true, false),
            ("transition", true, false),
            ("storage", true, false),
            ("pre-receive", false, false),
            ("pre-receive", false, true),
        ] {
            let home = tempfile::tempdir_in("/tmp")?;
            let _home = crate::session::test_support::isolate_home(home.path());
            crate::migrations::run_migrations()?;
            let storage = Arc::new(Storage::new_unwatched("default")?);
            let mut row = crate::session::Instance::new(
                "hosted natal parent loss",
                home.path().to_str().unwrap(),
            );
            row.view = crate::session::View::Structured;
            row.command = "/bin/sh".into();
            row.extra_args = "-c 'printf forbidden > target-executed'".into();
            storage.update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })?;
            let expected_goal = serde_json::json!({
                "session_id": row.id,
                "created_at": row.created_at,
                "project_path": row.project_path,
                "worktree": row.worktree_info,
                "workspace": row.workspace_info,
                "sandbox": row.sandbox_info,
                "command": row.command,
                "extra_args": row.extra_args,
                "tool": row.tool,
                "detect_as": row.detect_as,
                "yolo_mode": row.yolo_mode,
                "agent_provider": row.agent_provider,
                "first_launch_names_agent": row.first_launch_names_agent,
                "active_execution": row.active_execution,
                "title": row.title,
                "archived": row.is_archived(),
                "trashed": row.is_trashed(),
            });
            let original_profile = storage.original_profile_identity()?;
            assert!(original_profile.is_durable());
            let issuer_listener = UnixListener::bind(home.path().join("issuer.sock"))?;
            let barrier_listener = UnixListener::bind(home.path().join("barrier.sock"))?;
            issuer_listener.set_nonblocking(true)?;
            barrier_listener.set_nonblocking(true)?;
            let started = Instant::now();
            let child = Command::new(std::env::current_exe()?)
                .args([
                    EXACT,
                    "--exact",
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("AOE_HOSTED_NATAL_PRODUCER", home.path())
                .env("AOE_HOSTED_NATAL_ID", &row.id)
                .env("AOE_HOSTED_NATAL_STAGE", stages)
                .env(
                    "AOE_HOSTED_NATAL_HARD_WAIT",
                    if hard_wait { "1" } else { "0" },
                )
                .env(
                    "AOE_HOSTED_NATAL_TRANSFERRED",
                    home.path().join("transferred"),
                )
                .spawn()?;
            let mut issuer = HostedIssuer(child);
            let mut evidence = hosted_accept(&issuer_listener)?;
            evidence.set_read_timeout(Some(Duration::from_secs(10)))?;
            let (born, execution, births, creations, coverage): (
                RunnerIdentity,
                serde_json::Value,
                Vec<NativeBirthKey>,
                Vec<super::super::native_create::CreateExecution>,
                super::super::CreationCoverage,
            ) = read_frame(&mut evidence)?;
            assert!(born.birth_is_complete());
            assert_eq!(born.profile_identity, Some(original_profile));
            assert_eq!(execution, expected_goal);
            assert_eq!(
                serde_json::to_value(creations)?,
                serde_json::to_value(&row.runner_journal.creations)?
            );
            assert_eq!(coverage, row.runner_journal.create_coverage);
            assert_eq!(births.len(), 1);
            assert!(births
                .iter()
                .any(|birth| birth.incarnation == born.incarnation));
            let birth = born
                .incarnation
                .context("issuer did not capture its actual original root")?;
            let death = crate::process::OriginalRootDeathObservation::bind(birth)?;
            hosted_wait_until(
                || home.path().join("transferred").exists(),
                Duration::from_secs(10),
            )?;
            let mut barrier = hosted_accept(&barrier_listener)?;
            barrier.set_read_timeout(Some(Duration::from_secs(10)))?;
            let (stage, actual): (String, Option<crate::process::ProcessIncarnation>) =
                read_frame(&mut barrier)?;
            assert_eq!(stage, stages.split(',').next().unwrap());
            assert_eq!(actual, Some(birth));
            let first_barrier = Instant::now();
            assert!(first_barrier.duration_since(started) < Duration::from_secs(10));
            assert!(!death.exited()?);
            assert_eq!(hosted_fence_state(&storage, &row.id)?, [true; 3]);
            assert!(!home.path().join("target-executed").exists());

            if stages.contains(',') {
                std::thread::sleep(Duration::from_secs(8));
                barrier.write_all(&[1])?;
                barrier = hosted_accept(&barrier_listener)?;
                barrier.set_read_timeout(Some(Duration::from_secs(10)))?;
                let (stage, actual): (String, Option<crate::process::ProcessIncarnation>) =
                    read_frame(&mut barrier)?;
                assert_eq!(stage, "runner-logging");
                assert_eq!(actual, Some(birth));
            }
            if kill_issuer {
                issuer.0.kill()?;
                assert!(!issuer.0.wait()?.success());
                assert!(!death.exited()?);
                assert_eq!(
                    hosted_fence_state(&storage, &row.id)?,
                    [true; 3],
                    "orphan must retain actual transferred/queued OFDs"
                );
            } else {
                hosted_wait_until(
                    || home.path().join("retiring").exists(),
                    Duration::from_secs(35),
                )?;
                assert!(started.elapsed() >= NATAL_LIFETIME - Duration::from_secs(1),
                    "real issuer watchdog must expire at the absolute 30s deadline, not an unrelated early refusal");
                assert_eq!(
                    hosted_fence_state(&storage, &row.id)?,
                    [true; 3],
                    "issuer fences cannot release before its original root reaper"
                );
                std::fs::write(
                    home.path().join("retire-release"),
                    b"retire actual original Child",
                )?;
                if hard_wait {
                    hosted_wait_until(
                        || home.path().join("hard-wait-returned").exists(),
                        Duration::from_secs(5),
                    )?;
                    hosted_wait_until(|| death.exited().unwrap(), Duration::from_secs(5))?;
                    assert!(
                        issuer.0.try_wait()?.is_none(),
                        "issuer observation window must remain live"
                    );
                    assert_eq!(hosted_fence_state(&storage, &row.id)?, [true; 3], "C2 before: actual ECHILD loses reachable fence custody after original root terminal");
                    assert!(!home.path().join("target-executed").exists());
                    println!("C2 BEFORE actual original ECHILD; root terminal observed by same prebound pidfd/kqueue; all original fences remain stuck; original ID={} DOB={} g={} PFD={original_profile:?} goal={execution:?}; {}", row.id, row.created_at, born.generation, std::fs::read_to_string(home.path().join("hard-wait-returned"))?);
                    std::fs::write(
                        home.path().join("issuer-release"),
                        b"end original observation window",
                    )?;
                } else {
                    hosted_wait_until(
                        || home.path().join("root-reaped").exists(),
                        Duration::from_secs(5),
                    )?;
                }
                assert!(issuer.0.wait()?.success());
            }
            hosted_wait_until(|| death.exited().unwrap(), Duration::from_secs(35))?;
            assert!(
                first_barrier.elapsed() <= NATAL_LIFETIME + Duration::from_secs(2),
                "the child lifetime is absolute and started before receive/logging"
            );
            hosted_wait_until(
                || hosted_fence_state(&storage, &row.id).unwrap() == [false; 3],
                Duration::from_secs(2),
            )?;
            assert!(!home.path().join("target-executed").exists());
            storage.verify_profile_identity()?;
            let canonical = storage
                .load()?
                .into_iter()
                .find(|current| current.id == row.id)
                .unwrap();
            assert_eq!(canonical.created_at, row.created_at);
            assert_eq!(canonical.lifecycle_generation, born.generation);
            assert!(
                !canonical.runner_journal.proves_runner_quiescent(),
                "root death/reap cannot fabricate original preparation or group quiescence ACK"
            );
            assert!(canonical
                .runner_journal
                .launches()
                .iter()
                .any(|launch| launch.incarnation == Some(birth)
                    && launch.generation == born.generation
                    && Some(Uuid::from_bytes(launch.nonce)) == born.launch_nonce
                    && launch.profile_identity == Some(original_profile)
                    && launch.stop_endpoint.is_none()
                    && launch.registry.is_none()));
            println!("hosted ManagedLaunch: {stages}; parent_loss={kill_issuer}; actual original death precedes fence release; no target effect; unresolved durable scope retained");
        }
        Ok(())
    }

    #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
    struct HostedIssuer(std::process::Child);

    #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
    impl Drop for HostedIssuer {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
    fn hosted_accept(listener: &std::os::unix::net::UnixListener) -> Result<UnixStream> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    // Darwin inherits the listener's nonblocking mode.
                    stream.set_nonblocking(false)?;
                    return Ok(stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    anyhow::ensure!(
                        Instant::now() < deadline,
                        "hosted native barrier was not reached"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
    fn hosted_wait_until(mut predicate: impl FnMut() -> bool, lifetime: Duration) -> Result<()> {
        let deadline = Instant::now() + lifetime;
        while !predicate() {
            anyhow::ensure!(Instant::now() < deadline, "hosted native proof timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    #[cfg(all(debug_assertions, any(target_os = "linux", target_os = "macos")))]
    fn hosted_fence_state(storage: &Storage, id: &str) -> Result<[bool; 3]> {
        let app = crate::session::get_app_dir()?;
        Ok([
            crate::session::try_acquire_storage_flock(&app, ".workspace-claim.lock")?.is_none(),
            crate::session::try_acquire_storage_flock(&app, ".title-mutation.lock")?.is_none(),
            storage.instance_lifecycle_lock_is_held_for_test(id),
        ])
    }

    #[test]
    fn transferred_fences_stay_locked_and_truncated_rights_close() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let profile = File::open(directory.path())?;
        let fences = [
            crate::session::acquire_storage_flock(directory.path(), "workspace")?,
            crate::session::acquire_storage_flock(directory.path(), "identity")?,
            crate::session::acquire_storage_flock(directory.path(), "lifecycle")?,
        ];
        let (sender, receiver) = UnixStream::pair()?;
        send_descriptors(
            &sender,
            [
                profile.as_fd(),
                fences[0].as_fd(),
                fences[1].as_fd(),
                fences[2].as_fd(),
            ],
        )
        .context("sending original descriptors")?;
        let [received_profile, workspace, identity, lifecycle] =
            crate::process::receive_bootstrap_descriptors(&receiver)
                .context("receiving original descriptors")?;
        assert_eq!(
            DirectoryIdentity::from_metadata(&File::from(received_profile).metadata()?),
            DirectoryIdentity::from_metadata(&profile.metadata()?)
        );
        for fd in [&workspace, &identity, &lifecycle] {
            assert!(nix::fcntl::FdFlag::from_bits_truncate(
                nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFD)
                    .context("reading received descriptor flags")?
            )
            .contains(nix::fcntl::FdFlag::FD_CLOEXEC));
        }
        drop([workspace, identity, lifecycle]);
        for name in ["workspace", "identity", "lifecycle"] {
            assert!(
                crate::session::try_acquire_storage_flock(directory.path(), name)
                    .context("checking retained issuer fence")?
                    .is_none(),
                "received close must not unlock the issuer's OFD"
            );
        }
        drop(fences);
        for name in ["workspace", "identity", "lifecycle"] {
            assert!(
                crate::session::try_acquire_storage_flock(directory.path(), name)
                    .context("checking released issuer fence")?
                    .is_some()
            );
        }

        let (transferred, mut peer) = UnixStream::pair()?;
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .context("setting close-witness deadline")?;
        let descriptors = [transferred.as_raw_fd(); 5];
        sendmsg::<()>(
            sender.as_raw_fd(),
            &[IoSlice::new(&[1])],
            &[ControlMessage::ScmRights(&descriptors)],
            MsgFlags::empty(),
            None,
        )
        .context("sending oversized descriptor transfer")?;
        drop(transferred);
        assert!(crate::process::receive_bootstrap_descriptors::<4>(&receiver).is_err());
        assert_eq!(
            peer.read(&mut [0])
                .context("reading rejected-transfer close witness")?,
            0,
            "rejected SCM_RIGHTS must leave no socket descriptor open"
        );
        Ok(())
    }

    #[test]
    fn bootstrap_frames_reject_oversize_before_body_or_prefix() -> Result<()> {
        let (mut sender, mut receiver) = UnixStream::pair()?;
        sender.write_all(&u32::MAX.to_le_bytes())?;
        receiver.set_read_timeout(Some(Duration::from_secs(1)))?;
        let error = read_frame::<serde_json::Value>(&mut receiver).unwrap_err();
        assert!(error.to_string().contains("frame limit"));
        receiver.set_nonblocking(true)?;
        let oversized = "x".repeat(BOOTSTRAP_FRAME_LIMIT);
        assert!(write_frame(&mut sender, &oversized)
            .unwrap_err()
            .to_string()
            .contains("frame limit"));
        assert_eq!(
            receiver.read(&mut [0]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        receiver.set_nonblocking(false)?;
        let exact = "x".repeat(BOOTSTRAP_FRAME_LIMIT - 2);
        let writer = std::thread::spawn(move || write_frame(&mut sender, &exact));
        let received: String = read_frame(&mut receiver)?;
        writer.join().expect("frame writer panicked")?;
        assert_eq!(received.len(), BOOTSTRAP_FRAME_LIMIT - 2);
        Ok(())
    }

    #[tokio::test]
    async fn child_guard_reads_only_after_frame_and_keeps_buffered_authorization() -> Result<()> {
        for authorization in [Some(1), Some(0), None] {
            let (mut sender, mut receiver) = UnixStream::pair()?;
            let mut guard = ChildNatalGuard::start(&receiver, Instant::now() + NATAL_LIFETIME)?;
            write_frame(&mut sender, &"complete execution goal")?;
            if let Some(byte) = authorization {
                sender.write_all(&[byte])?;
            }
            drop(sender);
            assert_eq!(
                read_frame::<String>(&mut receiver)?,
                "complete execution goal"
            );
            guard.frame_consumed();
            let result =
                tokio::time::timeout(Duration::from_secs(2), guard.authorization.take().unwrap())
                    .await??;
            let still_armed = !guard.state.0.lock().unwrap().released;
            guard.disarm();
            assert_eq!(
                result.is_ok(),
                authorization == Some(1),
                "buffered authorization outcome: {result:#?}"
            );
            assert!(
                still_armed,
                "reading authorization must not disarm the fence watchdog"
            );
        }
        Ok(())
    }

    #[test]
    fn expired_or_released_authorization_keeps_queued_byte_unconsumed() -> Result<()> {
        for released in [false, true] {
            let (mut sender, mut receiver) = UnixStream::pair()?;
            sender.write_all(&[1])?;
            let deadline = if released {
                Instant::now() + NATAL_LIFETIME
            } else {
                Instant::now()
            };
            let error =
                crate::process::receive_natal_authorization_until(&receiver, deadline, || released)
                    .unwrap_err();
            assert_eq!(
                error.kind(),
                if released {
                    std::io::ErrorKind::Interrupted
                } else {
                    std::io::ErrorKind::TimedOut
                }
            );
            let mut byte = [0];
            receiver.read_exact(&mut byte)?;
            assert_eq!(byte, [1]);
        }
        Ok(())
    }

    #[test]
    fn child_disarm_wakes_original_channel_before_its_deadline() -> Result<()> {
        let (_sender, receiver) = UnixStream::pair()?;
        let mut guard =
            ChildNatalGuard::start(&receiver, Instant::now() + Duration::from_secs(30))?;
        guard.frame_consumed();
        let (done, completed) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            guard.disarm();
            done.send(()).unwrap();
        });
        completed.recv_timeout(Duration::from_secs(5))?;
        thread.join().unwrap();
        Ok(())
    }

    #[test]
    fn issuer_guard_interrupts_ipc_on_cancellation_and_absolute_deadline() -> Result<()> {
        for cancel in [true, false] {
            let admission = RunnerAdmission::new();
            let (mut sender, _receiver) = UnixStream::pair()?;
            let deadline = if cancel {
                Instant::now() + NATAL_LIFETIME
            } else {
                Instant::now()
            };
            let mut guard = ParentNatalGuard::start(&sender, admission.clone(), deadline)?;
            sender.set_read_timeout(Some(Duration::from_secs(2)))?;
            if cancel {
                admission.cancel();
            }
            assert_eq!(sender.read(&mut [0])?, 0);
            assert!(guard.finish().is_err());
        }
        Ok(())
    }
}
