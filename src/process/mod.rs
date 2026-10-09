//! OS process utilities: timeouts, process trees, pane pids, and sleep inhibition.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::errno::Errno;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::sys::signal::{kill, Signal};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use nix::unistd::Pid;

/// Test-only original-root death observation; never group retirement authority.
#[cfg(all(test, debug_assertions, any(target_os = "linux", target_os = "macos")))]
pub(crate) struct OriginalRootDeathObservation(platform::OriginalRootDeathObservation);

#[cfg(all(test, debug_assertions, any(target_os = "linux", target_os = "macos")))]
pub(crate) fn ignore_child_reaping_for_hosted_probe() -> std::io::Result<()> {
    platform::ignore_child_reaping_for_hosted_probe()
}

#[cfg(all(test, debug_assertions, any(target_os = "linux", target_os = "macos")))]
impl OriginalRootDeathObservation {
    pub(crate) fn bind(birth: ProcessIncarnation) -> anyhow::Result<Self> {
        anyhow::ensure!(
            process_incarnation(birth.pid)? == Some(birth),
            "original root changed before death observation binding"
        );
        let observation = platform::OriginalRootDeathObservation::bind(birth)?;
        anyhow::ensure!(
            process_incarnation(birth.pid)? == Some(birth),
            "original root changed during death observation binding"
        );
        Ok(Self(observation))
    }

    pub(crate) fn exited(&self) -> anyhow::Result<bool> {
        self.0.exited()
    }
}

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::receive_natal_authorization_until;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn receive_natal_authorization_until(
    _: &std::os::unix::net::UnixStream,
    _: std::time::Instant,
    _: impl FnMut() -> bool,
) -> std::io::Result<Option<u8>> {
    Err(std::io::ErrorKind::Unsupported.into())
}

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    pub(super) struct OwnedCreateRoot;
    impl OwnedCreateRoot {
        pub(super) fn prepare(_: super::ProcessIncarnation) -> anyhow::Result<Self> {
            anyhow::bail!("owned native Create is unsupported on this OS")
        }
        pub(super) fn release(&mut self) -> anyhow::Result<()> {
            anyhow::bail!("owned native Create is unsupported on this OS")
        }
        pub(super) fn retire(
            &mut self,
            _: &mut std::process::Child,
            _: &tokio_util::sync::CancellationToken,
            _: impl FnMut(super::CreateObservation) -> anyhow::Result<()>,
        ) -> anyhow::Result<super::CreateRetirement> {
            anyhow::bail!("owned native Create is unsupported on this OS")
        }
    }
    pub(super) fn hold_owned_create_bootstrap() -> anyhow::Result<()> {
        anyhow::bail!("owned native Create is unsupported on this OS")
    }
    pub(super) fn exec_owned_create(
        _: &std::ffi::CString,
        _: &std::fs::File,
        _: &std::fs::File,
        _: &[std::ffi::CString],
        _: &[std::ffi::CString],
    ) -> anyhow::Result<()> {
        anyhow::bail!("owned native Create is unsupported on this OS")
    }
    pub(super) fn owned_create_descendant_traceable() -> bool {
        false
    }
    pub(super) fn owned_create_anchor_path(
        _: &std::fs::File,
        _: u32,
    ) -> anyhow::Result<(std::ffi::OsString, bool)> {
        anyhow::bail!("owned native Create is unsupported on this OS")
    }
    pub(super) fn install_owned_create_anchors(
        _: &[std::fs::File],
        _: &[u32],
    ) -> anyhow::Result<()> {
        anyhow::bail!("owned native Create is unsupported on this OS")
    }
    pub(super) fn configure_process_group(_: &mut std::process::Command) {}

    pub(super) fn kill_process_group(_: &std::process::Child) {}

    pub(super) fn terminate_process_group(_: &std::process::Child) {}

    pub(super) fn is_terminated(_pid: u32) -> bool {
        false
    }

    pub(super) fn is_process_group_alive(pgid: u32) -> bool {
        pgid != 0 && super::worker::is_pid_alive_and_ours(pgid)
    }

    pub(super) fn peer_pid_from_connected_socket(_: &impl std::os::fd::AsFd) -> Option<u32> {
        None
    }

    pub(super) fn process_group_has_live_members(_pgrp: u32) -> std::io::Result<bool> {
        Ok(false)
    }
}

pub(crate) enum CreateObservation {
    Birth(ProcessIncarnation),
    ExternalDomain,
    ScopeUnproven,
}
pub(crate) struct CreateRetirement {
    pub(crate) status: ExitStatus,
    pub(crate) raw_status: i32,
    pub(crate) descendants_retired: bool,
    pub(crate) group_retired: bool,
    pub(crate) external_domain: bool,
}
pub(crate) struct OwnedCreateRoot(platform::OwnedCreateRoot);
impl OwnedCreateRoot {
    pub(crate) fn prepare(birth: ProcessIncarnation) -> anyhow::Result<Self> {
        platform::OwnedCreateRoot::prepare(birth).map(Self)
    }
    pub(crate) fn release(&mut self) -> anyhow::Result<()> {
        self.0.release()
    }
    pub(crate) fn retire(
        &mut self,
        child: &mut Child,
        cancel: &CancellationToken,
        admit: impl FnMut(CreateObservation) -> anyhow::Result<()>,
    ) -> anyhow::Result<CreateRetirement> {
        self.0.retire(child, cancel, admit)
    }
}
pub(crate) fn hold_owned_create_bootstrap() -> anyhow::Result<()> {
    platform::hold_owned_create_bootstrap()
}
pub(crate) fn exec_owned_create(
    program: &std::ffi::CString,
    executable: &std::fs::File,
    directory: &std::fs::File,
    argv: &[std::ffi::CString],
    env: &[std::ffi::CString],
) -> anyhow::Result<()> {
    platform::exec_owned_create(program, executable, directory, argv, env)
}

pub(crate) fn owned_create_descendant_traceable() -> bool {
    platform::owned_create_descendant_traceable()
}
pub(crate) fn owned_create_anchor_path(
    file: &std::fs::File,
    slot: u32,
) -> anyhow::Result<(std::ffi::OsString, bool)> {
    platform::owned_create_anchor_path(file, slot)
}
pub(crate) fn install_owned_create_anchors(
    files: &[std::fs::File],
    slots: &[u32],
) -> anyhow::Result<()> {
    platform::install_owned_create_anchors(files, slots)
}

/// Lower the child's scheduling and I/O priority where the OS supports it.
pub(crate) fn throttle_child(cmd: &mut std::process::Command) {
    #[cfg(target_os = "macos")]
    macos::throttle_child(cmd);
    #[cfg(not(target_os = "macos"))]
    let _ = cmd;
}
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use platform::HAS_CODEX_MANAGED_PREFERENCES;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::receive_bootstrap_descriptors;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn receive_bootstrap_descriptors<const N: usize>(
    _: &std::os::unix::net::UnixStream,
) -> anyhow::Result<[std::os::fd::OwnedFd; N]> {
    anyhow::bail!("native bootstrap descriptor reception is unsupported on this platform")
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) const HAS_CODEX_MANAGED_PREFERENCES: bool = true;

pub(crate) mod metrics;

pub mod worker;

pub mod worker_registry;

pub mod runner;

const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PROCESS_GROUP_TERMINATION_GRACE: Duration = Duration::from_millis(250);
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

/// Only use this when output is not piped: a full pipe can wedge the child. Prefer
/// [`run_with_timeout`].
pub fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
) -> std::io::Result<Option<ExitStatus>> {
    wait_with_timeout_inner(child, timeout, false, None)
}

/// With `kill_process_group`, `child` must be a process-group leader, or the kill
/// could signal the parent's group.
fn wait_with_timeout_inner(
    child: &mut Child,
    timeout: Duration,
    kill_process_group: bool,
    cancel: Option<&CancellationToken>,
) -> std::io::Result<Option<ExitStatus>> {
    let deadline = Instant::now() + timeout;
    let termination_grace = (timeout / 4).min(PROCESS_GROUP_TERMINATION_GRACE);
    let terminate_at = deadline.checked_sub(termination_grace).unwrap_or(deadline);
    let mut termination_requested = false;
    // Most children exit within a few ms; start fine and back off to the cap.
    let mut poll = Duration::from_millis(1);
    loop {
        if let Some(status) = child.try_wait()? {
            if !termination_requested {
                return Ok(Some(status));
            }
        }
        let now = Instant::now();
        if kill_process_group && !termination_requested && now >= terminate_at {
            platform::terminate_process_group(child);
            termination_requested = true;
            continue;
        }
        if now >= deadline || cancel.is_some_and(CancellationToken::is_cancelled) {
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

/// Output goes to temp files, so a full pipe or a descendant holding the handles cannot
/// wedge capture. Returns `Ok(None)` on timeout.
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

/// Put a command in a fresh process group, so a signal to the group reaches it
/// and only it.
pub(crate) fn configure_process_group(cmd: &mut Command) {
    platform::configure_process_group(cmd);
}

pub fn run_with_timeout_process_group(
    cmd: &mut Command,
    timeout: Duration,
) -> std::io::Result<Option<Output>> {
    configure_process_group(cmd);
    run_with_timeout_inner(
        cmd,
        timeout,
        cfg!(any(target_os = "linux", target_os = "macos")),
        None,
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

    let Some(status) = wait_with_timeout_inner(&mut child, timeout, kill_process_group, cancel)?
    else {
        return Ok(None);
    };
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

/// `SIG_IGN` survives `exec()`, so children spawned inside `IgnoreSignalsGuard` would
/// otherwise be impossible to Ctrl+C.
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

pub fn get_pane_pid(session_name: &str) -> Option<u32> {
    let output = crate::tmux::utils::primary_command(session_name, "#{pane_pid}", false)
        .ok()?
        .output()
        .ok()?;

    if !output.status.success() {
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

    let pid = String::from_utf8_lossy(&output.stdout).trim().parse().ok();
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

/// Every supplied signal must match: an exact environment entry, a command-line
/// substring, and/or an argv token with an exact basename. Empty signals never match.
/// One process-table pass; call from `spawn_blocking` on async runtimes.
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

/// Read-only kernel birth observation, not a signalling handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProcessIncarnation {
    pub(crate) pid: u32,
    pub(crate) group: u32,
    pub(crate) start: [u64; 2],
    pub(crate) namespace: [u64; 2],
}

/// Durable liveness metadata, never an execution or cleanup capability.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OriginalCustodianBirth {
    pub(crate) boot: String,
    pub(crate) process: ProcessIncarnation,
    pub(crate) profile: crate::session::DirectoryIdentity,
    pub(crate) session_id: String,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CustodianLiveness {
    Live,
    Lost,
    Uncertain,
}

impl OriginalCustodianBirth {
    pub(crate) fn capture(
        storage: &crate::session::Storage,
        row: &crate::session::Instance,
    ) -> anyhow::Result<Self> {
        storage.verify_profile_identity()?;
        let profile = storage.original_profile_identity()?;
        anyhow::ensure!(
            profile.is_durable(),
            "custodian profile has no durable birth"
        );
        let boot = boot_id().ok_or_else(|| anyhow::anyhow!("custodian boot is unavailable"))?;
        let process = process_incarnation(std::process::id())?
            .ok_or_else(|| anyhow::anyhow!("original custodian process is unavailable"))?;
        anyhow::ensure!(
            process.start[0] != 0 && boot_id().as_deref() == Some(boot.as_str()),
            "custodian process birth cannot be verified"
        );
        Ok(Self {
            boot,
            process,
            profile,
            session_id: row.id.clone(),
            created_at: row.created_at,
            generation: row.lifecycle_generation,
        })
    }

    pub(crate) fn observe(&self) -> CustodianLiveness {
        let boot = boot_id();
        let namespace = process_namespace();
        if self.boot.is_empty()
            || self.process.start[0] == 0
            || !(2..=i32::MAX as u32).contains(&self.process.pid)
            || boot.as_deref() != Some(self.boot.as_str())
            || namespace.as_ref().ok() != Some(&self.process.namespace)
        {
            return CustodianLiveness::Uncertain;
        }
        let observed = process_incarnation(self.process.pid);
        let absent = if matches!(&observed, Ok(None)) {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                platform::custodian_process_absent(self.process.pid)
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            {
                false
            }
        } else {
            false
        };
        self.classify(boot.as_deref(), namespace, observed, absent)
    }

    fn classify(
        &self,
        boot: Option<&str>,
        namespace: std::io::Result<[u64; 2]>,
        observed: std::io::Result<Option<ProcessIncarnation>>,
        absent: bool,
    ) -> CustodianLiveness {
        if self.boot.is_empty()
            || self.process.start[0] == 0
            || !(2..=i32::MAX as u32).contains(&self.process.pid)
            || boot != Some(self.boot.as_str())
            || namespace.as_ref().ok() != Some(&self.process.namespace)
        {
            return CustodianLiveness::Uncertain;
        }
        match observed {
            Ok(Some(process))
                if process.pid == self.process.pid
                    && process.start[0] != 0
                    && process.namespace == self.process.namespace =>
            {
                if process.start == self.process.start {
                    CustodianLiveness::Live
                } else {
                    CustodianLiveness::Lost
                }
            }
            Ok(None) if absent => CustodianLiveness::Lost,
            _ => CustodianLiveness::Uncertain,
        }
    }
}

pub(crate) fn process_namespace() -> std::io::Result<[u64; 2]> {
    #[cfg(target_os = "linux")]
    {
        linux::process_namespace()
    }
    #[cfg(target_os = "macos")]
    {
        macos::process_namespace()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "process namespace unavailable",
        ))
    }
}

/// Observe a local process birth; missing processes return `None`, inaccessible metadata an error.
pub fn process_incarnation(pid: u32) -> std::io::Result<Option<ProcessIncarnation>> {
    if !(2..=i32::MAX as u32).contains(&pid) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid process id",
        ));
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        platform::process_incarnation(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "process incarnation unavailable",
        ))
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

/// Prevents idle sleep while sessions are active. The backing child exits on its own
/// when the daemon dies, so the assertion releases even without `Drop`.
pub trait SleepInhibit: Send {
    fn acquire(&mut self) -> anyhow::Result<()>;
    fn release(&mut self);
    /// Also true when respawning would be futile (backend unavailable, unsupported platform).
    fn is_held_alive(&mut self) -> bool;
}

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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
struct NoopInhibitor;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl SleepInhibit for NoopInhibitor {
    fn acquire(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    fn release(&mut self) {}

    fn is_held_alive(&mut self) -> bool {
        true
    }
}

/// Process-global because the reconciler rebuilds the inhibitor on every reacquire.
#[cfg(any(target_os = "linux", target_os = "macos"))]
static SLEEP_INHIBIT_UNAVAILABLE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sleep_inhibit_unavailable() -> bool {
    SLEEP_INHIBIT_UNAVAILABLE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Optimistic: `true` only means no failure has latched yet.
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn latch_sleep_inhibit_unavailable(reason: &str) {
    if !SLEEP_INHIBIT_UNAVAILABLE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(target: "process.sleep_inhibit", "{reason}");
    }
}

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
    }
}

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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn process_exists(pid: u32) -> bool {
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// Pauses output while a mobile client reads scrollback; the server guarantees SIGCONT on disconnect.
pub fn stop_process_tree(pid: u32) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    signal_process_tree(pid, Signal::SIGSTOP);

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
    }
}

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
    fn custodian_liveness_requires_same_boot_namespace_and_exact_birth() {
        let birth = OriginalCustodianBirth {
            boot: "boot".into(),
            process: ProcessIncarnation {
                pid: 4312,
                group: 4312,
                start: [123, 456],
                namespace: [7, 8],
            },
            profile: crate::session::DirectoryIdentity {
                device: 1,
                inode: 2,
                birth_time: Some(std::time::UNIX_EPOCH),
            },
            session_id: "owner".into(),
            created_at: chrono::Utc::now(),
            generation: 4,
        };
        let mut changed_group = birth.process;
        changed_group.group += 1;
        let mut recycled = birth.process;
        recycled.start[0] += 1;
        let mut foreign = birth.process;
        foreign.namespace[1] += 1;
        for (boot, namespace, observed, absent, expected) in [
            (
                Some("boot"),
                Ok([7, 8]),
                Ok(Some(birth.process)),
                false,
                CustodianLiveness::Live,
            ),
            (
                Some("boot"),
                Ok([7, 8]),
                Ok(Some(changed_group)),
                false,
                CustodianLiveness::Live,
            ),
            (
                Some("boot"),
                Ok([7, 8]),
                Ok(Some(recycled)),
                false,
                CustodianLiveness::Lost,
            ),
            (
                Some("boot"),
                Ok([7, 8]),
                Ok(Some(foreign)),
                false,
                CustodianLiveness::Uncertain,
            ),
            (
                Some("boot"),
                Ok([7, 8]),
                Ok(None),
                false,
                CustodianLiveness::Uncertain,
            ),
            (
                Some("boot"),
                Ok([7, 8]),
                Ok(None),
                true,
                CustodianLiveness::Lost,
            ),
            (
                Some("boot"),
                Ok([7, 9]),
                Ok(None),
                true,
                CustodianLiveness::Uncertain,
            ),
            (
                Some("next-boot"),
                Ok([7, 8]),
                Ok(None),
                true,
                CustodianLiveness::Uncertain,
            ),
            (
                None,
                Ok([7, 8]),
                Ok(None),
                true,
                CustodianLiveness::Uncertain,
            ),
            (
                Some("boot"),
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                Ok(None),
                true,
                CustodianLiveness::Uncertain,
            ),
            (
                Some("boot"),
                Ok([7, 8]),
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                false,
                CustodianLiveness::Uncertain,
            ),
        ] {
            assert_eq!(birth.classify(boot, namespace, observed, absent), expected);
        }
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

        let env = vec![String::new(), String::new()];
        let cmd = vec![Some(marker.clone()), Some(absent.clone())];

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

    #[cfg(target_os = "linux")]
    #[test]
    fn processes_matching_detects_live_process_by_env_entry() {
        let marker = format!("aoe_env_marker_{}", std::process::id());

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

    /// Restores signal dispositions even when a test panics.
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
        let _restore = RestoreSignalsOnDrop {
            prev_sigint: Some(prev_sigint),
            prev_sigquit: None,
        };

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
    fn run_with_timeout_process_group_kills_descendants() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("descendant.pid");
        let term_path = tmp.path().join("terminated");
        let mut cmd = Command::new("sh");
        cmd.args([
            "-c",
            "trap 'printf term > \"$2\"' TERM; sleep 10 & child=$!; printf %s \"$child\" > \"$1\"; wait",
            "sh",
        ])
        .arg(&pid_path)
        .arg(&term_path);

        let result = run_with_timeout_process_group(&mut cmd, Duration::from_secs(2)).unwrap();
        assert!(result.is_none(), "process group should time out");
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
            "descendant must be exited or a terminated zombie after the timeout"
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_with_timeout_does_not_wait_for_descendant_output_writer() {
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
