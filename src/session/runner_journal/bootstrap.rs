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
    pub(crate) fn receive(profile: &str, id: &str, nonce: Uuid, generation: u64) -> Result<Self> {
        // Stdin stays open through producer exit, not as native death or vnode authority.
        let input = unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) }.try_clone_to_owned()?;
        let mut channel = UnixStream::from(input);
        let guard = ChildNatalGuard::start(&channel, Instant::now() + NATAL_LIFETIME)?;
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
            }),
            generation,
            births: received.births.into(),
            creations: received.creations.into(),
            create_coverage: received.create_coverage,
        });
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
        let mut reader = channel.try_clone()?;
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
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    anyhow::ensure!(!remaining.is_zero(), "natal authorization deadline expired");
                    reader.set_read_timeout(Some(Duration::from_millis(25)))?;
                    loop {
                        anyhow::ensure!(
                            !deadline.saturating_duration_since(Instant::now()).is_zero(),
                            "natal authorization deadline expired"
                        );
                        if lock
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .released
                        {
                            anyhow::bail!("natal bootstrap was released");
                        }
                        let mut byte = [0];
                        match reader.read(&mut byte) {
                            Ok(1) if byte == [1] => return Ok(()),
                            Ok(0) => anyhow::bail!("execution authorization closed"),
                            Ok(_) => anyhow::bail!("execution authorization was refused"),
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::Interrupted
                                        | std::io::ErrorKind::WouldBlock
                                        | std::io::ErrorKind::TimedOut
                                ) => {}
                            Err(error) => {
                                return Err(error).context("execution authorization closed")
                            }
                        }
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
        self.disarm();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert_eq!(result.is_ok(), authorization == Some(1));
            assert!(
                !guard.state.0.lock().unwrap().released,
                "reading authorization must not disarm the fence watchdog"
            );
            guard.disarm();
        }
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
