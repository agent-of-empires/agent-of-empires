//! Thin tracing wrapper for `git` invocations.
//!
//! Used by the simpler call sites that just want `git foo bar`.output().
//! Streaming-clone and other custom invocations stay inline and emit
//! their own `git.command` events.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, Instant};

/// The user's own system/global `core.sshCommand` and `credential.helper`,
/// as lowercase `(key, value)` pairs. `None` when git cannot answer within the
/// deadline (a stalled `include.path`, or the caller's `deadline`) or fails to spawn.
fn discover_user_config(
    envs: &[(&str, &Path)],
    deadline: Option<Instant>,
) -> Option<Vec<(String, String)>> {
    let mut found = Vec::new();
    for scope in ["--system", "--global"] {
        let mut cmd = Command::new("git");
        cmd.args([
            "config",
            scope,
            "--includes",
            "--null",
            "--get-regexp",
            r"^(core\.sshcommand|credential\.helper)$",
        ])
        .envs(envs.iter().copied())
        .stdin(Stdio::null());
        let mut limit = Duration::from_secs(2);
        if let Some(deadline) = deadline {
            limit = limit.min(deadline.checked_duration_since(Instant::now())?);
        }
        let out = crate::process::run_with_timeout(&mut cmd, limit).ok()??;
        // 1 is "no match"; anything else is an unreadable config.
        if !matches!(out.status.code(), Some(0 | 1)) {
            return None;
        }
        found.extend(
            String::from_utf8_lossy(&out.stdout)
                .split('\0')
                .filter_map(|record| record.split_once('\n'))
                .map(|(k, v)| (k.to_owned(), v.to_owned())),
        );
    }
    Some(found)
}

/// `-c` overrides that pin what a repo-local config could use to run code on
/// the host: repo values never apply, the user's own do.
fn build_hardening_args(user: &[(String, String)]) -> Vec<String> {
    let values = |key: &str| -> Vec<String> {
        user.iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    };
    let mut kv = vec![
        "core.fsmonitor=false".to_owned(),
        "protocol.ext.allow=never".to_owned(),
    ];
    let ssh = values("core.sshcommand");
    if ssh.is_empty() {
        kv.push("core.sshCommand=ssh".to_owned());
    }
    kv.extend(ssh.into_iter().map(|v| format!("core.sshCommand={v}")));
    // An empty `credential.helper` resets the list the repo config built up.
    kv.push("credential.helper=".to_owned());
    kv.extend(
        values("credential.helper")
            .into_iter()
            .map(|v| format!("credential.helper={v}")),
    );
    kv.into_iter()
        .flat_map(|kv| ["-c".to_owned(), kv])
        .collect()
}

/// Cached only after a complete discovery, so a timed-out one is retried.
/// `None` means `deadline` passed while waiting for the cache or discovery.
fn hardening_args_in(
    cache: &Mutex<Option<Vec<String>>>,
    envs: &[(&str, &Path)],
    deadline: Option<Instant>,
) -> Option<Vec<String>> {
    let mut guard = loop {
        match cache.try_lock() {
            Ok(guard) => break guard,
            Err(TryLockError::Poisoned(e)) => break e.into_inner(),
            Err(TryLockError::WouldBlock) => match deadline {
                None => break cache.lock().unwrap_or_else(|e| e.into_inner()),
                Some(d) if Instant::now() >= d => return None,
                Some(_) => std::thread::sleep(Duration::from_millis(5)),
            },
        }
    };
    if let Some(args) = guard.as_ref() {
        return Some(args.clone());
    }
    match discover_user_config(envs, deadline) {
        Some(user) => Some(guard.insert(build_hardening_args(&user)).clone()),
        None if deadline.is_some_and(|d| Instant::now() >= d) => None,
        None => Some(build_hardening_args(&[])),
    }
}

/// `git` with config a sandbox-written `.git/config` cannot steer: no
/// `core.fsmonitor`, repo-local `core.sshCommand` or `credential.helper`, or
/// `ext::` transport. Repo hooks never run.
pub fn hardened_git() -> Command {
    hardened_git_within(None).expect("discovery cannot time out without a deadline")
}

/// [`hardened_git`] for a caller with a deadline: `None` once `deadline` passes
/// before the user's git config has been read.
pub fn hardened_git_within(deadline: Option<Instant>) -> Option<Command> {
    static CACHE: Mutex<Option<Vec<String>>> = Mutex::new(None);
    let mut cmd = Command::new("git");
    cmd.args(hardening_args_in(&CACHE, &[], deadline)?);
    cmd.args(["-c", "core.hooksPath=/dev/null"]);
    cmd.env("LC_ALL", "C");
    Some(cmd)
}

/// Run `git <args>` in `cwd`, instrumented with target `git.command`.
/// Logs a debug line before, then debug (success) or warn (failure)
/// after with exit code, duration, and a sanitized stderr summary.
///
/// `args` may contain URLs with embedded credentials; we strip the
/// userinfo before logging so tokens don't end up on disk.
pub fn run_git<I, S>(cwd: &Path, args: I) -> std::io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run(cwd, args, false, false)
}

/// [`run_git`] at background priority, for git commands that delete many files.
pub fn run_git_throttled<I, S>(cwd: &Path, args: I) -> std::io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run(cwd, args, false, true)
}

fn run<I, S>(cwd: &Path, args: I, quiet: bool, throttled: bool) -> std::io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let argv: Vec<std::ffi::OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let redacted: Vec<String> = argv.iter().map(|a| redact(a.as_os_str())).collect();
    let start = Instant::now();
    tracing::debug!(
        target: "git.command",
        args = ?redacted,
        cwd = %cwd.display(),
        "running git"
    );
    // Callers classify several Git failures. Keep diagnostics deterministic
    // instead of depending on the launching shell's locale.
    let mut command = hardened_git();
    command.args(&argv).current_dir(cwd);
    if throttled {
        crate::process::throttle_child(&mut command);
    }
    let output = command.output()?;
    let dur = start.elapsed().as_millis() as u64;
    if output.status.success() {
        tracing::debug!(
            target: "git.command",
            args = ?redacted,
            exit = output.status.code(),
            duration_ms = dur,
            "git command completed"
        );
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr_summary: String = stderr.chars().take(200).collect();
        if quiet {
            tracing::debug!(
                target: "git.command",
                args = ?redacted,
                exit = output.status.code(),
                duration_ms = dur,
                stderr_summary = %stderr_summary,
                "git command failed (expected by caller)"
            );
        } else {
            tracing::warn!(
                target: "git.command",
                args = ?redacted,
                exit = output.status.code(),
                duration_ms = dur,
                stderr_summary = %stderr_summary,
                "git command failed"
            );
        }
    }
    Ok(output)
}

/// Like [`run_git`], but kills the child if it outlives `timeout` and reports
/// the timeout as `Ok(None)`.
///
/// Used for bounded git mutations that normally finish quickly but could
/// otherwise hang indefinitely on a stalled filesystem. stdin is nulled so a
/// child can never block on a prompt, and `run_with_timeout_process_group`
/// captures stdout/stderr in temporary regular files rather than pipes, so a
/// grandchild that inherits the handles cannot stall the wait past `timeout`.
pub fn run_git_with_timeout<I, S>(
    cwd: &Path,
    args: I,
    timeout: Duration,
) -> std::io::Result<Option<Output>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_timed(cwd, args, timeout, false)
}

/// [`run_git_with_timeout`] with DEBUG logging for expected non-zero exits.
/// Timeouts still log at WARN.
pub fn run_git_quiet_with_timeout<I, S>(
    cwd: &Path,
    args: I,
    timeout: Duration,
) -> std::io::Result<Option<Output>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_timed(cwd, args, timeout, true)
}

fn run_timed<I, S>(
    cwd: &Path,
    args: I,
    timeout: Duration,
    quiet: bool,
) -> std::io::Result<Option<Output>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let argv: Vec<std::ffi::OsString> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
    let redacted: Vec<String> = argv.iter().map(|a| redact(a.as_os_str())).collect();
    let start = Instant::now();
    tracing::debug!(
        target: "git.command",
        args = ?redacted,
        cwd = %cwd.display(),
        timeout_s = timeout.as_secs(),
        "running git with timeout"
    );
    let result = match hardened_git_within(Some(start + timeout)) {
        Some(mut cmd) => {
            cmd.args(&argv).current_dir(cwd).stdin(Stdio::null());
            let remaining = timeout.saturating_sub(start.elapsed());
            crate::process::run_with_timeout_process_group(&mut cmd, remaining)?
        }
        None => None,
    };
    let dur = start.elapsed().as_millis() as u64;
    match &result {
        Some(output) if output.status.success() => tracing::debug!(
            target: "git.command",
            args = ?redacted,
            exit = output.status.code(),
            duration_ms = dur,
            "git command completed"
        ),
        Some(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr_summary: String = stderr.chars().take(200).collect();
            if quiet {
                tracing::debug!(
                    target: "git.command",
                    args = ?redacted,
                    exit = output.status.code(),
                    duration_ms = dur,
                    stderr_summary = %stderr_summary,
                    "git command failed (expected by caller)"
                );
            } else {
                tracing::warn!(
                    target: "git.command",
                    args = ?redacted,
                    exit = output.status.code(),
                    duration_ms = dur,
                    stderr_summary = %stderr_summary,
                    "git command failed"
                );
            }
        }
        None => tracing::warn!(
            target: "git.command",
            args = ?redacted,
            duration_ms = dur,
            timeout_s = timeout.as_secs(),
            "git command timed out and was killed"
        ),
    }
    Ok(result)
}

fn redact(arg: &OsStr) -> String {
    let s = arg.to_string_lossy();
    if let Some(scheme_end) = s.find("://") {
        let after = &s[scheme_end + 3..];
        if let Some(at_off) = after.find('@') {
            let prefix = &s[..scheme_end + 3];
            let rest = &after[at_off + 1..];
            return format!("{prefix}***@{rest}");
        }
    }
    s.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn run_git_quiet_demotes_expected_failure_to_debug() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = crate::session::test_support::LogCapture::start();
        // Unlocking outside a repository fails without affecting any worktree.
        let args = ["worktree", "unlock", "/nonexistent"];
        let timeout = Duration::from_secs(30);
        let loud = run_git_with_timeout(tmp.path(), args, timeout)
            .expect("git should spawn")
            .expect("git should not time out");
        let quiet = run_git_quiet_with_timeout(tmp.path(), args, timeout)
            .expect("git should spawn")
            .expect("git should not time out");
        assert!(!loud.status.success());
        assert!(!quiet.status.success());

        let logs = logs.contents();
        let failures = |level: &str| {
            logs.lines()
                .filter(|line| line.contains(level) && line.contains("git command failed"))
                .count()
        };
        assert_eq!(
            (failures("WARN"), failures("DEBUG")),
            (1, 1),
            "expected 1 warn and 1 debug failure line: {logs}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_git_with_timeout_kills_a_stalled_command() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("blocked-input");
        let status = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo should spawn");
        assert!(status.success(), "mkfifo should create the fixture");
        let args = [OsString::from("hash-object"), fifo.into_os_string()];
        let result = run_git_with_timeout(tmp.path(), args, Duration::from_millis(10))
            .expect("git should spawn");
        assert!(result.is_none(), "git blocked on the FIFO must time out");
    }

    /// A sandbox can write the repo's config and hooks; none of it may run on the host.
    #[cfg(unix)]
    #[test]
    fn repo_written_config_and_hooks_do_not_execute() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let marks = tmp.path().join("marks");
        std::fs::create_dir_all(&marks).unwrap();
        let script = |name: &str, body: &str| {
            let path = tmp.path().join(name);
            std::fs::write(
                &path,
                format!("#!/bin/sh\ntouch {}/{name}\n{body}\n", marks.display()),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let git = |args: &[&str]| {
            let out = run_git(&repo, args).unwrap();
            assert!(
                out.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        std::fs::create_dir(&repo).unwrap();
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "i",
        ]);
        let fsmonitor = script("fsmonitor", "printf '\\0'");
        let fsmonitor_wt = script("fsmonitor-wt", "printf '\\0'");
        let ssh = script("ssh", "exit 1");
        let hook = script("post-checkout", "");
        std::fs::create_dir_all(repo.join(".git/hooks")).unwrap();
        std::fs::copy(&hook, repo.join(".git/hooks/post-checkout")).unwrap();
        for (k, v) in [
            ("core.fsmonitor", fsmonitor.to_str().unwrap()),
            ("core.sshCommand", ssh.to_str().unwrap()),
            ("extensions.worktreeConfig", "true"),
            ("core.hooksPath", repo.join(".git/hooks").to_str().unwrap()),
        ] {
            std::process::Command::new("git")
                .args(["config", k, v])
                .current_dir(&repo)
                .status()
                .unwrap();
        }

        std::fs::write(
            repo.join(".git/config.worktree"),
            format!("[core]\n\tfsmonitor = {}\n", fsmonitor_wt.display()),
        )
        .unwrap();

        // Control: running the fixture does leave its marker.
        assert!(std::process::Command::new(&ssh).status().is_ok());
        assert!(marks.join("ssh").exists(), "fixture must record execution");
        std::fs::remove_file(marks.join("ssh")).unwrap();

        git(&["status"]);
        git(&["checkout", "-q", "-b", "a"]);
        let _ = run_git(&repo, ["ls-remote", "ssh://host/repo"]).unwrap();
        let ran = |name: &str| marks.join(name).exists();
        assert!(!ran("fsmonitor"), "core.fsmonitor ran");
        assert!(!ran("fsmonitor-wt"), "config.worktree fsmonitor ran");
        assert!(!ran("ssh"), "core.sshCommand ran");
        assert!(!ran("post-checkout"), "repo hook ran");
    }

    /// A stalled `include.path` must not hang discovery, and a plain one is read.
    #[cfg(unix)]
    #[test]
    fn user_config_discovery_is_bounded_and_follows_includes() {
        let tmp = tempfile::tempdir().unwrap();
        let included = tmp.path().join("included");
        std::fs::write(&included, "[core]\n\tsshCommand = my-ssh\n").unwrap();
        let fifo = tmp.path().join("stalled");
        assert!(Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let global = |include: &Path| {
            let path = tmp.path().join("gitconfig");
            std::fs::write(
                &path,
                format!("[include]\n\tpath = {}\n", include.display()),
            )
            .unwrap();
            path
        };
        let none = Path::new("/dev/null");

        let ok = global(&included);
        let found = discover_user_config(
            &[("GIT_CONFIG_SYSTEM", none), ("GIT_CONFIG_GLOBAL", &ok)],
            None,
        );
        assert_eq!(
            found,
            Some(vec![("core.sshcommand".into(), "my-ssh".into())])
        );

        let stalled = global(&fifo);
        let start = Instant::now();
        let found = discover_user_config(
            &[("GIT_CONFIG_SYSTEM", none), ("GIT_CONFIG_GLOBAL", &stalled)],
            None,
        );
        assert_eq!(found, None);
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    /// The caller's deadline covers discovery and the cache lock wait.
    #[cfg(unix)]
    #[test]
    fn hardening_discovery_honors_the_caller_deadline() {
        let tmp = tempfile::tempdir().unwrap();
        let fifo = tmp.path().join("stalled");
        assert!(Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let global = tmp.path().join("gitconfig");
        std::fs::write(&global, format!("[include]\n\tpath = {}\n", fifo.display())).unwrap();
        let none = Path::new("/dev/null");
        let envs = [("GIT_CONFIG_SYSTEM", none), ("GIT_CONFIG_GLOBAL", &*global)];

        let cache = Mutex::new(None);
        let start = Instant::now();
        let args = hardening_args_in(&cache, &envs, Some(start + Duration::from_millis(300)));
        assert_eq!(args, None);
        assert!(start.elapsed() < Duration::from_millis(1500));
        assert!(
            cache.lock().unwrap().is_none(),
            "a timeout must not be cached"
        );

        let held = Mutex::new(None);
        let _guard = held.lock().unwrap();
        let start = Instant::now();
        let args = hardening_args_in(&held, &[], Some(start + Duration::from_millis(100)));
        assert_eq!(args, None);
        assert!(start.elapsed() < Duration::from_millis(1500));
    }

    #[test]
    fn user_config_discovery_rejects_unreadable_config() {
        let tmp = tempfile::tempdir().unwrap();
        let broken = tmp.path().join("gitconfig");
        std::fs::write(&broken, "[core\n\tsshCommand = my-ssh\n").unwrap();
        let none = Path::new("/dev/null");
        let found = discover_user_config(
            &[("GIT_CONFIG_SYSTEM", none), ("GIT_CONFIG_GLOBAL", &broken)],
            None,
        );
        assert_eq!(found, None);
        let missing = tmp.path().join("absent");
        let found = discover_user_config(
            &[("GIT_CONFIG_SYSTEM", none), ("GIT_CONFIG_GLOBAL", &missing)],
            None,
        );
        assert_eq!(found, Some(vec![]));
    }

    #[test]
    fn user_config_discovery_keeps_multiline_values() {
        let tmp = tempfile::tempdir().unwrap();
        let global = tmp.path().join("gitconfig");
        std::fs::write(
            &global,
            "[credential]\n\thelper = \"!f() {\\n echo x\\n}; f\"\n",
        )
        .unwrap();
        let none = Path::new("/dev/null");
        let found = discover_user_config(
            &[("GIT_CONFIG_SYSTEM", none), ("GIT_CONFIG_GLOBAL", &global)],
            None,
        );
        assert_eq!(
            found,
            Some(vec![(
                "credential.helper".into(),
                "!f() {\n echo x\n}; f".into()
            )])
        );
    }

    #[test]
    fn redact_hides_only_url_userinfo() {
        for (arg, expected) in [
            (
                "https://user:pat@github.com/x/y.git",
                "https://***@github.com/x/y.git",
            ),
            ("git@github.com:foo/bar.git", "git@github.com:foo/bar.git"),
            (
                "https://github.com/foo/bar.git",
                "https://github.com/foo/bar.git",
            ),
            ("--prune", "--prune"),
            ("main", "main"),
        ] {
            assert_eq!(redact(&OsString::from(arg)), expected, "{arg}");
        }
    }
}
