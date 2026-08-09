//! Thin tracing wrapper for `git` invocations.
//!
//! Used by the simpler call sites that just want `git foo bar`.output().
//! Streaming-clone and other custom invocations stay inline and emit
//! their own `git.command` events.

use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Instant;

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
        tracing::warn!(
            target: "git.command",
            args = ?redacted,
            exit = output.status.code(),
            duration_ms = dur,
            stderr_summary = %stderr_summary,
            "git command failed"
        );
    }
    Ok(output)
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
