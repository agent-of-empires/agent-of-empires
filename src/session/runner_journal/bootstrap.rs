use super::{ExecutionPlan, LaunchOrigin, LaunchPlan, NativeBirthKey, RegistryWitness};
use crate::acp::runner_lifecycle::RunnerIdentity;
use crate::process::worker_registry::{self, BoundEndpoint, WorkerRecord};
use crate::session::storage::{DirectoryIdentity, StorageFlock};
use crate::session::Storage;
use anyhow::{Context, Result};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::fs::File;
use std::io::{IoSlice, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Serialize)]
struct OriginalBootstrap<'a> {
    profile: &'a str,
    execution: &'a ExecutionPlan,
    births: &'a [NativeBirthKey],
    born: RunnerIdentity,
    fences: [DirectoryIdentity; 3],
}

#[derive(Deserialize)]
struct ReceivedBootstrap {
    profile: String,
    execution: ExecutionPlan,
    births: Vec<NativeBirthKey>,
    born: RunnerIdentity,
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
    _fences: [File; 3],
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
        let [directory, workspace, identity, lifecycle] = receive_descriptors(&channel)?;
        let received: ReceivedBootstrap = read_frame(&mut channel)?;
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
            _fences,
            channel,
            publication: None,
        })
    }

    pub(crate) fn storage(&self) -> &Storage {
        self.origin.storage()
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
        self.channel.set_nonblocking(true)?;
        let mut channel = tokio::net::UnixStream::from_std(self.channel)?;
        let mut authorization = [0];
        tokio::io::AsyncReadExt::read_exact(&mut channel, &mut authorization)
            .await
            .context("execution authorization closed")?;
        anyhow::ensure!(authorization == [1], "execution authorization was refused");
        Ok(())
    }
}

fn write_frame<T: Serialize>(channel: &mut UnixStream, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    let length = u32::try_from(bytes.len()).context("native bootstrap exceeds wire length")?;
    channel.write_all(&length.to_le_bytes())?;
    channel.write_all(&bytes)?;
    Ok(())
}

fn read_frame<T: DeserializeOwned>(channel: &mut UnixStream) -> Result<T> {
    let mut length = [0; 4];
    channel.read_exact(&mut length)?;
    let mut bytes = vec![0; u32::from_le_bytes(length) as usize];
    channel.read_exact(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn send_descriptors(channel: &UnixStream, descriptors: [BorrowedFd<'_>; 4]) -> Result<()> {
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

fn receive_descriptors(channel: &UnixStream) -> Result<[OwnedFd; 4]> {
    #[repr(C)]
    struct Ancillary {
        header: libc::cmsghdr,
        descriptors: [RawFd; 4],
    }
    let mut ancillary = std::mem::MaybeUninit::<Ancillary>::zeroed();
    let mut marker = [0u8];
    let mut payload = libc::iovec {
        iov_base: marker.as_mut_ptr().cast(),
        iov_len: marker.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut payload;
    message.msg_iovlen = 1;
    message.msg_control = ancillary.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of::<Ancillary>() as _;
    #[cfg(target_os = "macos")]
    // SAFETY: marker, payload and ancillary buffers remain live for this call.
    unsafe {
        crate::process::reject_truncated_bootstrap_rights(channel, &mut message)?;
    }
    let received = loop {
        let received = unsafe {
            libc::recvmsg(
                channel.as_raw_fd(),
                &mut message,
                crate::process::BOOTSTRAP_RECV_FLAGS,
            )
        };
        if received >= 0 {
            break received;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    };
    let mut descriptors: [Option<OwnedFd>; 4] = [None, None, None, None];
    let mut count = 0;
    let mut unexpected = false;
    // Darwin may retain the sender's cmsg_len after truncating delivered rights.
    let base = message.msg_control as usize;
    let delivered = (message.msg_controllen as usize).min(std::mem::size_of::<Ancillary>());
    let end = base + delivered;
    let mut control = unsafe { libc::CMSG_FIRSTHDR(&message) };
    while !control.is_null() {
        let position = control as usize;
        if position < base
            || position > end
            || end - position < std::mem::size_of::<libc::cmsghdr>()
        {
            unexpected = true;
            break;
        }
        // SAFETY: the header is in the delivered buffer. Read only complete delivered FDs.
        unsafe {
            let header = libc::CMSG_LEN(0) as usize;
            let declared = (*control).cmsg_len as usize;
            let remaining = end - position;
            if declared < header || remaining < header {
                unexpected = true;
                break;
            }
            if (*control).cmsg_level == libc::SOL_SOCKET && (*control).cmsg_type == libc::SCM_RIGHTS
            {
                let bytes = declared.min(remaining) - header;
                let data = libc::CMSG_DATA(control).cast::<RawFd>();
                for index in 0..bytes / std::mem::size_of::<RawFd>() {
                    let fd = OwnedFd::from_raw_fd(*data.add(index));
                    if let Some(slot) = descriptors.get_mut(count) {
                        *slot = Some(fd);
                    }
                    count += 1;
                }
                unexpected |= bytes % std::mem::size_of::<RawFd>() != 0;
            } else {
                unexpected = true;
            }
            if declared > remaining {
                unexpected = true;
                break;
            }
            control = libc::CMSG_NXTHDR(&message, control);
        }
    }
    anyhow::ensure!(
        received == 1 && message.msg_flags & libc::MSG_CTRUNC == 0 && count == 4 && !unexpected,
        "original descriptor transfer was truncated or malformed"
    );
    anyhow::ensure!(marker == [1], "original descriptor transfer marker differs");
    let descriptors = descriptors.map(|fd| fd.expect("four received descriptors"));
    for fd in &descriptors {
        nix::fcntl::fcntl(
            fd,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        )?;
    }
    Ok(descriptors)
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
        )?;
        let [received_profile, workspace, identity, lifecycle] = receive_descriptors(&receiver)?;
        assert_eq!(
            DirectoryIdentity::from_metadata(&File::from(received_profile).metadata()?),
            DirectoryIdentity::from_metadata(&profile.metadata()?)
        );
        for fd in [&workspace, &identity, &lifecycle] {
            assert!(nix::fcntl::FdFlag::from_bits_truncate(nix::fcntl::fcntl(
                fd,
                nix::fcntl::FcntlArg::F_GETFD
            )?)
            .contains(nix::fcntl::FdFlag::FD_CLOEXEC));
        }
        drop([workspace, identity, lifecycle]);
        for name in ["workspace", "identity", "lifecycle"] {
            assert!(
                crate::session::try_acquire_storage_flock(directory.path(), name)?.is_none(),
                "received close must not unlock the issuer's OFD"
            );
        }
        drop(fences);
        for name in ["workspace", "identity", "lifecycle"] {
            assert!(crate::session::try_acquire_storage_flock(directory.path(), name)?.is_some());
        }

        let (transferred, mut peer) = UnixStream::pair()?;
        let descriptors = [transferred.as_raw_fd(); 5];
        sendmsg::<()>(
            sender.as_raw_fd(),
            &[IoSlice::new(&[1])],
            &[ControlMessage::ScmRights(&descriptors)],
            MsgFlags::empty(),
            None,
        )?;
        drop(transferred);
        assert!(receive_descriptors(&receiver).is_err());
        peer.set_read_timeout(Some(std::time::Duration::from_secs(1)))?;
        assert_eq!(
            peer.read(&mut [0])?,
            0,
            "rejected SCM_RIGHTS must leave no socket descriptor open"
        );
        Ok(())
    }
}
