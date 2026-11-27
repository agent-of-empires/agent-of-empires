use std::process::{Child, Command};

pub(super) fn independent_terminal_output() -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsFd, AsRawFd};
    let stdout = std::io::stdout();
    let output = stdout.as_fd();
    let mut name = [0 as libc::c_char; 4096];
    // SAFETY: ttyname_r writes only into the supplied live buffer.
    let error = unsafe { libc::ttyname_r(output.as_raw_fd(), name.as_mut_ptr(), name.len()) };
    let path = if error == 0 {
        // SAFETY: successful ttyname_r NUL-terminated the buffer.
        Some(unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) })
    } else {
        // SAFETY: stdout is still borrowed above.
        if unsafe { libc::isatty(output.as_raw_fd()) } != 1 {
            return Err(std::io::Error::from_raw_os_error(error));
        }
        None
    };
    reopen_terminal_output(output, path)
}

pub(super) fn open_terminal_path(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(path)
}

fn reopen_terminal_output(
    output: std::os::fd::BorrowedFd<'_>,
    path: Option<&std::ffi::CStr>,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};
    let file = match path {
        Some(path) => open_terminal_path(std::path::Path::new(std::ffi::OsStr::from_bytes(
            path.to_bytes(),
        )))?,
        None => super::platform::reopen_terminal_fd(output).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("cannot reopen terminal stdout independently: {error}"),
            )
        })?,
    };
    let output = std::fs::File::from(output.try_clone_to_owned()?);
    let actual = file.metadata()?;
    let expected = output.metadata()?;
    // SAFETY: both descriptors remain open throughout verification.
    if unsafe { libc::isatty(file.as_raw_fd()) } != 1
        || (actual.dev(), actual.ino(), actual.rdev())
            != (expected.dev(), expected.ino(), expected.rdev())
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "stdout terminal identity changed",
        ));
    }
    Ok(file)
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn unnamed_terminal_fallback_keeps_independent_output_flags() {
    use nix::fcntl::{fcntl, FcntlArg, OFlag};
    use std::io::Write;
    use std::os::fd::AsFd;

    let (mut master, slave) = crate::tui::terminal_output::tests::private_terminal();
    fcntl(&slave, FcntlArg::F_SETFL(OFlag::empty())).unwrap();
    let mut output = reopen_terminal_output(slave.as_fd(), None).unwrap();
    let original = OFlag::from_bits_truncate(fcntl(&slave, FcntlArg::F_GETFL).unwrap());
    let reopened = OFlag::from_bits_truncate(fcntl(&output, FcntlArg::F_GETFL).unwrap());
    assert!(!original.contains(OFlag::O_NONBLOCK));
    assert!(reopened.contains(OFlag::O_NONBLOCK));
    output.write_all(b"fallback").unwrap();
    let mut bytes = Vec::new();
    crate::tui::terminal_output::tests::read_terminal_until(&mut master, &mut bytes, b"fallback");
    assert_eq!(bytes, b"fallback");
}

pub(super) fn validate_recovery_file(file: &std::fs::File) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.uid() == nix::unistd::geteuid().as_raw()
            && metadata.nlink() == 1
            && metadata.mode() & 0o022 == 0,
        "Recovery evidence must be an owned, non-shared regular file"
    );
    Ok(())
}

pub(super) fn close_frozen_payload() -> std::io::Result<()> {
    nix::unistd::close(4).map_err(std::io::Error::from)
}

pub(super) fn inherit_file_at_fd(
    command: &mut Command,
    file: std::fs::File,
    target: std::os::fd::RawFd,
) {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;

    // SAFETY: lseek, dup2 and fcntl are async-signal-safe, the only pre_exec requirement.
    unsafe {
        command.pre_exec(move || {
            let source = file.as_raw_fd();
            // macOS `/dev/fd/N` shares this offset instead of reopening the file.
            let result = if libc::lseek(source, 0, libc::SEEK_SET) < 0 {
                -1
            } else if source == target {
                libc::fcntl(target, libc::F_SETFD, 0)
            } else {
                libc::dup2(source, target)
            };
            if result < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

pub(super) fn exec_command(command: &mut Command) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    command.exec()
}

pub(super) fn restore_environment_value(
    value: super::FrozenEnvironmentValue<String, Vec<u8>>,
) -> std::io::Result<std::ffi::OsString> {
    use std::os::unix::ffi::OsStringExt;
    Ok(match value {
        super::FrozenEnvironmentValue::Text(value) => value.into(),
        super::FrozenEnvironmentValue::Bytes(value) => std::ffi::OsString::from_vec(value),
    })
}

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

pub(super) fn try_wait_group_status(
    child: &mut Child,
    cleanup: super::ProcessGroupCleanup,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    let Some(success) = child_exit_success_unreaped(child.id())? else {
        return Ok(None);
    };
    let transferred = cleanup == super::ProcessGroupCleanup::OnFailure
        && success
        && cancel.is_none_or(|cancel| !cancel.is_cancelled());
    // Keep the original process group reserved until the cleanup signal.
    if !transferred {
        kill_process_group(child);
    }
    child.wait().map(Some)
}

pub(super) fn child_exited_unreaped(pid: u32) -> std::io::Result<bool> {
    child_exit_success_unreaped(pid).map(|success| success.is_some())
}

fn child_exit_success_unreaped(pid: u32) -> std::io::Result<Option<bool>> {
    use nix::libc;
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    if unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(error);
    }
    let observed = unsafe { info.si_pid() };
    if observed == 0 {
        return Ok(None);
    }
    if observed != pid as libc::pid_t
        || !matches!(
            info.si_code,
            libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED
        )
    {
        return Err(std::io::Error::other(
            "unexpected owned-child wait observation",
        ));
    }
    Ok(Some(
        info.si_code == libc::CLD_EXITED && unsafe { info.si_status() } == 0,
    ))
}

fn signal_process_group(child: &Child, signal: nix::sys::signal::Signal) {
    let Ok(pid) = i32::try_from(child.id()) else {
        return;
    };
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), signal);
}

pub(crate) fn detach_daemon_stdin() -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let null = std::fs::File::open("/dev/null")?;
    // Replace the inherited transaction descriptor without leaving fd0 vacant.
    if unsafe { nix::libc::dup2(null.as_raw_fd(), nix::libc::STDIN_FILENO) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
