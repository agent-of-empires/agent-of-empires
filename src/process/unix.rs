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

pub(super) fn try_wait_status_hook(
    child: &mut Child,
) -> std::io::Result<Option<std::process::ExitStatus>> {
    use nix::libc;
    // nix gates `waitid` off on macOS, so call libc directly on every Unix.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    if unsafe { libc::waitid(libc::P_PID, child.id() as libc::id_t, &mut info, flags) } < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            return Ok(None);
        }
        return Err(error);
    }
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    // Retain the exited leader until cleanup so its group ID cannot be reused.
    kill_process_group(child);
    child.wait().map(Some)
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
