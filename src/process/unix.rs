use std::process::{Child, Command};

pub(super) fn close_frozen_payload() -> std::io::Result<()> {
    nix::unistd::close(4).map_err(std::io::Error::from)
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

fn signal_process_group(child: &Child, signal: nix::sys::signal::Signal) {
    let Ok(pid) = i32::try_from(child.id()) else {
        return;
    };
    let _ = nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), signal);
}
