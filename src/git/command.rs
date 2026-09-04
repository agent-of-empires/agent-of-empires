//! Thin tracing wrapper for `git` invocations.
//!
//! Used by the simpler call sites that just want `git foo bar`.output().
//! Streaming-clone and other custom invocations stay inline and emit
//! their own `git.command` events.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

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
    run(cwd, args, false)
}

fn run<I, S>(cwd: &Path, args: I, quiet: bool) -> std::io::Result<Output>
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
    let output = Command::new("git")
        .args(&argv)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .output()?;
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

/// [`run_git_with_timeout`] for callers that have already classified a
/// non-zero exit as an expected, no-op outcome, mirroring the
/// [`run_git`] / [`run_git_quiet`] pair. The timeout itself still logs at
/// WARN: a killed child is never routine.
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
    let mut cmd = Command::new("git");
    cmd.args(&argv)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .env("LC_ALL", "C");
    let result = crate::process::run_with_timeout_process_group(&mut cmd, timeout)?;
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
    use tracing_test::traced_test;

    /// The same failing command is a WARN through `run_git_with_timeout` and a
    /// DEBUG through `run_git_quiet_with_timeout`; the quiet variant must not
    /// drop the record entirely, since the stderr summary is what makes a
    /// surprise diagnosable.
    #[traced_test]
    #[test]
    fn run_git_quiet_demotes_expected_failure_to_debug() {
        let tmp = tempfile::tempdir().unwrap();
        tracing::callsite::rebuild_interest_cache();
        // Not a repository, so `git worktree unlock` exits non-zero: the
        // shape `unlock_worktree` classifies as a harmless no-op.
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

        logs_assert(|lines: &[&str]| {
            let failures = |level: &str| {
                lines
                    .iter()
                    .filter(|l| l.contains(level) && l.contains("git command failed"))
                    .count()
            };
            match (failures("WARN"), failures("DEBUG")) {
                (1, 1) => Ok(()),
                (w, d) => Err(format!(
                    "expected 1 warn and 1 debug failure line, got {w}/{d}"
                )),
            }
        });
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

    #[test]
    fn redact_strips_basic_userinfo() {
        assert_eq!(
            redact(&OsString::from("https://user:pat@github.com/x/y.git")),
            "https://***@github.com/x/y.git"
        );
    }

    #[test]
    fn redact_passes_clean_urls_through() {
        assert_eq!(
            redact(&OsString::from("git@github.com:foo/bar.git")),
            "git@github.com:foo/bar.git"
        );
        assert_eq!(
            redact(&OsString::from("https://github.com/foo/bar.git")),
            "https://github.com/foo/bar.git"
        );
    }

    #[test]
    fn redact_passes_non_url_args_through() {
        assert_eq!(redact(&OsString::from("--prune")), "--prune");
        assert_eq!(redact(&OsString::from("main")), "main");
    }
}
