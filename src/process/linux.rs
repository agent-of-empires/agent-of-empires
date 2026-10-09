//! Linux-specific process utilities.

pub(crate) const HAS_CODEX_MANAGED_PREFERENCES: bool = false;

pub(super) fn receive_natal_authorization_byte(
    channel: &std::os::unix::net::UnixStream,
) -> std::io::Result<Option<u8>> {
    use std::os::fd::AsRawFd;
    let mut byte = [0u8];
    // SAFETY: the original socket remains owned and byte is writable for one byte.
    let received = unsafe {
        libc::recv(
            channel.as_raw_fd(),
            byte.as_mut_ptr().cast(),
            byte.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if received < 0 {
        Err(std::io::Error::last_os_error())
    } else if received == 0 {
        Ok(None)
    } else {
        Ok(Some(byte[0]))
    }
}

// SAFETY: the caller retains writable data and control buffers for the receive.
pub(super) unsafe fn receive_bootstrap_rights(
    channel: &std::os::unix::net::UnixStream,
    message: &mut libc::msghdr,
) -> std::io::Result<isize> {
    use std::os::fd::AsRawFd;
    loop {
        let received =
            unsafe { libc::recvmsg(channel.as_raw_fd(), message, libc::MSG_CMSG_CLOEXEC) };
        if received >= 0 {
            return Ok(received);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(all(test, debug_assertions))]
pub(super) fn ignore_child_reaping_for_hosted_probe() -> std::io::Result<()> {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = libc::SIG_IGN;
    let result = unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub(super) struct OriginalRootDeathObservation(std::os::fd::OwnedFd);

impl OriginalRootDeathObservation {
    pub(super) fn bind(birth: super::ProcessIncarnation) -> anyhow::Result<Self> {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, birth.pid, 0) as i32 };
        anyhow::ensure!(
            fd >= 0,
            "cannot bind original pidfd: {}",
            std::io::Error::last_os_error()
        );
        // pidfd_open returned one newly owned descriptor.
        Ok(Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }))
    }

    pub(super) fn exited(&self) -> anyhow::Result<bool> {
        use std::os::fd::AsRawFd;
        let mut event = libc::pollfd {
            fd: self.0.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut event, 1, 0) };
        anyhow::ensure!(
            result >= 0,
            "original pidfd poll failed: {}",
            std::io::Error::last_os_error()
        );
        anyhow::ensure!(
            event.revents & (libc::POLLERR | libc::POLLNVAL) == 0,
            "original pidfd observation failed"
        );
        Ok(result == 1 && event.revents & libc::POLLIN != 0)
    }
}

pub(super) fn peer_pid_from_connected_socket(stream: &impl std::os::fd::AsFd) -> Option<u32> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    let pid = getsockopt(stream, PeerCredentials).ok()?.pid();
    (pid > 0).then_some(pid as u32)
}
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};

// Owned Create uses a genuinely held bootstrap and follows all native descendants.
pub(super) struct OwnedCreateRoot {
    pid: u32,
    birth: super::ProcessIncarnation,
    diagnostics: bool,
    handles: std::collections::BTreeMap<u32, CreateTraceActor>,
}
impl OwnedCreateRoot {
    pub(super) fn prepare(birth: super::ProcessIncarnation) -> anyhow::Result<Self> {
        let pid = birth.pid;
        anyhow::ensure!(
            super::process_incarnation(pid)? == Some(birth) && birth.group == pid,
            "original held Linux bootstrap birth changed"
        );
        create_trace_begin(pid)?;
        create_assert_single_bootstrap_task(pid)?;
        anyhow::ensure!(
            super::process_incarnation(pid)? == Some(birth),
            "original held Linux bootstrap changed during trace admission"
        );
        Ok(Self {
            pid,
            birth,
            diagnostics: std::env::var_os("AOE_CREATE_SYSCALL_DIAGNOSTICS").as_deref()
                == Some(std::ffi::OsStr::new("1")),
            handles: std::collections::BTreeMap::from([(
                pid,
                CreateTraceActor {
                    trace_pid: pid,
                    fd: create_pidfd(pid)?,
                    birth: Some(birth),
                    pending: None,
                    initial_exit_allowed: true,
                },
            )]),
        })
    }
    pub(super) fn release(&mut self) -> anyhow::Result<()> {
        create_trace_resume(self.pid, 0)
    }
    pub(super) fn retire(
        &mut self,
        child: &mut std::process::Child,
        cancel: &tokio_util::sync::CancellationToken,
        mut admit: impl FnMut(super::CreateObservation) -> anyhow::Result<()>,
    ) -> anyhow::Result<super::CreateRetirement> {
        use anyhow::Context;
        use std::collections::BTreeSet;
        use std::os::fd::AsRawFd;
        use std::os::unix::process::ExitStatusExt;
        anyhow::ensure!(child.id() == self.pid, "original Create root was replaced");
        let mut live = BTreeSet::from([self.pid]);
        let handles = &mut self.handles;
        let mut diagnostics = CreateTraceDiagnostics {
            root: self.birth,
            enabled: self.diagnostics,
            remaining: 64,
            truncated: false,
        };
        let mut observed = Vec::new();
        let mut root_status = None;
        let mut external = false;
        let mut scope_unproven = false;
        while !live.is_empty() {
            if cancel.is_cancelled() {
                for fd in handles.values().filter_map(|actor| actor.fd.as_ref()) {
                    let result = unsafe {
                        libc::syscall(
                            libc::SYS_pidfd_send_signal,
                            fd.as_raw_fd(),
                            libc::SIGKILL,
                            0,
                            0,
                        )
                    };
                    if result == -1
                        && std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                    {
                        return Err(std::io::Error::last_os_error().into());
                    }
                }
            }
            observed.clear();
            observed.extend(live.iter().copied());
            let mut made_progress = false;
            for &pid in &observed {
                let mut status = 0;
                let waited =
                    unsafe { libc::waitpid(pid as i32, &mut status, libc::__WALL | libc::WNOHANG) };
                if waited == 0 {
                    continue;
                }
                if waited == -1 {
                    return Err(std::io::Error::last_os_error().into());
                }
                made_progress = true;
                if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                    let unresolved = handles
                        .get_mut(&pid)
                        .is_some_and(|actor| actor.missing_exit(&diagnostics, "actor_retired"));
                    if unresolved && !scope_unproven {
                        admit(super::CreateObservation::ScopeUnproven)?;
                        scope_unproven = true;
                    }
                    live.remove(&pid);
                    if pid != self.pid {
                        handles.remove(&pid);
                    }
                    if pid == self.pid {
                        root_status = Some(status);
                    }
                    continue;
                }
                anyhow::ensure!(libc::WIFSTOPPED(status), "unrecognized native trace status");
                let event = status >> 16;
                let signal = libc::WSTOPSIG(status);
                if event == libc::PTRACE_EVENT_FORK
                    || event == libc::PTRACE_EVENT_VFORK
                    || event == libc::PTRACE_EVENT_CLONE
                {
                    let mut born: libc::c_ulong = 0;
                    create_ptrace(
                        libc::PTRACE_GETEVENTMSG,
                        pid,
                        0,
                        (&mut born as *mut libc::c_ulong) as usize,
                    )?;
                    let born = u32::try_from(born)?;
                    let birth = super::process_incarnation(born)?
                        .context("held descendant lacks actual kernel birth")?;
                    live.insert(born);
                    handles.insert(
                        born,
                        CreateTraceActor {
                            trace_pid: born,
                            fd: create_pidfd(born)?,
                            birth: Some(birth),
                            pending: None,
                            initial_exit_allowed: true,
                        },
                    );
                    admit(super::CreateObservation::Birth(birth))?;
                } else if event == libc::PTRACE_EVENT_EXEC {
                    let mut former: libc::c_ulong = 0;
                    create_ptrace(
                        libc::PTRACE_GETEVENTMSG,
                        pid,
                        0,
                        (&mut former as *mut libc::c_ulong) as usize,
                    )?;
                    if former != 0 && former != pid as libc::c_ulong {
                        // Thread-group exec changes PID projection; never promote a fresh alias.
                        if let Some(actor) = handles.get_mut(&(former as u32)) {
                            actor.missing_exit(&diagnostics, "exec_pid_projection_changed");
                        }
                        if let Some(actor) = handles.get_mut(&pid) {
                            actor.missing_exit(&diagnostics, "exec_pid_projection_changed");
                            actor.birth = None;
                        }
                        live.remove(&(former as u32));
                        handles.remove(&(former as u32));
                        if !scope_unproven {
                            admit(super::CreateObservation::ScopeUnproven)?;
                            scope_unproven = true;
                        }
                    }
                } else if signal == (libc::SIGTRAP | 0x80) {
                    let actor = handles
                        .get_mut(&pid)
                        .context("held syscall actor lacks original trace state")?;
                    let audit =
                        create_audit_syscall(pid, actor, &mut diagnostics, !scope_unproven)?;
                    if audit.external && !external {
                        admit(super::CreateObservation::ExternalDomain)?;
                        external = true;
                    }
                    if audit.unproven && !scope_unproven {
                        admit(super::CreateObservation::ScopeUnproven)?;
                        scope_unproven = true;
                    }
                }
                if cancel.is_cancelled() {
                    create_ptrace(libc::PTRACE_KILL, pid, 0, 0)?;
                } else {
                    let delivery = if event != 0
                        || signal == (libc::SIGTRAP | 0x80)
                        || signal == libc::SIGSTOP
                    {
                        0
                    } else {
                        signal
                    };
                    create_trace_resume(pid, delivery)?;
                }
            }
            if !made_progress {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        // Keep this original producer until every original group member is gone.
        // Uncovered/foreign members are never signalled; observation failure is not absence.
        while process_group_has_live_members(self.pid)? {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let raw_status =
            root_status.context("original native root never returned its actual exit status")?;
        Ok(super::CreateRetirement {
            status: std::process::ExitStatus::from_raw(raw_status),
            raw_status,
            descendants_retired: !scope_unproven,
            group_retired: true,
            external_domain: external,
        })
    }
}
pub(super) fn hold_owned_create_bootstrap() -> anyhow::Result<()> {
    unsafe {
        if libc::ptrace(libc::PTRACE_TRACEME, 0, 0, 0) == -1 || libc::raise(libc::SIGSTOP) != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}
fn create_ptrace(
    request: libc::c_uint,
    pid: u32,
    address: usize,
    data: usize,
) -> anyhow::Result<libc::c_long> {
    let value = unsafe {
        libc::ptrace(
            request,
            pid as libc::pid_t,
            address as *mut libc::c_void,
            data as *mut libc::c_void,
        )
    };
    if value == -1 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(value)
    }
}
fn create_assert_single_bootstrap_task(pid: u32) -> anyhow::Result<()> {
    // The trusted bootstrap is a newly exec'd image with a private file table.
    // It must still be its single natal task when ptrace admits that table.
    for entry in fs::read_dir(format!("/proc/{pid}/task"))? {
        let entry = entry?;
        anyhow::ensure!(
            entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
                == Some(pid),
            "held native bootstrap lacks single-task file-table custody"
        );
    }
    Ok(())
}
fn create_trace_begin(pid: u32) -> anyhow::Result<()> {
    let mut status = 0;
    anyhow::ensure!(
        unsafe { libc::waitpid(pid as i32, &mut status, libc::__WALL) } == pid as i32
            && libc::WIFSTOPPED(status),
        "original native bootstrap was not held"
    );
    create_ptrace(
        libc::PTRACE_SETOPTIONS,
        pid,
        0,
        (libc::PTRACE_O_TRACEFORK
            | libc::PTRACE_O_TRACEVFORK
            | libc::PTRACE_O_TRACECLONE
            | libc::PTRACE_O_TRACEEXEC
            | libc::PTRACE_O_EXITKILL
            | libc::PTRACE_O_TRACESYSGOOD) as usize,
    )?;
    Ok(())
}
fn create_trace_resume(pid: u32, signal: i32) -> anyhow::Result<()> {
    create_ptrace(libc::PTRACE_SYSCALL, pid, 0, signal as usize).map(|_| ())
}
fn create_pidfd(pid: u32) -> anyhow::Result<Option<std::os::fd::OwnedFd>> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    if fd >= 0 {
        return Ok(Some(unsafe {
            std::os::fd::OwnedFd::from_raw_fd(fd as i32)
        }));
    }
    let error = std::io::Error::last_os_error();
    // Kernels without separate thread pidfds still retain real kernel ptrace ownership.
    if error.raw_os_error() == Some(libc::EINVAL) || error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(None);
    }
    Err(error.into())
}
// The semantic pending state is authoritative even with diagnostics disabled.
struct CreateTraceActor {
    trace_pid: u32,
    fd: Option<std::os::fd::OwnedFd>,
    birth: Option<super::ProcessIncarnation>,
    pending: Option<CreatePendingSyscall>,
    initial_exit_allowed: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreateFdEvidence {
    Pipe,
    UnixStream,
    OtherSocket,
    Other,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreateSyscallAction {
    Ordinary,
    External,
    Unproven,
    UnixConnect,
    PipePgrpQuery,
}
impl CreateSyscallAction {
    fn needs_exit(self) -> bool {
        matches!(self, Self::UnixConnect | Self::PipePgrpQuery)
    }
    fn entry(self) -> CreateSyscallAudit {
        CreateSyscallAudit {
            external: self == Self::External,
            unproven: self == Self::Unproven,
        }
    }
    fn exit(self, result: i64, is_error: bool) -> CreateSyscallAudit {
        let no_delegation = match self {
            Self::UnixConnect => is_error && result == -(libc::ENOENT as i64),
            Self::PipePgrpQuery => is_error && result == -(libc::ENOTTY as i64),
            _ => return self.entry(),
        };
        CreateSyscallAudit {
            external: !no_delegation,
            unproven: false,
        }
    }
}
struct CreatePendingSyscall {
    nr: i64,
    action: CreateSyscallAction,
    sequence: Option<u8>,
}
struct CreateTraceDiagnostics {
    root: super::ProcessIncarnation,
    enabled: bool,
    remaining: u8,
    truncated: bool,
}
fn create_trace_diagnostic(message: std::fmt::Arguments<'_>) {
    use std::io::Write;
    // A broken diagnostic sink must not change native admission or retirement.
    let _ = writeln!(std::io::stderr().lock(), "owned-create-syscall {message}");
}
impl CreateTraceActor {
    fn missing_exit(&mut self, diagnostics: &CreateTraceDiagnostics, reason: &str) -> bool {
        let Some(pending) = self.pending.take() else {
            return false;
        };
        if let Some(sequence) = pending.sequence {
            create_trace_diagnostic(format_args!(
                "root={:?} actor={:?} actor_pid={} sequence={sequence} nr={} phase=missing_exit reason={reason}",
                diagnostics.root, self.birth, self.trace_pid, pending.nr,
            ));
        }
        pending.action.needs_exit()
    }
}
impl CreateTraceDiagnostics {
    fn entry(
        &mut self,
        actor: &CreateTraceActor,
        nr: i64,
        args: &[u64; 6],
        action: CreateSyscallAction,
    ) -> Option<u8> {
        if !self.enabled || action == CreateSyscallAction::Ordinary {
            return None;
        }
        if self.remaining == 0 {
            if !self.truncated {
                create_trace_diagnostic(format_args!(
                    "root={:?} phase=truncated pair_limit=64 coverage=not_complete",
                    self.root,
                ));
                self.truncated = true;
            }
            return None;
        }
        let sequence = 65 - self.remaining;
        self.remaining -= 1;
        let socket = (nr == libc::SYS_socket).then(|| [args[0], args[1], args[2]]);
        let descriptor = create_descriptor_argument(nr).map(|index| args[index]);
        let request = (nr == libc::SYS_ioctl).then_some(args[1]);
        let command = (nr == libc::SYS_fcntl).then_some(args[1]);
        let audit = action.entry();
        create_trace_diagnostic(format_args!(
            "root={:?} actor={:?} actor_pid={} sequence={sequence} nr={nr} phase=held_entry external={} unproven={} decision={action:?} socket={socket:?} descriptor={descriptor:?} ioctl_request={request:?} fcntl_command={command:?}",
            self.root, actor.birth, actor.trace_pid, audit.external, audit.unproven,
        ));
        Some(sequence)
    }
}
#[derive(Default)]
struct CreateSyscallAudit {
    external: bool,
    unproven: bool,
}
#[derive(Clone, Copy)]
enum CreateSyscallStop {
    Entry { nr: i64, args: [u64; 6] },
    Exit { result: i64, is_error: bool },
    Unproven,
}
#[cfg(target_arch = "x86_64")]
fn create_parse_syscall_stop(info: &[u8; 128], length: usize) -> CreateSyscallStop {
    if length < 8 || u32::from_ne_bytes(info[4..8].try_into().unwrap()) != 0xc000003e {
        return CreateSyscallStop::Unproven;
    }
    match info[0] {
        1 if length >= 80 => {
            let nr = u64::from_ne_bytes(info[24..32].try_into().unwrap());
            if nr & 0x40000000 != 0 || nr > i64::MAX as u64 {
                return CreateSyscallStop::Unproven;
            }
            let mut args = [0; 6];
            for (index, arg) in args.iter_mut().enumerate() {
                *arg = u64::from_ne_bytes(info[32 + index * 8..40 + index * 8].try_into().unwrap());
            }
            CreateSyscallStop::Entry {
                nr: nr as i64,
                args,
            }
        }
        2 if length >= 33 && info[32] <= 1 => CreateSyscallStop::Exit {
            result: i64::from_ne_bytes(info[24..32].try_into().unwrap()),
            is_error: info[32] != 0,
        },
        _ => CreateSyscallStop::Unproven,
    }
}
fn create_descriptor_argument(nr: i64) -> Option<usize> {
    if [libc::SYS_splice, libc::SYS_copy_file_range].contains(&nr) {
        Some(2)
    } else if nr == libc::SYS_tee {
        Some(1)
    } else if [
        libc::SYS_connect,
        libc::SYS_ioctl,
        libc::SYS_fcntl,
        libc::SYS_write,
        libc::SYS_writev,
        libc::SYS_pwrite64,
        libc::SYS_pwritev,
        libc::SYS_pwritev2,
        libc::SYS_sendfile,
        libc::SYS_vmsplice,
    ]
    .contains(&nr)
    {
        Some(0)
    } else {
        None
    }
}
fn create_syscall_action(nr: i64, args: &[u64; 6], fd: CreateFdEvidence) -> CreateSyscallAction {
    use CreateSyscallAction as Action;
    if nr == libc::SYS_clone3
        || (nr == libc::SYS_clone
            && args[0] & (libc::CLONE_UNTRACED | libc::CLONE_FILES) as u64 != 0)
    {
        // No mutable clone3 payload peek or shared-FD-table absence assertion.
        return Action::Unproven;
    }
    if nr == libc::SYS_socket {
        let kind = args[1] & !((libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) as u64);
        return if args[0] == libc::AF_UNIX as u64
            && kind == libc::SOCK_STREAM as u64
            && args[2] == 0
        {
            Action::Ordinary
        } else {
            Action::External
        };
    }
    if nr == libc::SYS_connect {
        return match fd {
            CreateFdEvidence::UnixStream => Action::UnixConnect,
            CreateFdEvidence::Unknown => Action::Unproven,
            _ => Action::External,
        };
    }
    if nr == libc::SYS_ioctl {
        if args[1] == libc::TIOCGPGRP {
            return match fd {
                CreateFdEvidence::Pipe => Action::PipePgrpQuery,
                CreateFdEvidence::Unknown => Action::Unproven,
                _ => Action::External,
            };
        }
        return if [libc::TCGETS, libc::TIOCGWINSZ, libc::FIONREAD].contains(&args[1]) {
            Action::Ordinary
        } else {
            Action::External
        };
    }
    if nr == libc::SYS_fcntl && [libc::F_GETFD as u64, libc::F_GETFL as u64].contains(&args[1]) {
        return Action::Ordinary;
    }
    if [
        libc::SYS_socketpair,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_setsockopt,
        libc::SYS_shutdown,
        libc::SYS_sendmsg,
        libc::SYS_sendmmsg,
        libc::SYS_sendto,
        libc::SYS_bpf,
        libc::SYS_io_uring_setup,
        libc::SYS_ptrace,
        libc::SYS_shmget,
        libc::SYS_shmat,
        libc::SYS_semop,
        libc::SYS_msgsnd,
        libc::SYS_mq_open,
        libc::SYS_mq_timedsend,
        libc::SYS_pidfd_getfd,
        libc::SYS_process_vm_writev,
    ]
    .contains(&nr)
        || ((nr == libc::SYS_kill || nr == libc::SYS_tkill) && args[1] != 0)
        || (nr == libc::SYS_tgkill && args[2] != 0)
    {
        return Action::External;
    }
    if create_descriptor_argument(nr).is_some() {
        // Mutating descriptor operations can delegate through a socket.
        return match fd {
            CreateFdEvidence::UnixStream | CreateFdEvidence::OtherSocket => Action::External,
            CreateFdEvidence::Unknown => Action::Unproven,
            _ => Action::Ordinary,
        };
    }
    Action::Ordinary
}
fn create_unix_socket_type(line: &str, inode: u64) -> Option<u32> {
    let mut fields = line.split_ascii_whitespace();
    let kind = u32::from_str_radix(fields.nth(4)?, 16).ok()?;
    fields.next()?; // kernel socket state
    (fields.next()?.parse::<u64>().ok()? == inode).then_some(kind)
}
fn create_held_fd_evidence(
    actor: &CreateTraceActor,
    descriptor: u64,
    table_private: bool,
) -> CreateFdEvidence {
    use std::io::BufRead;
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let Some(birth) = actor.birth else {
        return CreateFdEvidence::Unknown;
    };
    if !table_private || descriptor > i32::MAX as u64 {
        return CreateFdEvidence::Unknown;
    }
    // This is the actual stopped original actor's current role FD, not a pathname
    // authority capture. No admitted CLONE_FILES/clone3 uncertainty may race it.
    let evidence = (|| -> std::io::Result<CreateFdEvidence> {
        let metadata = fs::metadata(format!("/proc/{}/fd/{descriptor}", actor.trace_pid))?;
        if metadata.file_type().is_fifo() {
            return Ok(CreateFdEvidence::Pipe);
        }
        if !metadata.file_type().is_socket() {
            return Ok(CreateFdEvidence::Other);
        }
        let mut sockets = std::io::BufReader::new(fs::File::open(format!(
            "/proc/{}/net/unix",
            actor.trace_pid,
        ))?);
        let mut line = String::new();
        while sockets.read_line(&mut line)? != 0 {
            if let Some(kind) = create_unix_socket_type(&line, metadata.ino()) {
                return Ok(if kind == libc::SOCK_STREAM as u32 {
                    CreateFdEvidence::UnixStream
                } else {
                    CreateFdEvidence::OtherSocket
                });
            }
            line.clear();
        }
        // A socket outside this actor's UNIX table is never given the ENOENT exemption.
        Ok(CreateFdEvidence::OtherSocket)
    })();
    if super::process_incarnation(actor.trace_pid).ok().flatten() != Some(birth) {
        return CreateFdEvidence::Unknown;
    }
    evidence.unwrap_or(CreateFdEvidence::Unknown)
}
fn create_audit_stop(
    actor: &mut CreateTraceActor,
    diagnostics: &mut CreateTraceDiagnostics,
    stop: CreateSyscallStop,
    fd: CreateFdEvidence,
    identity_matches: bool,
    table_private: bool,
) -> CreateSyscallAudit {
    if !identity_matches {
        actor.missing_exit(diagnostics, "original_actor_birth_changed");
        actor.initial_exit_allowed = false;
        return CreateSyscallAudit {
            external: false,
            unproven: true,
        };
    }
    match stop {
        CreateSyscallStop::Entry { nr, args } => {
            let replaced = actor.pending.is_some();
            actor.missing_exit(diagnostics, "next_entry_without_exit");
            actor.initial_exit_allowed = false;
            let action = create_syscall_action(nr, &args, fd);
            let sequence = diagnostics.entry(actor, nr, &args, action);
            actor.pending = Some(CreatePendingSyscall {
                nr,
                action,
                sequence,
            });
            let mut audit = action.entry();
            audit.unproven |= replaced || !table_private;
            audit
        }
        CreateSyscallStop::Exit { result, is_error } => {
            let Some(pending) = actor.pending.take() else {
                // Only the trusted natal SIGSTOP or held kernel fork birth can
                // start tracing in an already-entered syscall. Consume it once.
                let unproven = !std::mem::take(&mut actor.initial_exit_allowed) || !table_private;
                return CreateSyscallAudit {
                    external: false,
                    unproven,
                };
            };
            actor.initial_exit_allowed = false;
            if let Some(sequence) = pending.sequence {
                create_trace_diagnostic(format_args!(
                    "root={:?} actor={:?} actor_pid={} sequence={sequence} nr={} phase=held_exit result={result} kernel_is_error={is_error}",
                    diagnostics.root, actor.birth, actor.trace_pid, pending.nr,
                ));
            }
            let mut audit = pending.action.exit(result, is_error);
            audit.unproven |= !table_private;
            audit
        }
        CreateSyscallStop::Unproven => {
            actor.missing_exit(diagnostics, "unpaired_abi_phase");
            actor.initial_exit_allowed = false;
            CreateSyscallAudit {
                external: false,
                unproven: true,
            }
        }
    }
}
#[cfg(target_arch = "x86_64")]
fn create_audit_syscall(
    pid: u32,
    actor: &mut CreateTraceActor,
    diagnostics: &mut CreateTraceDiagnostics,
    table_private: bool,
) -> anyhow::Result<CreateSyscallAudit> {
    let mut info = [0u8; 128];
    let length = create_ptrace(0x420e, pid, info.len(), info.as_mut_ptr() as usize)? as usize;
    let stop = create_parse_syscall_stop(&info, length);
    let identity_matches = actor.birth.is_some() && super::process_incarnation(pid)? == actor.birth;
    let fd = match &stop {
        CreateSyscallStop::Entry { nr, args } if identity_matches => {
            create_descriptor_argument(*nr).map_or(CreateFdEvidence::Other, |index| {
                create_held_fd_evidence(actor, args[index], table_private)
            })
        }
        _ => CreateFdEvidence::Unknown,
    };
    Ok(create_audit_stop(
        actor,
        diagnostics,
        stop,
        fd,
        identity_matches,
        table_private,
    ))
}
#[cfg(not(target_arch = "x86_64"))]
fn create_audit_syscall(
    _: u32,
    _: &mut CreateTraceActor,
    _: &mut CreateTraceDiagnostics,
    _: bool,
) -> anyhow::Result<CreateSyscallAudit> {
    Ok(CreateSyscallAudit {
        external: false,
        unproven: true,
    })
}
pub(super) fn exec_owned_create(
    _program: &std::ffi::CString,
    executable: &std::fs::File,
    directory: &std::fs::File,
    argv: &[std::ffi::CString],
    env: &[std::ffi::CString],
) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    let mut argvp: Vec<_> = argv.iter().map(|v| v.as_ptr()).collect();
    argvp.push(std::ptr::null());
    let mut envp: Vec<_> = env.iter().map(|v| v.as_ptr()).collect();
    envp.push(std::ptr::null());
    let input = std::fs::File::open("/dev/null")?;
    unsafe {
        if libc::fchdir(directory.as_raw_fd()) != 0
            || libc::dup2(input.as_raw_fd(), libc::STDIN_FILENO) == -1
            || libc::fcntl(executable.as_raw_fd(), libc::F_SETFD, 0) == -1
        {
            return Err(std::io::Error::last_os_error().into());
        }
        libc::fexecve(executable.as_raw_fd(), argvp.as_ptr(), envp.as_ptr());
    }
    Err(std::io::Error::last_os_error().into())
}

pub(super) fn owned_create_descendant_traceable() -> bool {
    cfg!(target_arch = "x86_64")
}
pub(super) fn owned_create_anchor_path(
    _: &std::fs::File,
    slot: u32,
) -> anyhow::Result<(std::ffi::OsString, bool)> {
    Ok((format!("/proc/self/fd/{slot}").into(), false))
}
pub(super) fn install_owned_create_anchors(
    files: &[std::fs::File],
    slots: &[u32],
) -> anyhow::Result<()> {
    use std::os::fd::AsRawFd;
    for (file, &slot) in files.iter().zip(slots) {
        if unsafe { libc::dup2(file.as_raw_fd(), slot as i32) } == -1
            || unsafe { libc::fcntl(slot as i32, libc::F_SETFD, 0) } == -1
        {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    Ok(())
}

pub(super) fn is_process_group_alive(pgid: u32) -> bool {
    if pgid == 0 {
        return false;
    }
    if super::worker::is_pid_alive_and_ours(pgid) && !is_terminated(pgid) {
        return true;
    }
    process_group_has_live_members(pgid).unwrap_or(true)
}

pub(super) use super::unix::{
    configure_process_group, kill_process_group, terminate_process_group,
};
pub(super) fn rename_exclusive(
    source_dir: &std::os::fd::OwnedFd,
    source: &std::ffi::OsStr,
    destination_dir: &std::os::fd::OwnedFd,
    destination: &std::ffi::OsStr,
) -> std::io::Result<()> {
    use nix::NixPath;
    use std::os::fd::AsRawFd;

    let result = source.with_nix_path(|source| {
        destination.with_nix_path(|destination| {
            // SAFETY: both descriptors remain owned for the call and NixPath
            // supplies live NUL-terminated names. The syscall also supports musl.
            nix::errno::Errno::result(unsafe {
                libc::syscall(
                    libc::SYS_renameat2,
                    source_dir.as_raw_fd(),
                    source.as_ptr(),
                    destination_dir.as_raw_fd(),
                    destination.as_ptr(),
                    libc::RENAME_NOREPLACE,
                )
            })
            .map(|_| ())
        })
    })??;
    result.map_err(std::io::Error::from)
}

pub(super) fn collect_pid_tree(pid: u32) -> Vec<u32> {
    let children_map = build_children_map();
    let mut pids = vec![pid];
    super::collect_descendants_from_map(pid, &children_map, &mut pids);
    pids
}

pub(super) fn build_children_map() -> HashMap<u32, Vec<u32>> {
    let mut children_map: HashMap<u32, Vec<u32>> = HashMap::new();
    let proc_dir = Path::new("/proc");
    let Ok(entries) = fs::read_dir(proc_dir) else {
        return children_map;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        let Ok(child_pid) = name_str.parse::<u32>() else {
            continue;
        };

        let stat_path = entry.path().join("stat");
        let Ok(content) = fs::read_to_string(&stat_path) else {
            continue;
        };

        if let Some(ppid) = parse_stat_field(&content, 3) {
            children_map.entry(ppid as u32).or_default().push(child_pid);
        }
    }

    children_map
}

/// Environment entries are compared NUL-delimited, so there is no prefix collision.
/// `environ` is owner-only, so only same-uid processes can match on it.
pub(super) fn processes_matching(
    env_needles: &[String],
    cmdline_needles: &[Option<String>],
    executable_needles: &[Option<String>],
) -> Vec<bool> {
    let n = env_needles.len();
    let mut found = vec![false; n];
    let mut remaining = n;
    let Ok(entries) = fs::read_dir("/proc") else {
        return found;
    };
    for entry in entries.flatten() {
        if remaining == 0 {
            break;
        }
        let name = entry.file_name();
        if name.to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let dir = entry.path();

        let environ_raw = fs::read(dir.join("environ")).unwrap_or_default();
        let environ = String::from_utf8_lossy(&environ_raw);
        let env_entries: std::collections::HashSet<&str> =
            environ.split('\0').filter(|s| !s.is_empty()).collect();

        let cmd_raw = fs::read(dir.join("cmdline")).unwrap_or_default();
        let cmdline_raw = String::from_utf8_lossy(&cmd_raw);
        let cmd_tokens: Vec<&str> = cmdline_raw
            .split('\0')
            .filter(|value| !value.is_empty())
            .collect();
        let cmdline = cmdline_raw.replace('\0', " ");

        for i in 0..n {
            if found[i] {
                continue;
            }
            let env_hit =
                !env_needles[i].is_empty() && env_entries.contains(env_needles[i].as_str());
            let cmd_hit = cmdline_needles[i]
                .as_deref()
                .is_some_and(|s| !s.is_empty() && cmdline.contains(s));
            let executable_hit = executable_needles[i].as_deref().is_some_and(|needle| {
                !needle.is_empty()
                    && cmd_tokens.iter().any(|token| {
                        std::path::Path::new(token)
                            .file_name()
                            .and_then(|value| value.to_str())
                            == Some(needle)
                    })
            });
            let has_env = !env_needles[i].is_empty();
            let has_cmd = cmdline_needles[i]
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            let has_executable = executable_needles[i]
                .as_deref()
                .is_some_and(|value| !value.is_empty());
            let matched = (has_env || has_cmd || has_executable)
                && (!has_env || env_hit)
                && (!has_cmd || cmd_hit)
                && (!has_executable || executable_hit);
            if matched {
                found[i] = true;
                remaining -= 1;
            }
        }
    }
    found
}

pub(super) fn sample_memory() -> super::metrics::MemorySample {
    let meminfo = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let total = parse_meminfo_field(&meminfo, "MemTotal").map(kib_to_bytes);
    let avail = parse_meminfo_field(&meminfo, "MemAvailable").map(kib_to_bytes);

    let psi_mem_some_avg10 =
        parse_psi_some_avg10(&fs::read_to_string("/proc/pressure/memory").unwrap_or_default());
    let psi_io_some_avg10 =
        parse_psi_some_avg10(&fs::read_to_string("/proc/pressure/io").unwrap_or_default());

    // Old kernels and WSL1 omit MemAvailable; report unknown rather than a false 100% used.
    let (total_bytes, available_bytes) = match (total, avail) {
        (Some(t), Some(a)) => (t, a),
        _ => (0, 0),
    };

    super::metrics::MemorySample {
        total_bytes,
        available_bytes,
        psi_mem_some_avg10,
        psi_io_some_avg10,
        macos_pressure_level: None,
    }
}

pub(super) fn sample_system() -> super::metrics::SystemReading {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    let cpu = stat.lines().next().and_then(|line| {
        let values: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|value| value.parse().ok())
            .collect();
        (!values.is_empty()).then(|| {
            (
                values.iter().sum(),
                values.get(3).copied().unwrap_or(0) + values.get(4).copied().unwrap_or(0),
            )
        })
    });
    let load = fs::read_to_string("/proc/loadavg").ok().and_then(|value| {
        let values: Vec<f64> = value
            .split_whitespace()
            .take(3)
            .filter_map(|part| part.parse().ok())
            .collect();
        (values.len() == 3).then(|| [values[0], values[1], values[2]])
    });
    let mem = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| {
        mem.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    let total = field("SwapTotal:");
    let free = field("SwapFree:");
    (cpu, None, load, (total, total.saturating_sub(free)))
}

pub(super) fn process_snapshot() -> Vec<super::metrics::ProcessRecord> {
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(1) as u64;
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let end = stat.rfind(')')?;
            let fields: Vec<&str> = stat[end + 2..].split_whitespace().collect();
            let ppid = fields.get(1)?.parse().ok()?;
            let utime: u64 = fields.get(11)?.parse().ok()?;
            let stime: u64 = fields.get(12)?.parse().ok()?;
            let start_id = fields.get(19)?.parse().ok()?;
            let rss_pages: i64 = fields.get(21)?.parse().ok()?;
            Some(super::metrics::ProcessRecord {
                pid,
                ppid,
                start_id,
                rss_bytes: rss_pages.max(0) as u64 * page,
                cpu_seconds: (utime + stime) as f64 / hz,
            })
        })
        .collect()
}

fn kib_to_bytes(kib: u64) -> u64 {
    kib.saturating_mul(1024)
}

fn parse_meminfo_field(meminfo: &str, key: &str) -> Option<u64> {
    for line in meminfo.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != key {
            continue;
        }
        return rest.split_whitespace().next()?.parse().ok();
    }
    None
}

/// `None` when PSI is unavailable, never a false 0.0.
fn parse_psi_some_avg10(psi: &str) -> Option<f32> {
    for line in psi.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() != Some("some") {
            continue;
        }
        return fields.find_map(|kv| kv.strip_prefix("avg10=")?.parse().ok());
    }
    None
}

pub(super) fn process_namespace() -> std::io::Result<[u64; 2]> {
    use std::os::unix::fs::MetadataExt;
    let status = fs::read_to_string("/proc/self/status")?;
    let line = status
        .lines()
        .find_map(|line| line.strip_prefix("NSpid:"))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "PID namespace cannot be verified",
            )
        })?;
    let mut ids = line.split_whitespace();
    if ids.next().and_then(|id| id.parse::<u32>().ok()) != Some(std::process::id())
        || ids.next().is_some()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "procfs belongs to another PID namespace",
        ));
    }
    let namespace = fs::metadata("/proc/self/ns/pid")?;
    Ok([namespace.dev(), namespace.ino()])
}

// A hidden or unreadable process is not absent. Signal zero has no process effect.
pub(super) fn custodian_process_absent(pid: u32) -> bool {
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

pub(super) fn process_incarnation(pid: u32) -> std::io::Result<Option<super::ProcessIncarnation>> {
    let namespace = process_namespace()?;
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let end = stat.rfind(')').ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "process stat has no command boundary",
        )
    })?;
    let mut fields = stat[end + 1..].split_whitespace();
    let group = fields
        .nth(2)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "process stat has no group")
        })?
        .parse()
        .map_err(std::io::Error::other)?;
    let start = fields
        .nth(16)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "process stat has no start time",
            )
        })?
        .parse()
        .map_err(std::io::Error::other)?;
    Ok(Some(super::ProcessIncarnation {
        pid,
        group,
        start: [start, 0],
        namespace,
    }))
}

pub(super) fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn get_foreground_pid(shell_pid: u32) -> Option<u32> {
    let stat_path = format!("/proc/{}/stat", shell_pid);
    let stat_content = fs::read_to_string(&stat_path).ok()?;

    let tpgid = parse_stat_field(&stat_content, 7)?;

    if tpgid <= 0 {
        return Some(shell_pid);
    }

    find_process_in_group(tpgid as u32).or(Some(shell_pid))
}

fn find_process_in_group(pgrp: u32) -> Option<u32> {
    let proc_dir = Path::new("/proc");
    if !proc_dir.exists() {
        return None;
    }

    // A process can exit mid-scan; skip it rather than abort to the shell pid.
    for entry in fs::read_dir(proc_dir).ok()?.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        let Ok(pid) = name_str.parse::<u32>() else {
            continue;
        };

        let stat_path = entry.path().join("stat");
        let Ok(content) = fs::read_to_string(&stat_path) else {
            continue;
        };

        if let Some(proc_pgrp) = parse_stat_field(&content, 4) {
            if proc_pgrp as u32 == pgrp {
                return Some(pid);
            }
        }
    }

    None
}

/// Report non-zombie group members; failed observations do not prove absence.
pub(super) fn process_group_has_live_members(pgrp: u32) -> std::io::Result<bool> {
    use nix::{errno::Errno, sys::signal::killpg, unistd::Pid};

    match killpg(Pid::from_raw(pgrp as i32), None) {
        Ok(()) => {}
        Err(Errno::ESRCH) => return Ok(false),
        Err(error) => return Err(std::io::Error::from_raw_os_error(error as i32)),
    }
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let stat = match fs::read_to_string(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid proc stat");
        // `comm` may contain spaces and parentheses, so the fields past it start
        // after the last one: state, ppid, pgrp, then the session id.
        let (_, fields) = stat.rsplit_once(')').ok_or_else(invalid)?;
        let mut fields = fields.split_whitespace();
        let state = fields.next().ok_or_else(invalid)?;
        let _parent = fields.next().ok_or_else(invalid)?;
        let group = fields.next().ok_or_else(invalid)?;
        if state != "Z" && group.parse::<u32>() == Ok(pgrp) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `pid` has already terminated, which for a child of this process means
/// it is waiting to be reaped rather than still running.
///
/// A zombie holds nothing: it cannot write, it holds no file descriptors open
/// on a checkout, and its process group is dead. Treating it as alive makes a
/// torn-down runner unprovable forever. The repo's own descendant wait already
/// uses this rule (`process::mod` test helper: "exited or a terminated zombie").
pub(super) fn is_terminated(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The state letter follows `comm`, which may itself contain spaces and
    // parentheses, so anchor on the last `)`.
    let Some(close_paren) = stat.rfind(')') else {
        return false;
    };
    stat[close_paren + 1..].trim_start().starts_with('Z')
}

pub(super) fn parent_and_argv0(pid: u32) -> Option<(u32, String)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let ppid = u32::try_from(parse_stat_field(&stat, 3)?).ok()?;
    let cmdline = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv0 = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
    Some((ppid, String::from_utf8_lossy(argv0).into_owned()))
}

/// `comm` (field 2) may contain spaces.
fn parse_stat_field(content: &str, field_idx: usize) -> Option<i64> {
    let close_paren = content.rfind(')')?;
    let after_comm = &content[close_paren + 2..]; // Skip ") "

    let adjusted_idx = field_idx.checked_sub(2)?;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    fields.get(adjusted_idx)?.parse().ok()
}

pub(super) struct SystemdInhibitor {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

impl SystemdInhibitor {
    pub(super) fn new() -> Self {
        Self {
            child: None,
            stdin: None,
        }
    }
}

impl super::SleepInhibit for SystemdInhibitor {
    fn acquire(&mut self) -> anyhow::Result<()> {
        if super::sleep_inhibit_unavailable() {
            return Ok(());
        }
        let mut child = match Command::new("systemd-inhibit")
            .args([
                "--what=idle:sleep",
                "--mode=block",
                "--who=Agent of Empires",
                "--why=Active agent sessions",
                "cat",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                super::latch_sleep_inhibit_unavailable(
                    "systemd-inhibit not found; OS sleep will not be inhibited on this host",
                );
                return Ok(());
            }
            Err(e) => return Err(e.into()),
        };
        // `systemd-inhibit` holds the lock only while `cat` runs, which ends at stdin EOF.
        self.stdin = child.stdin.take();
        self.child = Some(child);
        Ok(())
    }

    fn release(&mut self) {
        // logind releases the lock when the holder dies by any cause.
        self.stdin = None;
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn is_held_alive(&mut self) -> bool {
        super::sleep_inhibit_child_held_alive(
            &mut self.child,
            "systemd-inhibit exited without taking the lock (no logind?); \
             OS sleep will not be inhibited on this host",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stat_field() {
        let stat = "1234 (bash) S 1233 1234 1234 34816 1234 4194304 1234 0 0 0";

        assert_eq!(parse_stat_field(stat, 3), Some(1233)); // ppid
        assert_eq!(parse_stat_field(stat, 4), Some(1234)); // pgrp
        assert_eq!(parse_stat_field(stat, 7), Some(1234)); // tpgid
    }

    const MEMINFO: &str = "\
MemTotal:       32791036 kB
MemFree:         1234567 kB
MemAvailable:    9876543 kB
Cached:          5678901 kB
";

    #[test]
    fn test_parse_meminfo_field() {
        let cases = [
            ("MemTotal", Some(32791036)),
            ("MemAvailable", Some(9876543)),
            ("MemFree", Some(1234567)),
            ("Nonexistent", None),
            ("Mem", None),
        ];
        for (key, expected) in cases {
            assert_eq!(parse_meminfo_field(MEMINFO, key), expected, "{key}");
        }
    }

    #[test]
    fn test_parse_psi_some_avg10() {
        let psi = "\
some avg10=1.23 avg60=4.56 avg300=7.89 total=123456789
full avg10=0.10 avg60=0.20 avg300=0.30 total=42
";
        assert_eq!(parse_psi_some_avg10(psi), Some(1.23));
        assert_eq!(parse_psi_some_avg10(""), None);
        assert_eq!(parse_psi_some_avg10("full avg10=5.0 total=9"), None);
    }
    #[test]
    fn owned_create_domain_outcomes_reach_real_quiescence_predicate() {
        use CreateFdEvidence as Fd;
        struct Case {
            name: &'static str,
            nr: i64,
            args: [u64; 6],
            fd: Fd,
            exit: Option<(i64, bool)>,
            identity: bool,
            private: bool,
            retired: bool,
        }
        let args = |a, b, c| [a, b, c, 0, 0, 0];
        let cases = [
            Case {
                name: "socket descriptor flag query",
                nr: libc::SYS_fcntl,
                args: args(3, libc::F_GETFD as u64, 0),
                fd: Fd::UnixStream,
                exit: Some((libc::FD_CLOEXEC as i64, false)),
                identity: true,
                private: true,
                retired: true,
            },
            Case {
                name: "unknown descriptor status query",
                nr: libc::SYS_fcntl,
                args: args(3, libc::F_GETFL as u64, 0),
                fd: Fd::Unknown,
                exit: Some((-(libc::EBADF as i64), true)),
                identity: true,
                private: true,
                retired: true,
            },
            Case {
                name: "socket descriptor owner mutation remains protected",
                nr: libc::SYS_fcntl,
                args: args(3, libc::F_SETOWN as u64, 42),
                fd: Fd::UnixStream,
                exit: Some((0, false)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "local UNIX allocation",
                nr: libc::SYS_socket,
                args: args(
                    libc::AF_UNIX as u64,
                    (libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK) as u64,
                    0,
                ),
                fd: Fd::Other,
                exit: Some((4, false)),
                identity: true,
                private: true,
                retired: true,
            },
            Case {
                name: "other socket stays conservative",
                nr: libc::SYS_socket,
                args: args(libc::AF_INET as u64, libc::SOCK_STREAM as u64, 0),
                fd: Fd::Other,
                exit: Some((4, false)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "exact UNIX ENOENT",
                nr: libc::SYS_connect,
                args: args(4, 0, 0),
                fd: Fd::UnixStream,
                exit: Some((-(libc::ENOENT as i64), true)),
                identity: true,
                private: true,
                retired: true,
            },
            Case {
                name: "successful delegation",
                nr: libc::SYS_connect,
                args: args(4, 0, 0),
                fd: Fd::UnixStream,
                exit: Some((0, false)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "asynchronous connection",
                nr: libc::SYS_connect,
                args: args(4, 0, 0),
                fd: Fd::UnixStream,
                exit: Some((-(libc::EINPROGRESS as i64), true)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "arbitrary failure is not safe",
                nr: libc::SYS_connect,
                args: args(4, 0, 0),
                fd: Fd::UnixStream,
                exit: Some((-(libc::EACCES as i64), true)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "ENOENT with wrong kernel phase",
                nr: libc::SYS_connect,
                args: args(4, 0, 0),
                fd: Fd::UnixStream,
                exit: Some((-(libc::ENOENT as i64), false)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "ENOENT on another socket",
                nr: libc::SYS_connect,
                args: args(4, 0, 0),
                fd: Fd::OtherSocket,
                exit: Some((-(libc::ENOENT as i64), true)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "FD identity unavailable",
                nr: libc::SYS_connect,
                args: args(4, 0, 0),
                fd: Fd::Unknown,
                exit: Some((-(libc::ENOENT as i64), true)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "original birth changed",
                nr: libc::SYS_connect,
                args: args(4, 0, 0),
                fd: Fd::UnixStream,
                exit: Some((-(libc::ENOENT as i64), true)),
                identity: false,
                private: true,
                retired: false,
            },
            Case {
                name: "shared table cannot certify FD",
                nr: libc::SYS_connect,
                args: args(4, 0, 0),
                fd: Fd::UnixStream,
                exit: Some((-(libc::ENOENT as i64), true)),
                identity: true,
                private: false,
                retired: false,
            },
            Case {
                name: "connect missing exit",
                nr: libc::SYS_connect,
                args: args(4, 0, 0),
                fd: Fd::UnixStream,
                exit: None,
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "pipe TIOCGPGRP ENOTTY",
                nr: libc::SYS_ioctl,
                args: args(2, libc::TIOCGPGRP, 0),
                fd: Fd::Pipe,
                exit: Some((-(libc::ENOTTY as i64), true)),
                identity: true,
                private: true,
                retired: true,
            },
            Case {
                name: "query unexpected success",
                nr: libc::SYS_ioctl,
                args: args(2, libc::TIOCGPGRP, 0),
                fd: Fd::Pipe,
                exit: Some((0, false)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "query arbitrary error",
                nr: libc::SYS_ioctl,
                args: args(2, libc::TIOCGPGRP, 0),
                fd: Fd::Pipe,
                exit: Some((-(libc::EBADF as i64), true)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "query wrong descriptor kind",
                nr: libc::SYS_ioctl,
                args: args(2, libc::TIOCGPGRP, 0),
                fd: Fd::Other,
                exit: Some((-(libc::ENOTTY as i64), true)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "query missing exit",
                nr: libc::SYS_ioctl,
                args: args(2, libc::TIOCGPGRP, 0),
                fd: Fd::Pipe,
                exit: None,
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "unknown ioctl is not safe",
                nr: libc::SYS_ioctl,
                args: args(2, 0xbad, 0),
                fd: Fd::Pipe,
                exit: Some((-(libc::ENOTTY as i64), true)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "write alias of socket",
                nr: libc::SYS_write,
                args: args(19, 0, 1),
                fd: Fd::UnixStream,
                exit: Some((1, false)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "replacement socket descriptor",
                nr: libc::SYS_writev,
                args: args(2, 0, 1),
                fd: Fd::OtherSocket,
                exit: Some((1, false)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "socket splice sink",
                nr: libc::SYS_splice,
                args: args(1, 0, 19),
                fd: Fd::UnixStream,
                exit: Some((1, false)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "original pipe output",
                nr: libc::SYS_write,
                args: args(2, 0, 1),
                fd: Fd::Pipe,
                exit: Some((1, false)),
                identity: true,
                private: true,
                retired: true,
            },
            Case {
                name: "unknown write descriptor",
                nr: libc::SYS_write,
                args: args(2, 0, 1),
                fd: Fd::Unknown,
                exit: Some((1, false)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "listener exposure",
                nr: libc::SYS_listen,
                args: args(4, 1, 0),
                fd: Fd::Other,
                exit: Some((0, false)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "accept remains protected even on error",
                nr: libc::SYS_accept4,
                args: args(4, 0, 0),
                fd: Fd::Other,
                exit: Some((-(libc::EAGAIN as i64), true)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "batched socket send",
                nr: libc::SYS_sendmmsg,
                args: args(4, 0, 1),
                fd: Fd::Other,
                exit: Some((1, false)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "CLONE_FILES exposure",
                nr: libc::SYS_clone,
                args: args(libc::CLONE_FILES as u64, 0, 0),
                fd: Fd::Other,
                exit: Some((8, false)),
                identity: true,
                private: true,
                retired: false,
            },
            Case {
                name: "clone3 mutable payload",
                nr: libc::SYS_clone3,
                args: args(0, 88, 0),
                fd: Fd::Other,
                exit: Some((-(libc::ENOSYS as i64), true)),
                identity: true,
                private: true,
                retired: false,
            },
        ];
        let birth = super::super::ProcessIncarnation {
            pid: 7,
            group: 7,
            start: [1, 0],
            namespace: [4, 1],
        };
        let boot = [0u8; 16];
        let commitment = [0u8; 32];
        for case in cases {
            // Neither disabled nor exhausted logging can remove semantic pending state.
            for enabled in [false, true] {
                let mut diagnostics = CreateTraceDiagnostics {
                    root: birth,
                    enabled,
                    remaining: 0,
                    truncated: true,
                };
                let mut actor = CreateTraceActor {
                    trace_pid: birth.pid,
                    fd: None,
                    birth: Some(birth),
                    pending: None,
                    initial_exit_allowed: false,
                };
                let entry = create_audit_stop(
                    &mut actor,
                    &mut diagnostics,
                    CreateSyscallStop::Entry {
                        nr: case.nr,
                        args: case.args,
                    },
                    case.fd,
                    case.identity,
                    case.private,
                );
                let mut external = entry.external;
                let mut unproven = entry.unproven;
                if case.identity {
                    assert!(actor.pending.is_some(), "{}", case.name);
                }
                match case.exit {
                    Some((result, is_error)) => {
                        let exit = create_audit_stop(
                            &mut actor,
                            &mut diagnostics,
                            CreateSyscallStop::Exit { result, is_error },
                            Fd::Unknown,
                            case.identity,
                            case.private,
                        );
                        external |= exit.external;
                        unproven |= exit.unproven;
                    }
                    None => unproven |= actor.missing_exit(&diagnostics, "actor_retired"),
                }
                let mut wire = serde_json::to_value(
                    crate::session::runner_journal::RunnerExecutionJournal::new(),
                )
                .unwrap();
                wire["creations"] = serde_json::json!([{
                    "format": 1,
                    "nonce": "00000000-0000-0000-0000-000000000001",
                    "session_id": "pure-original-receipt",
                    "created_at": "2026-10-09T00:00:00Z",
                    "generation": 1,
                    "boot": boot,
                    "commitment": commitment,
                    "profile_identity": {"device": 1, "inode": 2, "birth_time": {"secs_since_epoch": 1, "nanos_since_epoch": 0}},
                    "births": [birth],
                    "root_status": 23 << 8,
                    "descendants_retired": !unproven,
                    "group_retired": true,
                    "external_domain": external,
                    "scope_unproven": unproven,
                    "effect_acknowledged": true,
                    "no_target_approved": false
                }]);
                let journal: crate::session::runner_journal::RunnerExecutionJournal =
                    serde_json::from_value(wire).unwrap();
                assert!(journal.proves_runner_quiescent());
                // This invokes the same CreateExecution::proves_retired predicate
                // used by native withdrawal, not a duplicate test-only predicate.
                assert_eq!(
                    journal.proves_quiescent(),
                    case.retired,
                    "{} logging={enabled}",
                    case.name
                );
            }
        }
        let mut diagnostics = CreateTraceDiagnostics {
            root: birth,
            enabled: false,
            remaining: 64,
            truncated: false,
        };
        let mut actor = CreateTraceActor {
            trace_pid: birth.pid,
            fd: None,
            birth: Some(birth),
            pending: None,
            initial_exit_allowed: true,
        };
        let exit = CreateSyscallStop::Exit {
            result: 0,
            is_error: false,
        };
        assert!(
            !create_audit_stop(&mut actor, &mut diagnostics, exit, Fd::Unknown, true, true)
                .unproven
        );
        assert!(
            create_audit_stop(&mut actor, &mut diagnostics, exit, Fd::Unknown, true, true).unproven
        );
        create_audit_stop(
            &mut actor,
            &mut diagnostics,
            CreateSyscallStop::Entry {
                nr: libc::SYS_connect,
                args: args(4, 0, 0),
            },
            Fd::UnixStream,
            true,
            true,
        );
        assert!(
            create_audit_stop(
                &mut actor,
                &mut diagnostics,
                CreateSyscallStop::Unproven,
                Fd::Unknown,
                true,
                true
            )
            .unproven
        );
        assert!(actor.pending.is_none());
        #[cfg(target_arch = "x86_64")]
        {
            let mut info = [0u8; 128];
            info[0] = 1;
            info[4..8].copy_from_slice(&0xc000003eu32.to_ne_bytes());
            info[24..32].copy_from_slice(&(libc::SYS_connect as u64).to_ne_bytes());
            assert!(matches!(
                create_parse_syscall_stop(&info, 80),
                CreateSyscallStop::Entry {
                    nr: libc::SYS_connect,
                    ..
                }
            ));
            assert!(matches!(
                create_parse_syscall_stop(&info, 79),
                CreateSyscallStop::Unproven
            ));
            info[24..32].copy_from_slice(&(0x40000000u64 | libc::SYS_connect as u64).to_ne_bytes());
            assert!(matches!(
                create_parse_syscall_stop(&info, 80),
                CreateSyscallStop::Unproven
            ));
            info[0] = 2;
            info[32] = 2;
            assert!(matches!(
                create_parse_syscall_stop(&info, 33),
                CreateSyscallStop::Unproven
            ));
        }
        assert_eq!(create_descriptor_argument(libc::SYS_splice), Some(2));
        assert_eq!(create_descriptor_argument(libc::SYS_tee), Some(1));
        assert_eq!(
            create_unix_socket_type("0000: 00000002 00000000 00000000 0001 01 17", 17),
            Some(1)
        );
        assert_eq!(
            create_unix_socket_type("0000: 00000002 00000000 00000000 0001 01 17", 18),
            None
        );
    }
}
