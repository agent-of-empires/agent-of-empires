use std::process::{Child, Command};

pub(super) fn configure_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    cmd.process_group(0);
}

pub(super) fn terminate_process_group(child: &Child) {
    signal_process_group(child, nix::sys::signal::Signal::SIGTERM);
}

pub(super) fn kill_process_group(child: &Child) {
    signal_process_group(child, nix::sys::signal::Signal::SIGKILL);
}

fn signal_process_group(child: &Child, signal: nix::sys::signal::Signal) {
    let Ok(pid) = i32::try_from(child.id()) else {
        return;
    };
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), signal);
}

use anyhow::Result;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

// Cancellation sets released permanently and wakes this original socket with shutdown(Read).
pub(crate) fn receive_natal_authorization_until(
    channel: &UnixStream,
    deadline: std::time::Instant,
    mut released: impl FnMut() -> bool,
) -> std::io::Result<Option<u8>> {
    use std::io::{Error, ErrorKind};
    use std::os::fd::AsRawFd;
    use std::time::Instant;

    let mut terminal_readiness = false;
    loop {
        if released() {
            return Err(ErrorKind::Interrupted.into());
        }
        if Instant::now() >= deadline {
            return Err(ErrorKind::TimedOut.into());
        }
        match super::platform::receive_natal_authorization_byte(channel) {
            Ok(byte) => {
                if released() {
                    return Err(ErrorKind::Interrupted.into());
                }
                if Instant::now() >= deadline {
                    return Err(ErrorKind::TimedOut.into());
                }
                return Ok(byte);
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if terminal_readiness {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
        if released() {
            return Err(ErrorKind::Interrupted.into());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(ErrorKind::TimedOut.into());
        }
        // Round poll up to milliseconds; the Instant still bounds authorization.
        let milliseconds =
            remaining.as_millis() + u128::from(remaining.subsec_nanos() % 1_000_000 != 0);
        let timeout = milliseconds.min(libc::c_int::MAX as u128) as libc::c_int;
        let mut event = libc::pollfd {
            fd: channel.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: event remains writable and channel keeps the descriptor owned.
        let ready = unsafe { libc::poll(&mut event, 1, timeout) };
        if ready < 0 {
            let error = Error::last_os_error();
            if error.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            continue;
        }
        if event.revents & libc::POLLNVAL != 0 {
            return Err(Error::from_raw_os_error(libc::EBADF));
        }
        if event.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
            return Err(ErrorKind::Other.into());
        }
        terminal_readiness = event.revents & (libc::POLLHUP | libc::POLLERR) != 0;
    }
}

pub(crate) fn receive_bootstrap_descriptors<const N: usize>(
    channel: &UnixStream,
) -> Result<[OwnedFd; N]> {
    #[repr(C)]
    struct Ancillary<const N: usize> {
        header: libc::cmsghdr,
        descriptors: [RawFd; N],
    }
    let mut ancillary = std::mem::MaybeUninit::<Ancillary<N>>::zeroed();
    let mut marker = [0u8];
    let mut payload = libc::iovec {
        iov_base: marker.as_mut_ptr().cast(),
        iov_len: marker.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut payload;
    message.msg_iovlen = 1;
    message.msg_control = ancillary.as_mut_ptr().cast();
    message.msg_controllen = std::mem::size_of::<Ancillary<N>>() as _;
    // SAFETY: the buffers remain live for the complete platform receive.
    let received = unsafe { super::platform::receive_bootstrap_rights(channel, &mut message)? };
    let mut descriptors: [Option<OwnedFd>; N] = std::array::from_fn(|_| None);
    let mut count = 0;
    let mut unexpected = false;
    // Darwin may retain the sender's cmsg_len after truncating delivered rights.
    let base = message.msg_control as usize;
    let delivered = (message.msg_controllen as usize).min(std::mem::size_of::<Ancillary<N>>());
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
        received == 1 && message.msg_flags & libc::MSG_CTRUNC == 0 && count == N && !unexpected,
        "original descriptor transfer was truncated or malformed"
    );
    anyhow::ensure!(marker == [1], "original descriptor transfer marker differs");
    let descriptors = descriptors.map(|fd| fd.expect("validated original descriptor count"));
    for fd in &descriptors {
        nix::fcntl::fcntl(
            fd,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        )?;
    }
    Ok(descriptors)
}
