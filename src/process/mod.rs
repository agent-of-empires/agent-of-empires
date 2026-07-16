//! Process utilities for tmux session management

use std::collections::HashMap;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

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

/// Protocol-agnostic plumbing for supervised worker subprocesses, lifted
/// out of `src/acp/` so the future plugin host can reuse it. Serve-gated
/// because its only consumer today is the serve-gated `acp` module.
#[cfg(feature = "serve")]
pub mod worker;

const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);

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
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(WAIT_POLL_INTERVAL);
    }
}

/// Spawn `cmd` with piped stdout/stderr and wait for it to exit, killing it
/// if it outlives `timeout`. Returns `Ok(None)` when the timeout fired and
/// the child was killed; `Err` covers spawn/wait failures.
///
/// stdout/stderr are drained on dedicated threads so a full pipe buffer
/// cannot wedge the child (and thus this wait) before the deadline. The
/// caller keeps control of stdin; pipe it to null when the child might
/// prompt (SSH passphrases, credential helpers).
pub fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> std::io::Result<Option<Output>> {
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn()?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let (otx, orx) = mpsc::channel();
    let (etx, erx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = stdout_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = otx.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = stderr_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = etx.send(buf);
    });

    let deadline = Instant::now() + timeout;
    let Some(status) = wait_with_timeout(&mut child, timeout)? else {
        return Ok(None);
    };
    // The child exited, but if it spawned a grandchild that inherited the
    // pipe (credential helper, pager, backgrounded job), `read_to_end` (and
    // thus an unbounded `recv`) would block forever. Cap the drain at the
    // remaining deadline so the timeout guarantee holds even then; the exit
    // status is already in hand.
    let remaining = deadline.saturating_duration_since(Instant::now());
    let stdout = orx.recv_timeout(remaining).unwrap_or_default();
    let remaining = deadline.saturating_duration_since(Instant::now());
    let stderr = erx.recv_timeout(remaining).unwrap_or_default();
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

/// Get the PID of the shell process running in a tmux pane
pub fn get_pane_pid(session_name: &str) -> Option<u32> {
    // Use `^.0` to target the first window's first pane regardless of
    // base-index or which pane is active, so we always query the agent's
    // pane even when the user has created additional tmux windows or split
    // panes.  See #435, #488.
    let target = format!("{session_name}:^.0");
    let output = crate::tmux::tmux_command()
        .args(["display-message", "-t", &target, "-p", "#{pane_pid}"])
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

/// Portable "is this pid still around?" check via kill(pid, 0).
/// EPERM means the process exists but we lack permission (still exists).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn process_exists(pid: u32) -> bool {
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::EPERM) => true,
        Err(_) => false,
    }
}

/// Send SIGSTOP to a process and all its descendants. Used to pause
/// the agent (claude) while a mobile client is reading tmux scrollback
/// — without this, claude's continued output keeps pushing lines into
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
    fn test_collect_descendants_from_map_empty() {
        let children_map = HashMap::new();
        let mut pids = vec![100];
        collect_descendants_from_map(100, &children_map, &mut pids);
        assert_eq!(pids, vec![100]);
    }

    #[test]
    fn test_collect_descendants_from_map_single_child() {
        let mut children_map = HashMap::new();
        children_map.insert(100, vec![101]);

        let mut pids = vec![100];
        collect_descendants_from_map(100, &children_map, &mut pids);
        assert_eq!(pids, vec![100, 101]);
    }

    #[test]
    fn test_collect_descendants_from_map_multiple_children() {
        let mut children_map = HashMap::new();
        children_map.insert(100, vec![101, 102, 103]);

        let mut pids = vec![100];
        collect_descendants_from_map(100, &children_map, &mut pids);
        assert_eq!(pids, vec![100, 101, 102, 103]);
    }

    #[test]
    fn test_collect_descendants_from_map_nested() {
        // Tree: 100 -> 101 -> 102 -> 103
        let mut children_map = HashMap::new();
        children_map.insert(100, vec![101]);
        children_map.insert(101, vec![102]);
        children_map.insert(102, vec![103]);

        let mut pids = vec![100];
        collect_descendants_from_map(100, &children_map, &mut pids);
        assert_eq!(pids, vec![100, 101, 102, 103]);
    }

    #[test]
    fn test_collect_descendants_from_map_branching() {
        // Tree: 100 -> [101, 102], 101 -> [103, 104], 102 -> [105]
        let mut children_map = HashMap::new();
        children_map.insert(100, vec![101, 102]);
        children_map.insert(101, vec![103, 104]);
        children_map.insert(102, vec![105]);

        let mut pids = vec![100];
        collect_descendants_from_map(100, &children_map, &mut pids);

        assert!(pids.contains(&100));
        assert!(pids.contains(&101));
        assert!(pids.contains(&102));
        assert!(pids.contains(&103));
        assert!(pids.contains(&104));
        assert!(pids.contains(&105));
        assert_eq!(pids.len(), 6);
    }

    #[test]
    fn test_collect_descendants_unrelated_processes() {
        let mut children_map = HashMap::new();
        children_map.insert(200, vec![201, 202]);
        children_map.insert(300, vec![301]);

        let mut pids = vec![100];
        collect_descendants_from_map(100, &children_map, &mut pids);
        assert_eq!(pids, vec![100]);
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

    #[test]
    #[cfg(unix)]
    fn wait_with_timeout_kills_child_that_outlives_deadline() {
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();

        let start = Instant::now();
        let status = wait_with_timeout(&mut child, Duration::from_millis(200)).unwrap();
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
    fn run_with_timeout_kills_child_that_outlives_deadline() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");

        let start = Instant::now();
        let result = run_with_timeout(&mut cmd, Duration::from_millis(300)).unwrap();
        assert!(
            result.is_none(),
            "expected the timeout to fire and kill the child"
        );
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "wait should return promptly after the deadline, not block on the child"
        );
    }

    #[test]
    #[cfg(unix)]
    fn run_with_timeout_bounds_drain_when_grandchild_holds_pipe() {
        // The immediate child (sh) exits fast but backgrounds a `sleep` that
        // inherits the pipes, so they never close. The drain must still
        // return by the deadline rather than blocking on read_to_end; this is
        // the exact shape of the git-clone hang (a credential helper or pager
        // grandchild outliving the parent). `sleep 10` (>> the 4s assertion)
        // ensures an unbounded recv would visibly fail.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 10 & printf done"]);

        let start = Instant::now();
        let output = run_with_timeout(&mut cmd, Duration::from_millis(500))
            .unwrap()
            .expect("the sh child exits quickly, so an Output is produced");
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "drain must be bounded by the deadline even while the pipe stays open"
        );
        assert!(output.status.success());
    }
}
