//! Process utilities for tmux session management

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::errno::Errno;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::sys::signal::{kill, Signal};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::unistd::Pid;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;
pub(crate) fn validate_recovery_file(file: &std::fs::File) -> anyhow::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        unix::validate_recovery_file(file)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = file;
        anyhow::bail!("Ownership recovery is unavailable on this platform")
    }
}

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    pub(super) fn configure_process_group(_: &mut std::process::Command) {}

    pub(super) fn kill_process_group(_: &std::process::Child) {}

    pub(super) fn terminate_process_group(_: &std::process::Child) {}

    pub(super) fn try_wait_status_hook(
        child: &mut std::process::Child,
    ) -> std::io::Result<Option<std::process::ExitStatus>> {
        child.try_wait()
    }
    pub(super) fn restore_environment_value(
        _: super::FrozenEnvironmentValue<String, Vec<u8>>,
    ) -> std::io::Result<std::ffi::OsString> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "frozen environments require Linux or macOS",
        ))
    }
    pub(super) fn close_frozen_payload() -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "frozen environments require Linux or macOS",
        ))
    }
    pub(super) fn exec_command(_: &mut std::process::Command) -> std::io::Error {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "process replacement requires Linux or macOS",
        )
    }
}
#[cfg(target_os = "linux")]
pub(crate) fn effective_uid() -> u32 {
    platform::effective_uid()
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn metadata_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    platform::metadata_identity(metadata)
}
pub(crate) fn open_read_nonblocking(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        platform::open_read_nonblocking(path)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        std::fs::File::open(path)
    }
}

#[cfg(target_os = "linux")]
pub(crate) use linux::runtime_io;
pub(crate) const LIVE_PANE_ENV_KEYS: [&str; 3] = ["TERM", "TMUX", "TMUX_PANE"];

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct NativeLaunchPayload<T, E> {
    pub(crate) argv: Vec<String>,
    pub(crate) cwd: T,
    pub(crate) environment: Vec<(E, E)>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum FrozenEnvironmentValue<T, B> {
    Text(T),
    Bytes(B),
}

pub(crate) fn frozen_environment_value(
    value: &std::ffi::OsStr,
) -> FrozenEnvironmentValue<&str, &[u8]> {
    match value.to_str() {
        Some(value) => FrozenEnvironmentValue::Text(value),
        None => FrozenEnvironmentValue::Bytes(value.as_encoded_bytes()),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    thread_local! { static EXECUTABLE: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) }; }
    pub(crate) struct NativeLaunchExecutable;
    impl NativeLaunchExecutable {
        pub(crate) fn install(path: std::path::PathBuf) -> Self {
            EXECUTABLE.with(|slot| {
                assert!(slot.borrow().is_none());
                *slot.borrow_mut() = Some(path);
            });
            Self
        }
    }
    impl Drop for NativeLaunchExecutable {
        fn drop(&mut self) {
            EXECUTABLE.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }
    pub(super) fn executable() -> Option<std::path::PathBuf> {
        EXECUTABLE.with(|slot| slot.borrow().clone())
    }
}

pub(crate) fn executable_for_native_launch() -> std::io::Result<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(executable) = test_support::executable() {
        return Ok(executable);
    }
    let executable = std::env::current_exe()?;
    #[cfg(target_os = "linux")]
    return linux::launch_executable(executable);
    #[cfg(not(target_os = "linux"))]
    Ok(executable)
}

fn read_native_launch_payload() -> anyhow::Result<(
    NativeLaunchPayload<String, FrozenEnvironmentValue<String, Vec<u8>>>,
    usize,
)> {
    use anyhow::Context;
    let file = std::fs::File::open("/dev/fd/4").context("opening native launch descriptor")?;
    let payload = serde_json::from_reader::<
        _,
        NativeLaunchPayload<String, FrozenEnvironmentValue<String, Vec<u8>>>,
    >(file);
    platform::close_frozen_payload().context("closing native launch descriptor")?;
    let payload = payload.context("reading native launch descriptor")?;
    let program_at = payload
        .argv
        .iter()
        .position(|word| {
            !word
                .split_once('=')
                .is_some_and(|(key, _)| crate::session::environment::is_valid_env_key(key))
        })
        .context("native launch program is missing")?;
    anyhow::ensure!(
        !payload.argv[program_at].is_empty(),
        "native launch program is empty"
    );
    Ok((payload, program_at))
}

#[doc(hidden)]
pub fn exec_frozen_environment() -> anyhow::Result<()> {
    let (payload, program_at) = read_native_launch_payload()?;
    let mut command = Command::new(&payload.argv[program_at]);
    command
        .args(&payload.argv[program_at + 1..])
        .current_dir(payload.cwd)
        .env_clear();
    for (key, value) in payload.environment {
        command.env(
            platform::restore_environment_value(key)?,
            platform::restore_environment_value(value)?,
        );
    }
    for key in LIVE_PANE_ENV_KEYS {
        command.env_remove(key);
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    for assignment in &payload.argv[..program_at] {
        let (key, value) = assignment.split_once('=').unwrap();
        command.env(key, value);
    }
    Err(platform::exec_command(&mut command).into())
}

#[doc(hidden)]
pub fn exec_post_login_opencode_yolo() -> anyhow::Result<()> {
    use anyhow::Context;
    let (payload, program_at) = read_native_launch_payload()?;
    let mut command = Command::new(&payload.argv[program_at]);
    command
        .current_dir(payload.cwd)
        .env_clear()
        .envs(std::env::vars_os());
    for (key, value) in payload.environment {
        command.env(
            platform::restore_environment_value(key)?,
            platform::restore_environment_value(value)?,
        );
    }
    for assignment in &payload.argv[..program_at] {
        let (key, value) = assignment.split_once('=').unwrap();
        command.env(key, value);
    }
    let agent =
        crate::agents::get_agent("opencode").context("OpenCode agent definition is missing")?;
    let generation = crate::agents::agent_generation_for(agent, &command);
    command.args(&payload.argv[program_at + 1..]);
    match agent
        .yolo
        .as_ref()
        .and_then(|mode| mode.resolve(generation))
    {
        Some(crate::agents::YoloMode::CliFlag(flag)) => {
            command.arg(flag);
        }
        Some(crate::agents::YoloMode::EnvVar(key, value)) => {
            command.env(key, value);
        }
        Some(crate::agents::YoloMode::AlwaysYolo) => {}
        Some(crate::agents::YoloMode::EitherGeneration { .. }) => {
            anyhow::bail!("unresolved OpenCode approval mechanism")
        }
        None => eprintln!("OpenCode --help did not establish a generation; YOLO was not applied"),
    }
    Err(platform::exec_command(&mut command).into())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use platform::unix_peer_uid;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::detach_daemon_stdin;

/// Lower the child's scheduling and I/O priority where the OS supports it.
pub(crate) fn throttle_child(cmd: &mut std::process::Command) {
    #[cfg(target_os = "macos")]
    macos::throttle_child(cmd);
    #[cfg(not(target_os = "macos"))]
    let _ = cmd;
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use platform::HAS_CODEX_MANAGED_PREFERENCES;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) const HAS_CODEX_MANAGED_PREFERENCES: bool = true;

/// System memory + agent-count sampling for the TUI diagnostics strip.
pub(crate) mod metrics;

pub mod systemd;

/// Protocol-agnostic plumbing for supervised worker subprocesses, lifted
/// out of `src/acp/` so the future plugin host can reuse it.
pub mod worker;

/// On-disk registry of detached ACP worker subprocesses (pid, socket path,
/// build version, `stored_acp_session_id`). Colocated here with the
/// protocol-agnostic `worker` substrate it builds on, so the plugin host can
/// reuse that substrate directly. Dependency direction is one-way: consumers
/// point down to `process`, never to each other.
pub mod worker_registry;

pub mod prime;
pub mod runner;

const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PROCESS_GROUP_TERMINATION_GRACE: Duration = Duration::from_millis(250);

pub(crate) fn spawn_process_group(command: &mut Command) -> std::io::Result<Child> {
    platform::configure_process_group(command);
    command.spawn()
}

/// Keep the owned leader unreaped until the last group signal, reserving its PID.
pub(crate) fn terminate_and_reap_process_group(
    child: &mut Child,
    grace: Duration,
) -> std::io::Result<()> {
    platform::terminate_process_group(child);
    std::thread::sleep(grace);
    platform::kill_process_group(child);
    let _ = child.kill();
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut poll = Duration::from_millis(1);
    loop {
        if child.try_wait()?.is_some() {
            return Ok(());
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "terminated process group leader could not be reaped",
            ));
        }
        std::thread::sleep(poll.min(deadline.saturating_duration_since(now)));
        poll = (poll * 2).min(WAIT_POLL_INTERVAL);
    }
}

/// Atomically publish a directory entry without replacing an existing entry.
/// Both names are relative to retained directory descriptors; there is no
/// check-then-rename fallback on platforms without an exclusive rename syscall.
#[cfg(unix)]
pub(crate) fn rename_exclusive(
    source_dir: &std::os::fd::OwnedFd,
    source: &std::ffi::OsStr,
    destination_dir: &std::os::fd::OwnedFd,
    destination: &std::ffi::OsStr,
) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        platform::rename_exclusive(source_dir, source, destination_dir, destination)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (source_dir, source, destination_dir, destination);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "exclusive directory publication is unsupported on this platform",
        ))
    }
}

/// Wait for `child` to exit, killing and reaping it if it outlives `timeout`.
/// Returns `Ok(None)` when the timeout fired and the child was killed.
///
/// Only use this directly when the child's stdout/stderr are not piped (or
/// are drained elsewhere): a full pipe buffer can wedge the child before the
/// deadline. [`run_with_timeout`] handles piped output safely.
pub fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
) -> std::io::Result<Option<ExitStatus>> {
    wait_with_timeout_inner(child, timeout, false, false, None)
}

/// When `kill_process_group` is true, the caller MUST have configured `child`
/// as a process-group leader, so its process-group id equals `child.id()`.
/// Otherwise the kill path could signal the parent's process group.
fn wait_with_timeout_inner(
    child: &mut Child,
    timeout: Duration,
    kill_process_group: bool,
    cleanup_on_exit: bool,
    cancel: Option<&CancellationToken>,
) -> std::io::Result<Option<ExitStatus>> {
    let mut deadline = Instant::now() + timeout;
    let termination_grace = (timeout / 4).min(PROCESS_GROUP_TERMINATION_GRACE);
    let mut terminate_at = deadline.checked_sub(termination_grace).unwrap_or(deadline);
    let mut termination_requested = false;
    // Most children exit within a few ms; start fine and back off to the cap.
    let mut poll = Duration::from_millis(1);
    loop {
        if !termination_requested {
            let status = if cleanup_on_exit {
                platform::try_wait_status_hook(child)?
            } else {
                child.try_wait()?
            };
            if let Some(status) = status {
                return Ok(Some(status));
            }
        }
        let now = Instant::now();
        if !termination_requested && cancel.is_some_and(CancellationToken::is_cancelled) {
            deadline = deadline.min(now + termination_grace);
            terminate_at = now;
        }
        if kill_process_group && !termination_requested && now >= terminate_at {
            platform::terminate_process_group(child);
            termination_requested = true;
            continue;
        }
        // A cancellation has already shortened the deadline above, so reaching
        // it here is the escalation step; testing the token again would skip
        // the SIGTERM the group is entitled to and its traps.
        if now >= deadline {
            if kill_process_group {
                platform::kill_process_group(child);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(poll.min(deadline.saturating_duration_since(now)));
        poll = (poll * 2).min(WAIT_POLL_INTERVAL);
    }
}

/// Spawn `cmd` with stdout/stderr captured in temporary regular files and wait
/// for it to exit, killing it if it outlives `timeout`. Returns `Ok(None)` when
/// the timeout fired and the child was killed; `Err` covers spawn/wait failures.
///
/// Regular temporary files prevent a full pipe buffer from wedging the child
/// before the deadline and keep capture independent of inherited writers.
/// The caller keeps control of stdin; pipe it to null when the child might
/// prompt (SSH passphrases, credential helpers).
pub fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> std::io::Result<Option<Output>> {
    run_with_timeout_inner(cmd, timeout, false, None)
}

/// [`run_with_timeout`] that also kills the child once `cancel` fires, returning `Ok(None)`.
pub fn run_until_cancelled(
    cmd: &mut Command,
    timeout: Duration,
    cancel: &CancellationToken,
) -> std::io::Result<Option<Output>> {
    run_with_timeout_inner(cmd, timeout, false, Some(cancel))
}

/// [`run_with_timeout`] variant that makes the child a process-group leader
/// and kills the whole group on timeout on Linux and macOS. Other platforms
/// still bound output capture independently of descendant-owned handles and
/// fall back to killing and reaping the direct child.
pub fn run_with_timeout_process_group(
    cmd: &mut Command,
    timeout: Duration,
) -> std::io::Result<Option<Output>> {
    platform::configure_process_group(cmd);
    run_with_timeout_inner(
        cmd,
        timeout,
        cfg!(any(target_os = "linux", target_os = "macos")),
        None,
    )
}

/// Run a command without capturing output; cancellation terminates and reaps
/// its process group with the same bounded grace as a timeout.
/// Remaining process-group members are killed before reaping an exited leader.
pub(crate) fn run_status_with_timeout_process_group(
    cmd: &mut Command,
    timeout: Duration,
    cancel: Option<&CancellationToken>,
) -> std::io::Result<Option<ExitStatus>> {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Ok(None);
    }
    platform::configure_process_group(cmd);
    let mut child = cmd.spawn()?;
    wait_with_timeout_inner(
        &mut child,
        timeout,
        cfg!(any(target_os = "linux", target_os = "macos")),
        true,
        cancel,
    )
}
pub(crate) fn run_until_cancelled_process_group(
    cmd: &mut Command,
    timeout: Duration,
    cancel: &CancellationToken,
) -> std::io::Result<Option<Output>> {
    platform::configure_process_group(cmd);
    run_with_timeout_inner(
        cmd,
        timeout,
        cfg!(any(target_os = "linux", target_os = "macos")),
        Some(cancel),
    )
}

fn run_with_timeout_inner(
    cmd: &mut Command,
    timeout: Duration,
    kill_process_group: bool,
    cancel: Option<&CancellationToken>,
) -> std::io::Result<Option<Output>> {
    let mut stdout_file = tempfile::NamedTempFile::new()?;
    let mut stderr_file = tempfile::NamedTempFile::new()?;
    cmd.stdout(Stdio::from(stdout_file.reopen()?));
    cmd.stderr(Stdio::from(stderr_file.reopen()?));
    let mut child = cmd.spawn()?;

    let Some(status) =
        wait_with_timeout_inner(&mut child, timeout, kill_process_group, false, cancel)?
    else {
        return Ok(None);
    };
    // Regular files, rather than pipes drained by background threads, keep
    // output capture bounded when a descendant outlives the direct child. A
    // read reaches the current EOF without waiting for every inherited writer
    // handle to close.
    let mut stdout = Vec::new();
    stdout_file.as_file_mut().read_to_end(&mut stdout)?;
    let mut stderr = Vec::new();
    stderr_file.as_file_mut().read_to_end(&mut stderr)?;
    Ok(Some(Output {
        status,
        stdout,
        stderr,
    }))
}

/// Reset `SIGINT`/`SIGQUIT` to their default disposition.
///
/// `SIG_IGN` (unlike a caught handler) survives `exec()` per POSIX. A
/// child spawned while `IgnoreSignalsGuard` (`src/tui/app.rs`) has
/// SIGINT/SIGQUIT ignored on aoe itself would otherwise silently inherit
/// that ignore, leaving no way for the user to Ctrl+C out of it. Call
/// this from a `pre_exec` closure (see [`reset_signals_on_exec`]) on any
/// `Command` spawned from inside that guard's window: tmux attach, the
/// editor shell-out, update helpers (brew/tar/sudo).
#[cfg(unix)]
pub fn reset_ignored_signals_before_exec() -> std::io::Result<()> {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

    let default = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
    // SAFETY: called from a `pre_exec` closure, which runs in the child
    // between fork and exec where only async-signal-safe operations are
    // permitted. SIG_DFL is async-signal-safe per POSIX, the only
    // requirement for sigaction calls made outside a signal handler.
    // `io::Error::from_raw_os_error` (unlike `Error::other`, which boxes
    // its argument) builds the error without allocating, so this stays
    // safe to call between fork and exec.
    unsafe { sigaction(Signal::SIGINT, &default) }
        .map_err(|errno| std::io::Error::from_raw_os_error(errno as i32))?;
    // SAFETY: see above.
    unsafe { sigaction(Signal::SIGQUIT, &default) }
        .map_err(|errno| std::io::Error::from_raw_os_error(errno as i32))?;
    Ok(())
}

/// Wire [`reset_ignored_signals_before_exec`] into `cmd` via `pre_exec` so
/// its child doesn't inherit whatever SIGINT/SIGQUIT disposition happens
/// to be in effect on the parent at spawn time.
#[cfg(unix)]
pub fn reset_signals_on_exec(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;

    // SAFETY: the closure only calls `sigaction`, which is
    // async-signal-safe per POSIX, the only requirement for a `pre_exec`
    // closure running between fork and exec.
    unsafe {
        cmd.pre_exec(reset_ignored_signals_before_exec);
    }
}

/// Recursively collect all descendant PIDs of `pid` using a pre-built
/// parent -> children map. Shared by the per-OS `collect_pid_tree`
/// implementations, which each build the map their own way (a `/proc`
/// scan on Linux, a `ps` parse on macOS).
fn collect_descendants_from_map(
    pid: u32,
    children_map: &HashMap<u32, Vec<u32>>,
    pids: &mut Vec<u32>,
) {
    if let Some(children) = children_map.get(&pid) {
        for &child_pid in children {
            pids.push(child_pid);
            collect_descendants_from_map(child_pid, children_map, pids);
        }
    }
}

/// The live shell PID in the exact session’s first pane, the agent pane.
pub fn get_pane_pid(session_name: &str) -> Option<u32> {
    let pane = crate::tmux::first_pane_id(session_name)?;
    // The pane id must travel alone: `session:%id` reads as a window
    // spec and falls back to the window's active pane (tmux 3.7c).
    let output = crate::tmux::tmux_command()
        .args([
            "display-message",
            "-t",
            &pane,
            "-p",
            "#{pane_dead}\t#{pane_pid}",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        // Guarded: hot poll path. Only formats arguments when the user has
        // enabled `process.ppid=trace` (or finer) on their filter.
        if tracing::enabled!(target: "process.ppid", tracing::Level::TRACE) {
            tracing::trace!(
                target: "process.ppid",
                session = %session_name,
                status = ?output.status,
                "display-message failed; no pane pid",
            );
        }
        return None;
    }

    let pid = std::str::from_utf8(&output.stdout)
        .ok()
        .and_then(|value| value.trim().strip_prefix("0\t"))
        .and_then(|pid| pid.parse::<u32>().ok())
        .filter(|pid| *pid > 0 && i32::try_from(*pid).is_ok());
    if tracing::enabled!(target: "process.ppid", tracing::Level::TRACE) {
        tracing::trace!(
            target: "process.ppid",
            session = %session_name,
            pid = ?pid,
            "resolved pane pid",
        );
    }
    pid
}

/// For a single pass over the process table, decide for each candidate `i`
/// whether a live process belongs to it. Every supplied signal must match:
/// an exact environment entry, a command-line substring, and/or an executable
/// argv token whose basename is exact. The three slices must have equal length.
/// Empty or absent signals never match on their own. Best-effort: an unreadable
/// process table yields all `false`.
///
/// Startup recovery uses this after losing an old tmux socket. Hook-enabled
/// agents require their exact `AOE_INSTANCE_ID=<id>` marker plus their built-in
/// executable token. This remains stable across native conversation rotation
/// while rejecting helpers that only inherited the environment. Non-hook
/// agents use the session id in the command line. The host `docker exec` argv
/// carries both forms for sandboxed sessions.
///
/// Batching keeps this to one `/proc` walk (or one `ps` fork) regardless of
/// candidate count. Callers on an async runtime must wrap it in
/// `tokio::task::spawn_blocking`.
pub fn processes_matching(
    env_needles: &[String],
    cmdline_needles: &[Option<String>],
    executable_needles: &[Option<String>],
) -> Vec<bool> {
    debug_assert_eq!(env_needles.len(), cmdline_needles.len());
    debug_assert_eq!(env_needles.len(), executable_needles.len());
    let n = env_needles
        .len()
        .min(cmdline_needles.len())
        .min(executable_needles.len());
    if n == 0 {
        return Vec::new();
    }
    #[cfg(target_os = "linux")]
    {
        linux::processes_matching(
            &env_needles[..n],
            &cmdline_needles[..n],
            &executable_needles[..n],
        )
    }
    #[cfg(target_os = "macos")]
    {
        macos::processes_matching(
            &env_needles[..n],
            &cmdline_needles[..n],
            &executable_needles[..n],
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        vec![false; n]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct PrimeProcessIdentity {
    pub pid: u32,
    pub start_id: String,
}

pub(super) struct PrimeProcessSample {
    pub identity: PrimeProcessIdentity,
    pub parent: u32,
    pub marked: bool,
}

fn prime_selection_mask(
    samples: &[PrimeProcessSample],
    known: &[PrimeProcessIdentity],
) -> Vec<bool> {
    use std::collections::HashMap;
    let known: HashMap<_, _> = known
        .iter()
        .map(|identity| (identity.pid, &identity.start_id))
        .collect();
    let mut children: HashMap<u32, Vec<usize>> = HashMap::new();
    let mut selected = vec![false; samples.len()];
    let mut pending = Vec::new();
    for (index, sample) in samples.iter().enumerate() {
        children.entry(sample.parent).or_default().push(index);
        if sample.marked
            || known
                .get(&sample.identity.pid)
                .is_some_and(|start| **start == sample.identity.start_id)
        {
            selected[index] = true;
            pending.push(index);
        }
    }
    while let Some(index) = pending.pop() {
        if let Some(descendants) = children.get(&samples[index].identity.pid) {
            for &child in descendants {
                if !selected[child] {
                    selected[child] = true;
                    pending.push(child);
                }
            }
        }
    }
    selected
}

pub(crate) fn select_prime_processes(
    samples: Vec<PrimeProcessSample>,
    known: &[PrimeProcessIdentity],
) -> Vec<PrimeProcessIdentity> {
    let selected = prime_selection_mask(&samples, known);
    samples
        .into_iter()
        .zip(selected)
        .filter_map(|(sample, selected)| selected.then_some(sample.identity))
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn select_stable_prime_processes(
    mut samples: Vec<PrimeProcessSample>,
    known: &[PrimeProcessIdentity],
) -> anyhow::Result<Vec<PrimeProcessIdentity>> {
    let selected = prime_selection_mask(&samples, known);
    let mut index = 0;
    samples.retain(|_| {
        let keep = selected[index];
        index += 1;
        keep
    });
    let handles = samples
        .iter()
        .map(|sample| platform::ProcessHandle::capture(&sample.identity))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut index = 0;
    let mut failure = None;
    samples.retain(|_| {
        let live = match &handles[index] {
            Some(handle) => match handle.exited() {
                Ok(exited) => !exited,
                Err(error) => {
                    failure = Some(error);
                    false
                }
            },
            None => false,
        };
        index += 1;
        live
    });
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(select_prime_processes(samples, known))
}

pub(crate) fn prime_processes(
    marker: &str,
    socket: &Path,
    known: &[PrimeProcessIdentity],
) -> anyhow::Result<Vec<PrimeProcessIdentity>> {
    #[cfg(target_os = "linux")]
    {
        linux::prime_processes(marker, socket, known)
    }
    #[cfg(target_os = "macos")]
    {
        macos::prime_processes(marker, socket, known)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (marker, socket, known);
        anyhow::bail!("Prime process proof unsupported on this platform")
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct ProcessBirthIdentity {
    pub boot_id: String,
    pub start_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ProcessBirthFingerprint {
    boot: [u8; 32],
    process: [u8; 32],
}

impl ProcessBirthIdentity {
    pub(crate) fn fingerprint(&self) -> ProcessBirthFingerprint {
        use sha2::{Digest, Sha256};
        ProcessBirthFingerprint {
            boot: Sha256::digest(self.boot_id.as_bytes()).into(),
            process: Sha256::digest(self.start_id.as_bytes()).into(),
        }
    }
}

/// A handle to one physical process incarnation, never its process group.
/// Callers must establish their own custody before sending any signal.
pub(crate) struct CapturedProcess {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    handle: platform::ProcessHandle,
}

impl CapturedProcess {
    pub(crate) fn capture(
        pid: u32,
        expected: &ProcessBirthIdentity,
    ) -> anyhow::Result<Option<Self>> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            if process_birth_identity(pid)?.as_ref() != Some(expected) {
                return Ok(None);
            }
            let identity = PrimeProcessIdentity {
                pid,
                start_id: expected.start_id.clone(),
            };
            let Some(handle) = platform::ProcessHandle::capture(&identity)? else {
                return Ok(None);
            };
            if process_birth_identity(pid)?.as_ref() != Some(expected) || handle.exited()? {
                return Ok(None);
            }
            Ok(Some(Self { handle }))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (pid, expected);
            anyhow::bail!("incarnation-bound process control unsupported on this platform")
        }
    }

    pub(crate) fn exited(&self) -> anyhow::Result<bool> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.handle.exited()
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        anyhow::bail!("incarnation-bound process control unsupported on this platform")
    }

    pub(crate) fn signal(&self, signal: i32) -> anyhow::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.handle.signal(signal)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = signal;
            anyhow::bail!("incarnation-bound process control unsupported on this platform")
        }
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod captured_process_tests {
    use super::*;

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn waiting_child() -> ChildGuard {
        let ready = tempfile::tempdir().unwrap();
        let path = ready.path().join("ready");
        let child = Command::new("sh")
            .args(["-c", ": > \"$1\"; read -r line", "sh"])
            .arg(&path)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let child = ChildGuard(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !path.exists() {
            assert!(Instant::now() < deadline, "child did not become ready");
            std::thread::sleep(Duration::from_millis(5));
        }
        child
    }

    #[test]
    fn single_process_capture_refuses_another_birth() {
        let child = waiting_child();
        let pid = child.0.id();
        let birth = process_birth_identity(pid).unwrap().unwrap();
        assert!(CapturedProcess::capture(pid, &birth).unwrap().is_some());
        let mut wrong = birth.clone();
        wrong.start_id.push_str("-not-this-incarnation");
        assert!(CapturedProcess::capture(pid, &wrong).unwrap().is_none());
        let mut wrong = birth;
        wrong.boot_id.push_str("-not-this-boot");
        assert!(CapturedProcess::capture(pid, &wrong).unwrap().is_none());
    }

    #[test]
    fn single_process_handle_observes_exit_before_parent_reaps() {
        let mut child = waiting_child();
        let pid = child.0.id();
        let birth = process_birth_identity(pid).unwrap().unwrap();
        let process = CapturedProcess::capture(pid, &birth).unwrap().unwrap();
        drop(child.0.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !process.exited().unwrap() {
            assert!(
                Instant::now() < deadline,
                "child physical exit was not observed"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Linux zombies still satisfy kill(pid, 0), but cannot own a writer.
        #[cfg(target_os = "linux")]
        assert!(nix::sys::signal::kill(Pid::from_raw(pid as i32), None).is_ok());
        child.0.wait().unwrap();
        assert!(process.exited().unwrap());
    }
}

pub(crate) struct RunnerProcessGuard {
    pid: u32,
    fingerprint: Option<ProcessBirthFingerprint>,
    obsolete: bool,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    members: parking_lot::Mutex<Vec<(PrimeProcessIdentity, platform::ProcessHandle)>>,
    #[cfg(target_os = "linux")]
    group_anchor: Option<platform::ProcessHandle>,
}

impl RunnerProcessGuard {
    pub(crate) fn capture(
        pid: u32,
        fingerprint: Option<ProcessBirthFingerprint>,
    ) -> anyhow::Result<Self> {
        use sha2::{Digest, Sha256};
        anyhow::ensure!(pid != 0, "invalid runner PID");
        let obsolete_boot = if let Some(expected) = fingerprint {
            let boot =
                boot_id().ok_or_else(|| anyhow::anyhow!("current boot identity unavailable"))?;
            expected.boot != <[u8; 32]>::from(Sha256::digest(boot.as_bytes()))
        } else {
            false
        };
        let before = if obsolete_boot {
            None
        } else {
            runner_birth_identity(pid)?
        };
        let obsolete = obsolete_boot
            || before.as_ref().is_some_and(|birth| {
                Some(birth.fingerprint()) != fingerprint && fingerprint.is_some()
            });
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            #[cfg(target_os = "linux")]
            let group_anchor = if !obsolete {
                if let Some(birth) = &before {
                    anyhow::ensure!(fingerprint.is_some(), "runner birth identity is unattested; confirm external settlement before recovery");
                    let identity = PrimeProcessIdentity {
                        pid,
                        start_id: birth.start_id.clone(),
                    };
                    match platform::ProcessHandle::capture_group(&identity)? {
                        Some(handle) if handle.group_executing()?.is_some() => Some(handle),
                        _ => None,
                    }
                } else {
                    None
                }
            } else {
                None
            };
            let guard = Self {
                pid,
                fingerprint,
                obsolete,
                members: parking_lot::Mutex::new(Vec::new()),
                #[cfg(target_os = "linux")]
                group_anchor,
            };
            if !obsolete {
                #[cfg(target_os = "linux")]
                if guard.group_anchor.is_some() {
                    return Ok(guard);
                }
                let mut members = guard.members.lock();
                guard.refresh(&mut members)?;
            }
            Ok(guard)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (before, obsolete);
            anyhow::bail!("incarnation-bound runner control unsupported")
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn refresh(
        &self,
        members: &mut Vec<(PrimeProcessIdentity, platform::ProcessHandle)>,
    ) -> anyhow::Result<()> {
        let before = runner_birth_identity(self.pid)?;
        let root_attested = before
            .as_ref()
            .is_some_and(|birth| Some(birth.fingerprint()) == self.fingerprint);
        anyhow::ensure!(
            self.fingerprint.is_some() || before.is_none(),
            "runner birth identity is unattested; confirm external settlement before recovery"
        );
        if before.is_none() {
            members.retain(|(identity, _)| identity.pid != self.pid);
        }
        let anchor = if root_attested {
            None
        } else {
            let mut anchor = None;
            for (index, (identity, handle)) in members.iter().enumerate() {
                if !handle.exited()? && platform::process_group_id(identity.pid)? == Some(self.pid)
                {
                    anchor = Some(index);
                    break;
                }
            }
            anchor
        };
        if !root_attested && anchor.is_none() {
            anyhow::ensure!(
                !runner_group_executing(self.pid)?,
                "runner group ownership is unattested; retain evidence until external settlement"
            );
            return Ok(());
        }
        let mut identities = runner_group_identities(self.pid)?;
        if let Some(birth) = before.as_ref().filter(|_| root_attested) {
            identities.push(PrimeProcessIdentity {
                pid: self.pid,
                start_id: birth.start_id.clone(),
            });
        }
        let mut added = Vec::new();
        for identity in identities {
            if !members.iter().any(|(known, _)| known == &identity) {
                if let Some(handle) = platform::ProcessHandle::capture(&identity)? {
                    added.push((identity, handle));
                }
            }
        }
        let stable = if root_attested {
            let after = runner_birth_identity(self.pid)?;
            anyhow::ensure!(
                after.is_none() || after == before,
                "runner incarnation changed during group capture"
            );
            after == before
        } else if let Some(index) = anchor {
            let (identity, handle) = &members[index];
            !handle.exited()? && platform::process_group_id(identity.pid)? == Some(self.pid)
        } else {
            false
        };
        if !stable {
            return Ok(());
        }
        members.extend(added);
        Ok(())
    }

    pub(crate) fn executing(&self) -> anyhow::Result<bool> {
        if self.obsolete {
            return Ok(false);
        }
        #[cfg(target_os = "linux")]
        if let Some(anchor) = &self.group_anchor {
            let alive = anchor
                .group_executing()?
                .ok_or_else(|| anyhow::anyhow!("kernel lost bound group signalling support"))?;
            if !alive {
                return Ok(false);
            }
            let executing = runner_group_executing(self.pid)?;
            return Ok(executing && anchor.group_executing()? == Some(true));
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let mut members = self.members.lock();
            self.refresh(&mut members)?;
            for (_, handle) in members.iter() {
                if !handle.exited()? {
                    return Ok(true);
                }
            }
        }
        runner_group_executing(self.pid)
    }

    pub(crate) fn signal(&self, signal: i32) -> anyhow::Result<()> {
        if self.obsolete {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        if let Some(anchor) = &self.group_anchor {
            return anchor.signal_group(signal);
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let mut members = self.members.lock();
            self.refresh(&mut members)?;
            for (_, handle) in members.iter() {
                handle.signal(signal)?;
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        anyhow::bail!("incarnation-bound runner signalling unsupported: {signal}");
        Ok(())
    }

    pub(crate) fn settle(&self, grace: Duration) -> anyhow::Result<()> {
        self.signal(libc::SIGTERM)?;
        let term_deadline = Instant::now() + grace;
        let kill_deadline = term_deadline + Duration::from_secs(5);
        let mut killed = false;
        while self.executing()? {
            let now = Instant::now();
            if now >= term_deadline && !killed {
                self.signal(libc::SIGKILL)?;
                killed = true;
            }
            anyhow::ensure!(
                now < kill_deadline,
                "runner termination unproven; retained incarnation-bound ownership"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    }
}

pub(crate) fn process_birth_identity(pid: u32) -> anyhow::Result<Option<ProcessBirthIdentity>> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let Some(start_id) = platform::process_start_id(pid)? else {
            return Ok(None);
        };
        let boot_id =
            boot_id().ok_or_else(|| anyhow::anyhow!("process boot identity unavailable"))?;
        Ok(Some(ProcessBirthIdentity { boot_id, start_id }))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        anyhow::bail!("process birth identity unsupported on this platform")
    }
}

pub(crate) fn process_birth_is_live(
    pid: u32,
    expected: &ProcessBirthIdentity,
) -> anyhow::Result<bool> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let Some(current) = process_birth_identity(pid)? else {
            return Ok(false);
        };
        if &current != expected {
            return Ok(false);
        }
        let identity = PrimeProcessIdentity {
            pid,
            start_id: current.start_id,
        };
        Ok(platform::ProcessHandle::capture(&identity)?.is_some())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (pid, expected);
        anyhow::bail!("process birth liveness unsupported on this platform")
    }
}

pub(crate) fn runner_birth_identity(pid: u32) -> anyhow::Result<Option<ProcessBirthIdentity>> {
    let birth = process_birth_identity(pid);
    #[cfg(target_os = "macos")]
    if birth.is_err() && platform::foreign_runner_group_absent(pid)? {
        return Ok(None);
    }
    birth
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapturedProcessTreeReceipt {
    boot_id: String,
    processes: Vec<PrimeProcessIdentity>,
}

impl CapturedProcessTreeReceipt {
    pub(crate) fn boot_identity(&self) -> &str {
        &self.boot_id
    }

    pub(crate) fn root_is_current(&self) -> anyhow::Result<bool> {
        let root = self
            .processes
            .first()
            .ok_or_else(|| anyhow::anyhow!("captured tool root is missing"))?;
        Ok(process_birth_identity(root.pid)?
            .is_some_and(|birth| birth.boot_id == self.boot_id && birth.start_id == root.start_id))
    }

    pub(crate) fn validate(&self, root_pid: u32) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.boot_id.is_empty()
                && self
                    .processes
                    .first()
                    .is_some_and(|root| root.pid == root_pid)
                && self.processes.iter().all(|identity| {
                    identity.pid > 0
                        && i32::try_from(identity.pid).is_ok()
                        && !identity.start_id.is_empty()
                })
                && self.processes.iter().enumerate().all(|(index, identity)| {
                    !self.processes[..index]
                        .iter()
                        .any(|known| known.pid == identity.pid)
                }),
            "captured tool retirement identity is unavailable"
        );
        Ok(())
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_CAPTURED_TREE_AFTER_ROOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) struct CapturedProcessTree {
    receipt: CapturedProcessTreeReceipt,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    handles: Vec<platform::ProcessHandle>,
}

impl CapturedProcessTree {
    pub(crate) fn capture(pid: u32) -> anyhow::Result<Self> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let start_id = platform::process_start_id(pid)?
                .ok_or_else(|| anyhow::anyhow!("tool process exited before capture"))?;
            let root = PrimeProcessIdentity { pid, start_id };
            let handle = platform::ProcessHandle::capture(&root)?
                .ok_or_else(|| anyhow::anyhow!("tool process exited before capture"))?;
            let samples = platform::process_tree_samples(&root)?;
            let selected = prime_selection_mask(&samples, std::slice::from_ref(&root));
            let mut handles = vec![handle];
            let mut identities = vec![root.clone()];
            for (sample, selected) in samples.into_iter().zip(selected) {
                if selected && sample.identity != root {
                    if let Some(handle) = platform::ProcessHandle::capture(&sample.identity)? {
                        handles.push(handle);
                        identities.push(sample.identity);
                    }
                }
            }
            anyhow::ensure!(
                platform::process_start_id(pid)?.as_ref() == Some(&root.start_id),
                "tool process incarnation changed during capture"
            );
            Ok(Self {
                handles,
                receipt: CapturedProcessTreeReceipt {
                    boot_id: boot_id()
                        .ok_or_else(|| anyhow::anyhow!("process boot identity unavailable"))?,
                    processes: identities,
                },
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = pid;
            anyhow::bail!("incarnation-bound signalling unsupported on this platform")
        }
    }

    pub(crate) fn receipt(&self) -> &CapturedProcessTreeReceipt {
        &self.receipt
    }

    pub(crate) fn resume(
        receipt: CapturedProcessTreeReceipt,
        root_pid: u32,
    ) -> anyhow::Result<Self> {
        receipt.validate(root_pid)?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let current_boot = boot_id()
            .filter(|identity| !identity.is_empty())
            .ok_or_else(|| anyhow::anyhow!("process boot identity unavailable"))?;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let mut handles = Vec::new();
            if receipt.boot_id == current_boot {
                handles.reserve(receipt.processes.len());
                for identity in &receipt.processes {
                    if let Some(handle) = platform::ProcessHandle::capture(identity)? {
                        handles.push(handle);
                    }
                }
            }
            Ok(Self { receipt, handles })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        anyhow::bail!("incarnation-bound signalling unsupported on this platform")
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn all_exited(&self) -> anyhow::Result<bool> {
        for handle in &self.handles {
            if !handle.exited()? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn settle(self, grace: Duration) -> anyhow::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            #[cfg(test)]
            if FAIL_CAPTURED_TREE_AFTER_ROOT.replace(false) {
                let root = self.handles.first().expect("captured root handle");
                root.signal(libc::SIGKILL)?;
                let deadline = Instant::now() + Duration::from_secs(5);
                while !root.exited()? {
                    anyhow::ensure!(Instant::now() < deadline, "injected root exit unproven");
                    std::thread::yield_now();
                }
                anyhow::bail!("injected retirement proof failure after root exit");
            }
            settle_process_handles(&self.handles, grace)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = grace;
            anyhow::bail!("incarnation-bound signalling unsupported on this platform")
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn settle_process_handles(
    handles: &[platform::ProcessHandle],
    grace: Duration,
) -> anyhow::Result<()> {
    for handle in handles {
        handle.signal(libc::SIGTERM)?;
    }
    let term_deadline = Instant::now() + grace;
    let kill_deadline = term_deadline + Duration::from_secs(5);
    let mut killed = false;
    loop {
        let mut live = false;
        for handle in handles {
            live |= !handle.exited()?;
        }
        if !live {
            return Ok(());
        }
        let now = Instant::now();
        if now >= term_deadline && !killed {
            for handle in handles {
                handle.signal(libc::SIGKILL)?;
            }
            killed = true;
        }
        anyhow::ensure!(now < kill_deadline, "owned process termination unproven");
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub(crate) fn fence_processes(
    identities: &[PrimeProcessIdentity],
    grace: Duration,
) -> anyhow::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut handles = Vec::with_capacity(identities.len());
        for identity in identities {
            if let Some(handle) = platform::ProcessHandle::capture(identity)? {
                handles.push(handle);
            }
        }
        settle_process_handles(&handles, grace)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (identities, grace);
        anyhow::bail!("incarnation-bound signalling unsupported on this platform")
    }
}

pub(crate) fn runner_group_identities(group: u32) -> anyhow::Result<Vec<PrimeProcessIdentity>> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        platform::runner_group_identities(group)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = group;
        anyhow::bail!("runner group inspection unsupported on this platform")
    }
}
pub fn boot_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        linux::boot_id()
    }
    #[cfg(target_os = "macos")]
    {
        macos::boot_id()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Whether a locally launched container can run on this host's own kernel, so
/// the boot_id/inode mount proof in the sandbox content migration can be
/// established. Only a Linux host runs containers on its own kernel; macOS and
/// every other platform run them inside a VM whose kernel identity never
/// matches the host's, so the proof is skipped and declared mounts are trusted.
pub fn host_shares_container_kernel() -> bool {
    cfg!(target_os = "linux")
}

pub(crate) struct ProcessArguments {
    pub argv: Vec<String>,
    pub stdin_is_tty: Option<bool>,
}

pub(crate) fn foreground_arguments(shell_pid: u32) -> Vec<ProcessArguments> {
    #[cfg(target_os = "linux")]
    {
        linux::foreground_arguments(shell_pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos::foreground_arguments(shell_pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = shell_pid;
        Vec::new()
    }
}

/// The parent pid and `argv[0]` of `pid`, or `None` when it cannot be read.
pub fn parent_and_argv0(pid: u32) -> Option<(u32, String)> {
    #[cfg(target_os = "linux")]
    {
        linux::parent_and_argv0(pid)
    }

    #[cfg(target_os = "macos")]
    {
        macos::parent_and_argv0(pid)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Get the foreground process group leader PID for a given shell PID
/// This finds the actual process that has the terminal foreground
pub fn get_foreground_pid(shell_pid: u32) -> Option<u32> {
    let pid = {
        #[cfg(target_os = "linux")]
        {
            linux::get_foreground_pid(shell_pid)
        }

        #[cfg(target_os = "macos")]
        {
            macos::get_foreground_pid(shell_pid)
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = shell_pid;
            None
        }
    };
    if tracing::enabled!(target: "process.ppid", tracing::Level::TRACE) {
        tracing::trace!(
            target: "process.ppid",
            shell_pid,
            foreground_pid = ?pid,
            "resolved foreground pid",
        );
    }
    pid
}

/// An acquired OS assertion that prevents user-idle system sleep while agent
/// sessions are active. The assertion is held by a subprocess whose lifetime
/// is tied to the daemon, so it releases even when the daemon exits via
/// `std::process::exit` or dies from a panic, OOM, or `kill -9` (paths where
/// no `Drop` runs): the child terminates itself once the daemon is gone (macOS
/// `caffeinate -w <daemon_pid>` watches the daemon PID; the Linux `cat` sees
/// EOF on the stdin pipe the daemon held), and init then reaps it. The
/// `server` status loop is the only consumer.
pub trait SleepInhibit: Send {
    /// Acquire the assertion by spawning and retaining the backing child.
    fn acquire(&mut self) -> anyhow::Result<()>;
    /// Release the assertion, terminating the backing child.
    fn release(&mut self);
    /// Whether the reconciler should treat the assertion as still held and skip
    /// respawning. True while the backing child is alive, and when respawning
    /// would be futile (backend latched unavailable, or an unsupported platform).
    fn is_held_alive(&mut self) -> bool;
}

/// Build the platform sleep inhibitor: `caffeinate -i -w <daemon_pid>` on
/// macOS, `systemd-inhibit --what=idle:sleep ... cat` on Linux, and a no-op
/// on every other platform.
pub fn sleep_inhibitor() -> Box<dyn SleepInhibit> {
    #[cfg(target_os = "macos")]
    {
        Box::new(macos::CaffeinateInhibitor::new())
    }

    #[cfg(target_os = "linux")]
    {
        Box::new(linux::SystemdInhibitor::new())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Box::new(NoopInhibitor)
    }
}

/// Sleep inhibitor for platforms with no supported backend: acquire and release
/// are no-ops, and `is_held_alive` reports held to avoid pointless respawns.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct NoopInhibitor;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl SleepInhibit for NoopInhibitor {
    fn acquire(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn release(&mut self) {}

    fn is_held_alive(&mut self) -> bool {
        // No backend exists on this platform, so there is nothing to respawn:
        // report held to keep the reconciler at a steady-state no-op instead of
        // rebuilding a fresh Noop every reconcile interval.
        true
    }
}

/// Latched once a sleep-inhibit backend is found unusable on this host: the
/// helper binary is missing, or it spawns but exits without taking the lock
/// (WSL2 or a container with no logind). Process-global because the reconciler
/// builds a fresh inhibitor on every reacquire, so a per-instance guard could
/// not remember across rebuilds; the backends read it to report the dead
/// backend as held, which stops the reconciler respawning a doomed child.
#[cfg(any(target_os = "linux", target_os = "macos"))]
static SLEEP_INHIBIT_UNAVAILABLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sleep_inhibit_unavailable() -> bool {
    SLEEP_INHIBIT_UNAVAILABLE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether a real OS sleep-inhibit backend is still believed able to hold the
/// assertion on this host, for read-only status reporting.
/// Optimistic: `true` only means no failure has latched yet, not that the
/// backend was verified; it is never actively probed, so a host where it would
/// fail still reads `true` until a real attempt latches it. `false` once the
/// backend latches unavailable (helper binary missing, or a WSL2/container with
/// no logind), and false on platforms with only `NoopInhibitor`. The latch is
/// monotonic and never resets for the daemon's lifetime, so this stays false
/// until restart even if the missing tool is later installed.
pub(crate) fn sleep_inhibit_backend_available() -> bool {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        !sleep_inhibit_unavailable()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

/// Latch the sleep-inhibit backend unavailable and warn exactly once. Shared
/// by the platform backends so the warn-once policy lives in one place while
/// each backend keeps only its own spawn and liveness mechanics.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn latch_sleep_inhibit_unavailable(reason: &str) {
    if !SLEEP_INHIBIT_UNAVAILABLE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(target: "process.sleep_inhibit", "{reason}");
    }
}

/// Report whether a spawned inhibitor's backing child should be treated as
/// still holding the assertion. Shared by both platform backends because the
/// exit-code discrimination is policy (like the warn-once latch), not per-OS
/// mechanics, so the two copies cannot drift; only `exit_reason` differs.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sleep_inhibit_child_held_alive(child: &mut Option<Child>, exit_reason: &str) -> bool {
    if sleep_inhibit_unavailable() {
        return true;
    }
    let Some(child) = child.as_mut() else {
        return false;
    };
    match child.try_wait() {
        Ok(None) => true,
        // A nonzero exit means the helper failed to hold the lock; death by signal is an
        // external kill, so respawn.
        Ok(Some(status)) if status.code().is_some_and(|c| c != 0) => {
            latch_sleep_inhibit_unavailable(exit_reason);
            true
        }
        _ => false,
    }
}

/// Kill a process and all its descendants
/// Sends SIGTERM first, then SIGKILL to any survivors
pub fn kill_process_tree(pid: u32) {
    #[cfg(target_os = "linux")]
    let pids = linux::collect_pid_tree(pid);

    #[cfg(target_os = "macos")]
    let pids = macos::collect_pid_tree(pid);

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    kill_with_fallback(&pids);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        // No-op on unsupported platforms, fall back to tmux kill-session only
    }
}

/// SIGTERM every pid in reverse order (children first), wait briefly for
/// graceful shutdown, then SIGKILL anything still alive.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn kill_with_fallback(pids: &[u32]) {
    tracing::debug!(
        target: "process.tree",
        descendants = ?pids,
        "killing process tree"
    );

    for &p in pids.iter().rev() {
        tracing::debug!(target: "process.signal", pid = p, signal = "SIGTERM", "sending signal");
        let _ = kill(Pid::from_raw(p as i32), Signal::SIGTERM);
    }

    std::thread::sleep(Duration::from_millis(100));

    for &p in pids.iter().rev() {
        if process_exists(p) {
            tracing::warn!(
                target: "process.reap",
                pid = p,
                "pid survived SIGTERM after 100ms; sending SIGKILL"
            );
            tracing::info!(target: "process.signal", pid = p, signal = "SIGKILL", "sending signal");
            let _ = kill(Pid::from_raw(p as i32), Signal::SIGKILL);
        }
    }
}

pub(crate) fn runner_group_executing(pid: u32) -> anyhow::Result<bool> {
    #[cfg(target_os = "linux")]
    {
        linux::runner_group_executing(pid)
    }
    #[cfg(target_os = "macos")]
    {
        macos::runner_group_executing(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        anyhow::bail!("runner settlement unsupported for PID {pid}")
    }
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn process_exists(pid: u32) -> bool {
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// Send SIGSTOP to a process and all its descendants. Used to pause
/// the agent (claude) while a mobile client is reading tmux scrollback:
/// without this, claude's continued output keeps pushing lines into
/// scrollback under the reader and shifts what they're trying to read.
///
/// Paired with [`continue_process_tree`] which sends SIGCONT. The web
/// server guarantees a SIGCONT on client disconnect so a dropped
/// connection cannot leave the pane's process permanently suspended.
pub fn stop_process_tree(pid: u32) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    signal_process_tree(pid, Signal::SIGSTOP);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
    }
}

/// Send SIGCONT to a process and all its descendants. Inverse of
/// [`stop_process_tree`]; SIGCONT to a non-stopped process is a no-op,
/// so this is safe to invoke unconditionally as cleanup.
pub fn continue_process_tree(pid: u32) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    signal_process_tree(pid, Signal::SIGCONT);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn signal_process_tree(pid: u32, signal: Signal) {
    #[cfg(target_os = "linux")]
    let pids = linux::collect_pid_tree(pid);
    #[cfg(target_os = "macos")]
    let pids = macos::collect_pid_tree(pid);

    tracing::debug!(
        target: "process.tree",
        descendants = ?pids,
        ?signal,
        "signaling process tree"
    );
    for &p in pids.iter().rev() {
        if let Err(e) = kill(Pid::from_raw(p as i32), signal) {
            if e != Errno::ESRCH {
                tracing::debug!(
                    target: "process.signal",
                    pid = p,
                    ?signal,
                    error = %e,
                    "kill failed"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prime_process_selection_preserves_scope_across_pid_reuse() {
        let sample = |pid, parent, start: &str, marked| PrimeProcessSample {
            identity: PrimeProcessIdentity {
                pid,
                start_id: start.into(),
            },
            parent,
            marked,
        };
        let known = PrimeProcessIdentity {
            pid: 10,
            start_id: "proc:100".into(),
        };
        for (start, expected) in [("proc:100", vec![10, 11, 12]), ("proc:200", vec![])] {
            let selected = select_prime_processes(
                vec![
                    sample(10, 1, start, false),
                    sample(11, 10, "proc:101", false),
                    sample(12, 11, "proc:102", false),
                    sample(20, 1, "proc:103", false),
                ],
                std::slice::from_ref(&known),
            );
            assert_eq!(
                selected
                    .into_iter()
                    .map(|identity| identity.pid)
                    .collect::<Vec<_>>(),
                expected
            );
        }
        let selected = select_prime_processes(
            vec![
                sample(10, 1, "proc:200", false),
                sample(30, 1, "proc:300", true),
                sample(31, 30, "proc:301", false),
            ],
            &[known],
        );
        assert_eq!(
            selected
                .into_iter()
                .map(|identity| identity.pid)
                .collect::<Vec<_>>(),
            [30, 31]
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn captured_tree_changed_boot_is_obsolete_but_malformed_identity_is_not() {
        let current = CapturedProcessTree::capture(std::process::id()).unwrap();
        let mut receipt = current.receipt().clone();
        receipt.boot_id = format!("different-{}", receipt.boot_id);
        let obsolete = CapturedProcessTree::resume(receipt.clone(), std::process::id()).unwrap();
        assert!(obsolete.all_exited().unwrap());
        obsolete.settle(Duration::ZERO).unwrap();
        receipt.processes.push(receipt.processes[0].clone());
        assert!(CapturedProcessTree::resume(receipt, std::process::id()).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn throttle_child_lowers_the_child_scheduling_priority() {
        let priority = |throttled: bool| -> i32 {
            let mut cmd = std::process::Command::new("sh");
            cmd.args(["-c", "ps -o pri= -p $$"]);
            if throttled {
                throttle_child(&mut cmd);
            }
            let out = cmd.output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
        };
        assert!(priority(true) < priority(false));
    }

    #[test]
    fn processes_matching_empty_input_is_empty() {
        assert!(processes_matching(&[], &[], &[]).is_empty());
    }

    #[test]
    fn boot_id_is_stable_within_a_boot() {
        assert_eq!(boot_id(), boot_id());
        if let Some(id) = boot_id() {
            assert!(!id.is_empty());
        }
    }

    /// A live process carrying a unique marker in its argv is matched by the
    /// cmdline needle; a marker no process carries is not. Backs the #2994
    /// orphan guard's fallback signal for non-hook agents.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn processes_matching_detects_live_process_by_cmdline() {
        let marker = format!("aoe_orphan_scan_marker_{}", std::process::id());
        let absent = format!("aoe_absent_scan_marker_{}", std::process::id());

        // The marker rides as `$0` of a compound list so sh does not exec it away.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30; true")
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn marker process");

        // env slot empty, cmdline slot carries the needle. Two candidates: the
        // live marker and an absent one, testing both outcomes in one pass.
        let env = vec![String::new(), String::new()];
        let cmd = vec![Some(marker.clone()), Some(absent.clone())];

        // Poll until the child's argv is observable (spawn returns before the
        // kernel necessarily populates cmdline).
        let mut flags = vec![false, false];
        for _ in 0..100 {
            flags = processes_matching(&env, &cmd, &[None, None]);
            if flags[0] {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let _ = child.kill();
        let _ = child.wait();

        assert!(
            flags[0],
            "a live process carrying the marker in argv must match"
        );
        assert!(!flags[1], "a marker no live process carries must not match");
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn parent_and_argv0_reads_a_live_child() {
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // Until exec completes, argv is empty or still the parent's.
        let deadline = Instant::now() + Duration::from_secs(5);
        let read = loop {
            let read = parent_and_argv0(child.id());
            if read.as_ref().is_some_and(|(_, argv0)| argv0 == "sleep")
                || Instant::now() >= deadline
            {
                break read;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(read, Some((std::process::id(), "sleep".to_string())));
    }

    /// The environment signal: a live process with `AOE_INSTANCE_ID=<marker>`
    /// in its environment is matched by the anchored env needle even when the
    /// marker is absent from argv. This is the argv-rewrite-proof identity
    /// signal the #2994 guard relies on for hook-enabled agents. Linux-only:
    /// `/proc/<pid>/environ` is a stable probe; macOS `ps -E` env visibility is
    /// hardening-dependent and not asserted here.
    #[cfg(target_os = "linux")]
    #[test]
    fn processes_matching_detects_live_process_by_env_entry() {
        let marker = format!("aoe_env_marker_{}", std::process::id());

        // `sleep` keeps the process alive; the marker is only in the env, never
        // in argv, so a match must come from the environ scan.
        let key = crate::tmux::env::AOE_INSTANCE_ID_KEY;
        let mut child = Command::new("sleep")
            .arg("30")
            .env(key, &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn env-marker process");

        let env = vec![format!("{key}={marker}")];
        // A prefix of the real value must NOT match (anchored entry, not prefix).
        let env_prefix = vec![format!("{key}={}", &marker[..marker.len() - 1])];
        let cmd = vec![None];

        let mut found = false;
        let mut prefix_hit = true;
        for _ in 0..100 {
            found = processes_matching(&env, &cmd, &[None])[0];
            prefix_hit = processes_matching(&env_prefix, &cmd, &[None])[0];
            if found {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let _ = child.kill();
        let _ = child.wait();

        assert!(found, "AOE_INSTANCE_ID in the environment must be matched");
        assert!(
            !prefix_hit,
            "a prefix of the env value must not match (anchored entry, not substring)"
        );
    }

    /// Restores the pre-test SIGINT/SIGQUIT disposition on drop. `#[serial]`
    /// only keeps these tests from racing each other; it does nothing to
    /// stop a mid-test `expect`/`assert!` panic from leaving process-wide
    /// signal state mutated for the rest of the test binary's run, since
    /// the harness catches per-test panics and keeps going. This guard
    /// makes that restoration unconditional.
    #[cfg(unix)]
    struct RestoreSignalsOnDrop {
        prev_sigint: Option<nix::sys::signal::SigAction>,
        prev_sigquit: Option<nix::sys::signal::SigAction>,
    }

    #[cfg(unix)]
    impl Drop for RestoreSignalsOnDrop {
        fn drop(&mut self) {
            use nix::sys::signal::{sigaction, Signal};

            if let Some(prev) = &self.prev_sigint {
                // SAFETY: sigaction is async-signal-safe and safe to call
                // from a normal (non-signal-handler) context, which is all
                // a `Drop` impl running on the test thread is.
                let _ = unsafe { sigaction(Signal::SIGINT, prev) };
            }
            if let Some(prev) = &self.prev_sigquit {
                // SAFETY: see above.
                let _ = unsafe { sigaction(Signal::SIGQUIT, prev) };
            }
        }
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn reset_ignored_signals_before_exec_clears_sig_ign_on_sigint_and_sigquit() {
        use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

        let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
        // SAFETY: SIG_IGN is async-signal-safe per POSIX, the only
        // requirement for sigaction calls made outside a signal handler.
        let prev_sigint = unsafe { sigaction(Signal::SIGINT, &ignore) }.expect("ignore SIGINT");
        // SAFETY: see above.
        let prev_sigquit = unsafe { sigaction(Signal::SIGQUIT, &ignore) }.expect("ignore SIGQUIT");
        let _restore = RestoreSignalsOnDrop {
            prev_sigint: Some(prev_sigint),
            prev_sigquit: Some(prev_sigquit),
        };

        reset_ignored_signals_before_exec().expect("reset must succeed");

        let probe = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
        // SAFETY: querying via sigaction (which both sets and returns the
        // previous disposition) is async-signal-safe; SIG_DFL is a no-op
        // here since the function under test already set it.
        let sigint_after = unsafe { sigaction(Signal::SIGINT, &probe) }
            .expect("query SIGINT")
            .handler();
        // SAFETY: see above.
        let sigquit_after = unsafe { sigaction(Signal::SIGQUIT, &probe) }
            .expect("query SIGQUIT")
            .handler();

        assert!(
            matches!(sigint_after, SigHandler::SigDfl),
            "SIGINT should be reset to SIG_DFL, not left as SIG_IGN"
        );
        assert!(
            matches!(sigquit_after, SigHandler::SigDfl),
            "SIGQUIT should be reset to SIG_DFL, not left as SIG_IGN"
        );
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn reset_signals_on_exec_stops_child_from_inheriting_sig_ign() {
        use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, Signal};

        let ignore = SigAction::new(
            SigHandler::SigIgn,
            SaFlags::empty(),
            nix::sys::signal::SigSet::empty(),
        );
        // SAFETY: see the test above; SIG_IGN is async-signal-safe.
        let prev_sigint = unsafe { sigaction(Signal::SIGINT, &ignore) }.expect("ignore SIGINT");
        // This test only mutates SIGINT, so there is nothing to restore
        // for SIGQUIT.
        let _restore = RestoreSignalsOnDrop {
            prev_sigint: Some(prev_sigint),
            prev_sigquit: None,
        };

        // A shell that signals itself and only then prints: if the child
        // inherits SIGINT ignored from the parent, the self-signal is a
        // no-op and the shell keeps running to print "survived". If
        // `reset_signals_on_exec` did its job, the default SIGINT action
        // (terminate) kills the shell before the echo runs.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "kill -INT $$; echo survived"]);
        reset_signals_on_exec(&mut cmd);
        let output = cmd.output().expect("spawn sh");

        assert!(
            !output.status.success(),
            "child should have been killed by its own SIGINT instead of exiting cleanly"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains("survived"),
            "child should die on SIGINT before reaching the echo, not inherit the parent's ignore"
        );
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn restore_signals_on_drop_runs_even_when_the_scope_panics() {
        use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

        let ignore = SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty());
        // SAFETY: SIG_IGN is async-signal-safe per POSIX, the only
        // requirement for sigaction calls made outside a signal handler.
        let baseline = unsafe { sigaction(Signal::SIGINT, &ignore) }.expect("ignore SIGINT");

        // The guard is told to restore SIG_IGN (as if that's what the test
        // found in place). Inside the panicking scope we then flip the
        // disposition to SIG_DFL via `reset_ignored_signals_before_exec`,
        // so a restore that only ran on normal return would leave SIG_DFL
        // behind; only an unconditional (Drop-based) restore gets back to
        // SIG_IGN across the unwind.
        let unwound = std::panic::catch_unwind(|| {
            let _restore = RestoreSignalsOnDrop {
                prev_sigint: Some(ignore),
                prev_sigquit: None,
            };
            reset_ignored_signals_before_exec().expect("reset must succeed");
            panic!("simulate a mid-test assertion failure");
        });
        assert!(unwound.is_err(), "the closure should have panicked");

        let probe = SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty());
        // SAFETY: see the test above; querying via sigaction is
        // async-signal-safe.
        let sigint_after = unsafe { sigaction(Signal::SIGINT, &probe) }
            .expect("query SIGINT")
            .handler();
        assert!(
            matches!(sigint_after, SigHandler::SigIgn),
            "RestoreSignalsOnDrop should have restored SIG_IGN across the panic unwind, \
             not left the SIG_DFL that reset_ignored_signals_before_exec set"
        );

        // SAFETY: restoring the pre-test disposition for tests after this one.
        unsafe { sigaction(Signal::SIGINT, &baseline) }.expect("restore SIGINT");
    }

    #[test]
    fn collect_descendants_from_map_walks_only_the_root_subtree() {
        let cases: [(&[(u32, &[u32])], &[u32]); 6] = [
            (&[], &[100]),
            (&[(100, &[101])], &[100, 101]),
            (&[(100, &[101, 102, 103])], &[100, 101, 102, 103]),
            (
                &[(100, &[101]), (101, &[102]), (102, &[103])],
                &[100, 101, 102, 103],
            ),
            (
                &[(100, &[101, 102]), (101, &[103, 104]), (102, &[105])],
                &[100, 101, 102, 103, 104, 105],
            ),
            (&[(200, &[201, 202]), (300, &[301])], &[100]),
        ];
        for (edges, expected) in cases {
            let children_map: HashMap<u32, Vec<u32>> = edges
                .iter()
                .map(|(parent, kids)| (*parent, kids.to_vec()))
                .collect();
            let mut pids = vec![100];
            collect_descendants_from_map(100, &children_map, &mut pids);
            pids.sort_unstable();
            assert_eq!(pids, expected, "{edges:?}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn wait_with_timeout_returns_status_for_fast_child() {
        let mut child = Command::new("sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let status = wait_with_timeout(&mut child, Duration::from_secs(10))
            .unwrap()
            .expect("fast child exits before the timeout");
        assert!(status.success());
    }

    #[cfg(unix)]
    fn assert_child_reaped(pid: u32) {
        let mut status = 0;
        let result = unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) };
        let error = std::io::Error::last_os_error().raw_os_error();
        if result == 0 {
            unsafe {
                libc::kill(pid as i32, libc::SIGKILL);
                libc::waitpid(pid as i32, &mut status, 0);
            }
        }
        assert_eq!(
            result, -1,
            "timeout must reap its direct child before returning"
        );
        assert_eq!(error, Some(libc::ECHILD));
    }

    #[test]
    #[cfg(unix)]
    fn wait_with_timeout_kills_child_that_outlives_deadline() {
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();

        let start = Instant::now();
        let status = wait_with_timeout(&mut child, Duration::from_millis(200)).unwrap();
        assert_child_reaped(child.id());
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            child.try_wait().unwrap().unwrap().signal(),
            Some(libc::SIGKILL)
        );
        assert!(
            status.is_none(),
            "expected the timeout to fire and kill the child"
        );
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "wait should return promptly after the deadline, not block on the child"
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_with_timeout_captures_output_for_fast_child() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf out; printf err >&2"]);

        let output = run_with_timeout(&mut cmd, Duration::from_secs(10))
            .unwrap()
            .expect("fast child should complete before the timeout");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"out");
        assert_eq!(output.stderr, b"err");
    }

    #[test]
    #[cfg(unix)]
    fn run_with_timeout_kills_child_on_deadline_or_cancel() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::{net::UnixStream, process::CommandExt};

        for cancelled in [false, true] {
            let (mut pid_reader, pid_writer) = UnixStream::pair().unwrap();
            pid_reader
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut cmd = Command::new("sleep");
            cmd.arg("5");
            unsafe {
                cmd.pre_exec(move || {
                    let pid = libc::getpid().to_ne_bytes();
                    let written =
                        libc::write(pid_writer.as_raw_fd(), pid.as_ptr().cast(), pid.len());
                    if written != pid.len() as isize {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }

            let start = Instant::now();
            let result = if cancelled {
                let cancel = CancellationToken::new();
                cancel.cancel();
                run_until_cancelled(&mut cmd, Duration::from_secs(60), &cancel).unwrap()
            } else {
                run_with_timeout(&mut cmd, Duration::from_millis(300)).unwrap()
            };
            let mut pid = [0; std::mem::size_of::<libc::pid_t>()];
            pid_reader
                .read_exact(&mut pid)
                .expect("child must publish its PID before exec");
            assert_child_reaped(libc::pid_t::from_ne_bytes(pid) as u32);
            assert!(
                result.is_none(),
                "cancelled={cancelled}: child must be killed"
            );
            assert!(
                start.elapsed() < Duration::from_secs(4),
                "cancelled={cancelled}: wait should return promptly, not block on the child"
            );
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn timeout_and_cancellation_terminate_process_group_descendants() {
        for cancel in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let pid_path = tmp.path().join("descendant.pid");
            let term_path = tmp.path().join("terminated");
            let mut cmd = Command::new("sh");
            cmd.args([
                "-c",
                r#"trap 'printf term > "$2"' TERM; sh -c 'trap "" TERM; printf "%s" "$$" > "$1"; exec sleep 600' child "$1" & wait; wait"#,
                "hook",
            ])
            .arg(&pid_path)
            .arg(&term_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

            let token = CancellationToken::new();
            // Cancel once the descendant's pid is on disk, so the wait has
            // something to tear down. The wait runs on its own thread because
            // the child is only spawned once the call below starts, and it is
            // bounded so a shell that never reports its pid fails the test
            // instead of hanging it.
            let canceler = cancel.then(|| {
                let token = token.clone();
                let pid_path = pid_path.clone();
                std::thread::spawn(move || {
                    let deadline = Instant::now() + Duration::from_secs(10);
                    while !pid_path.exists() {
                        if Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    token.cancel();
                })
            });
            let result = if cancel {
                run_status_with_timeout_process_group(
                    &mut cmd,
                    Duration::from_secs(30),
                    Some(&token),
                )
            } else {
                run_with_timeout_process_group(&mut cmd, Duration::from_secs(2))
                    .map(|output| output.map(|output| output.status))
            }
            .unwrap();
            // The canceler has fired by the time the call returns; join it so
            // the test does not outlive its own helper thread.
            if let Some(canceler) = canceler {
                canceler.join().unwrap();
            }
            assert!(result.is_none(), "process group must be interrupted");
            assert_eq!(
                std::fs::read_to_string(term_path).expect("SIGTERM trap should run"),
                "term",
                "the process-group leader must receive SIGTERM before escalation"
            );

            let pid: i32 = std::fs::read_to_string(pid_path)
                .expect("shell should publish descendant pid")
                .parse()
                .unwrap();
            let process_is_running = |pid: i32| {
                let output = Command::new("ps")
                    .args(["-o", "stat=", "-p", &pid.to_string()])
                    .output()
                    .expect("ps should inspect descendant state");
                let state = String::from_utf8_lossy(&output.stdout);
                output.status.success()
                    && !state.trim().is_empty()
                    && !state.trim_start().starts_with('Z')
            };
            let deadline = Instant::now() + Duration::from_secs(1);
            while process_is_running(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                !process_is_running(pid),
                "descendant must be exited or a terminated zombie after interruption"
            );
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn completed_status_hook_does_not_abandon_background_children() {
        let root = tempfile::tempdir().unwrap();
        let pid_path = root.path().join("child.pid");
        let mut command = Command::new("sh");
        command
            .args(["-c", r#"sleep 30 & printf %s $! > "$1""#, "sh"])
            .arg(&pid_path);
        let result =
            run_status_with_timeout_process_group(&mut command, Duration::from_secs(2), None)
                .unwrap();
        assert!(result.unwrap().success());
        let pid: i32 = std::fs::read_to_string(pid_path).unwrap().parse().unwrap();
        // Signalling the group precedes the descendants' exit transition.
        let deadline = Instant::now() + Duration::from_secs(1);
        let running = loop {
            let output = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&output.stdout);
            let running = output.status.success()
                && !state.trim().is_empty()
                && !state.trim_start().starts_with('Z');
            if !running || Instant::now() >= deadline {
                break running;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if running {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        assert!(!running, "completed hook left its background child running");
    }

    #[test]
    #[cfg(unix)]
    fn run_with_timeout_does_not_wait_for_descendant_output_writer() {
        // The immediate child (sh) exits fast but backgrounds a `sleep` that
        // retains stdout/stderr. Output capture must read the current regular
        // file contents without waiting for that descendant to close its
        // inherited handles. `sleep 10` (>> the 4s assertion) ensures an
        // inherited pipe plus an unbounded drain would visibly fail.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 10 & printf done"]);

        let start = Instant::now();
        let output = run_with_timeout(&mut cmd, Duration::from_millis(500))
            .unwrap()
            .expect("the sh child exits quickly, so an Output is produced");
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "output capture must not wait on a descendant that still holds the handle"
        );
        assert!(output.status.success());
    }
}
