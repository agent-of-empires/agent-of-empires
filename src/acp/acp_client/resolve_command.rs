//! Resolving an agent name to a command: PATH lookup, the bundled copy, and
//! the version floor a PATH copy has to clear.

use std::process::Stdio;
use tracing::warn;

/// Resolve a bare agent command name to an absolute path, scanning common
/// node-version-manager bin dirs (nvm, fnm, mise, asdf, Volta) plus the
/// usual system locations. Returns the absolute binary path and the bin
/// dir we found it in; the caller prepends that dir to the agent's PATH
/// so the adapter's own subprocesses (`node`, `npx`) can still resolve.
///
/// Re-runs per spawn (no cache) so an `nvm use <other-version>` after the
/// daemon started picks up immediately without a daemon restart. Returns
/// None when the command is already a path, contains a `${placeholder}`,
/// or isn't found anywhere we know to look.
/// A resolved agent binary plus the directories to prepend to the child's
/// PATH before spawning it.
pub struct ResolvedAgentCommand {
    pub path: std::path::PathBuf,
    pub prepend_paths: Vec<std::path::PathBuf>,
}

/// Resolve an agent adapter's binary. PATH first, so a user's explicit
/// install wins, EXCEPT when that copy is below the version floor the
/// startup gate enforces and a pinned bundled copy is available: spawning a
/// binary we know `initialize` will reject, while a compliant one sits in
/// the data dir, helps nobody. Then the bundled adapter aoe installs on
/// demand (see #1017), then the legacy node-version-manager scan.
///
/// `app_dir` is optional so a failure to resolve the data dir degrades to
/// PATH plus the node-manager scan (see #1048) instead of no resolution.
pub fn resolve_agent_command(
    command: &str,
    app_dir: Option<&std::path::Path>,
) -> Option<ResolvedAgentCommand> {
    if command.contains('/') || command.contains('\\') || command.contains("${") {
        return None;
    }

    if let Some(path) = find_in_path_env(command) {
        let bundled = app_dir.and_then(|d| crate::acp::adapters::bundled_adapter_bin(d, command));
        // Only probe the version when there is actually a bundle to fall
        // back to; otherwise the PATH copy is the only option anyway.
        match bundled {
            Some(bundled_path) if path_copy_below_floor(command, &path) => {
                warn!(
                    target: "acp.adapters",
                    adapter = command,
                    path = %path.display(),
                    "PATH copy is below the supported version floor; using the bundled pinned copy"
                );
                return Some(bundled_resolution(bundled_path, app_dir));
            }
            _ => {
                let dir = path
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(std::path::PathBuf::new);
                return Some(ResolvedAgentCommand {
                    path,
                    prepend_paths: vec![dir],
                });
            }
        }
    }

    if let Some(path) = app_dir.and_then(|d| crate::acp::adapters::bundled_adapter_bin(d, command))
    {
        return Some(bundled_resolution(path, app_dir));
    }

    for dir in node_search_dirs() {
        let candidate = dir.join(command);
        if candidate.is_file() {
            return Some(ResolvedAgentCommand {
                path: candidate,
                prepend_paths: vec![dir],
            });
        }
    }
    None
}

/// The npm `.bin` shim is `#!/usr/bin/env node`, so resolving it is not
/// enough: a Node interpreter must be reachable at spawn time. Add the same
/// Node aoe uses for the adapter (the bundled one when the host has none) to
/// the child PATH.
pub(super) fn bundled_resolution(
    path: std::path::PathBuf,
    app_dir: Option<&std::path::Path>,
) -> ResolvedAgentCommand {
    let mut prepend_paths = Vec::new();
    if let Some(dir) = path.parent() {
        prepend_paths.push(dir.to_path_buf());
    }
    if let Some(node) = app_dir.and_then(|d| crate::acp::node::resolve("", d).ok()) {
        if let Some(node_bin) = node.path.parent() {
            prepend_paths.push(node_bin.to_path_buf());
        }
    }
    ResolvedAgentCommand {
        path,
        prepend_paths,
    }
}

/// True when `path` reports a version below the adapter's startup floor.
/// Conservative: any probe failure or unparseable output returns false, so
/// an unknown version keeps the user's own copy rather than overriding it.
pub(super) fn path_copy_below_floor(command: &str, path: &std::path::Path) -> bool {
    let Some(gate) = crate::acp::agent_compat::version_gate_for(
        crate::acp::agent_compat::ExpectedAgent::from_command(command),
    ) else {
        return false;
    };
    let Ok(min) = semver::Version::parse(gate.min_version) else {
        return false;
    };
    let Some(raw) = probe_version_bounded(path) else {
        return false;
    };
    crate::acp::version_probe::whitespace_token_below_floor(&raw, min)
}

/// Run `<path> --version` with a deadline and return its stdout.
///
/// This runs on the synchronous spawn path, so it cannot reuse
/// `version_probe`'s async `tokio::time::timeout`; it polls instead. The
/// bound matters: an adapter that waits on stdin or a network login would
/// otherwise block session spawn forever. It mirrors `version_probe`'s 2s
/// budget; any failure or timeout yields `None` so the caller keeps the
/// user's own copy.
pub(super) fn probe_version_bounded(path: &std::path::Path) -> Option<String> {
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

    let mut child = std::process::Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let out = child.wait_with_output().ok()?;
                return Some(String::from_utf8_lossy(&out.stdout).into_owned());
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    // Reap it so the probe never leaves a zombie behind.
                    let _ = child.kill();
                    let _ = child.wait();
                    warn!(
                        target: "acp.adapters",
                        path = %path.display(),
                        "version probe timed out; keeping the PATH copy"
                    );
                    return None;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(_) => return None,
        }
    }
}

pub(super) fn find_in_path_env(binary: &str) -> Option<std::path::PathBuf> {
    which::which(binary).ok()
}

/// Best-effort enumeration of node bin dirs the user is likely to have
/// the adapter installed into. Order matters only for tie-breaking; the
/// first hit wins, but in practice each binary only lives in one place.
pub(super) fn node_search_dirs() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = dirs::home_dir() {
        // nvm: `~/.nvm/versions/node/v<ver>/bin/<binary>`
        push_subdirs(&mut out, &home.join(".nvm/versions/node"), "bin");
        // fnm: `~/.fnm/node-versions/v<ver>/installation/bin/<binary>`
        push_subdirs(
            &mut out,
            &home.join(".fnm/node-versions"),
            "installation/bin",
        );
        // mise: `~/.local/share/mise/installs/node/<ver>/bin/<binary>`
        push_subdirs(
            &mut out,
            &home.join(".local/share/mise/installs/node"),
            "bin",
        );
        // asdf: `~/.asdf/installs/nodejs/<ver>/bin/<binary>`
        push_subdirs(&mut out, &home.join(".asdf/installs/nodejs"), "bin");
        // Volta + user-scoped npm prefixes
        out.push(home.join(".volta/bin"));
        out.push(home.join(".npm-global/bin"));
        out.push(home.join(".local/bin"));
        out.push(home.join("bin"));
    }
    out.push(std::path::PathBuf::from("/usr/local/bin"));
    out.push(std::path::PathBuf::from("/opt/homebrew/bin"));
    out.push(std::path::PathBuf::from("/usr/bin"));
    out
}

pub(super) fn push_subdirs(out: &mut Vec<std::path::PathBuf>, root: &std::path::Path, leaf: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let bin = entry.path().join(leaf);
        if bin.is_dir() {
            out.push(bin);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_agent_command_returns_none_for_absolute_path() {
        let app = std::path::Path::new("/nonexistent-app-dir");
        assert!(resolve_agent_command("/usr/local/bin/claude-agent-acp", Some(app)).is_none());
        assert!(resolve_agent_command("./relative/path", Some(app)).is_none());
    }

    #[test]
    fn resolve_agent_command_returns_none_for_placeholder() {
        let app = std::path::Path::new("/nonexistent-app-dir");
        assert!(
            resolve_agent_command("${aoe_data_dir}/acp-worker/dist/aoe-agent", Some(app)).is_none()
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolve_agent_command_falls_back_to_bundled_when_not_on_path() {
        // Tagged `#[serial]` and PATH-scrubbed because the adapter names are
        // real: a dev machine with a global `claude-agent-acp` would
        // (correctly) resolve that copy instead of the bundled one.
        let app = tempfile::TempDir::new().unwrap();
        let name = "claude-agent-acp";
        let bin_dir = app
            .path()
            .join("acp-worker/adapters/claude-agent-acp/node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bin = bin_dir.join(name);
        std::fs::write(&bin, "#!/usr/bin/env node\n").unwrap();

        let empty = tempfile::TempDir::new().unwrap();
        let prev = std::env::var_os("PATH");
        // SAFETY: mutates the process-wide PATH; `#[serial]` keeps other
        // PATH readers out of the way.
        unsafe {
            std::env::set_var("PATH", empty.path());
        }
        let resolved = resolve_agent_command(name, Some(app.path()));
        if let Some(prev) = prev {
            unsafe {
                std::env::set_var("PATH", prev);
            }
        }

        let resolved = resolved.expect("should resolve from the bundled adapter dir");
        assert_eq!(resolved.path, bin);
        assert_eq!(resolved.prepend_paths.first(), Some(&bin_dir));
    }

    /// A hanging adapter must not block session spawn: the probe has to give
    /// up on its deadline and report nothing, so the caller keeps the user's
    /// copy rather than waiting forever.
    #[cfg(unix)]
    #[test]
    fn probe_version_bounded_gives_up_on_a_hanging_binary() {
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("hangs");
        std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = std::time::Instant::now();
        assert!(probe_version_bounded(&script).is_none());
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(10),
            "probe should abandon a hanging binary, took {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_version_bounded_reads_version_output() {
        let dir = tempfile::TempDir::new().unwrap();
        let script = dir.path().join("prints");
        std::fs::write(&script, "#!/bin/sh\necho 0.61.0\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let out = probe_version_bounded(&script).expect("should capture stdout");
        assert_eq!(out.trim(), "0.61.0");
    }

    /// Without an app dir (a `get_app_dir` failure) resolution must still
    /// fall through to PATH and the node-manager scan, not collapse to
    /// nothing. Regression guard for #1048.
    #[test]
    #[serial_test::serial]
    fn resolve_agent_command_without_app_dir_still_uses_path() {
        assert!(resolve_agent_command("aoe-definitely-not-installed", None).is_none());
        // `sh` is on PATH everywhere the suite runs.
        let resolved =
            resolve_agent_command("sh", None).expect("PATH resolution must work without app_dir");
        assert!(resolved.path.is_file());
    }

    #[test]
    #[serial_test::serial]
    fn resolve_agent_command_finds_binary_in_path_env() {
        // Build a temp dir with a fake binary, point PATH at it.
        // Tagged `#[serial]` because the test mutates the process-wide
        // PATH; any concurrent test that reads PATH (e.g. resolves a
        // real binary) would race.
        let dir = tempfile::TempDir::new().unwrap();
        let bin = dir.path().join("aoe-test-resolver-fake");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let prev = std::env::var_os("PATH");
        let new_path = format!(
            "{}:{}",
            dir.path().display(),
            prev.as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        // SAFETY: this test mutates the process-wide PATH. Other PATH
        // readers in the same test binary would race; `#[serial]` keeps
        // them apart.
        unsafe {
            std::env::set_var("PATH", &new_path);
        }
        let resolved = resolve_agent_command("aoe-test-resolver-fake", None);
        if let Some(prev) = prev {
            unsafe {
                std::env::set_var("PATH", prev);
            }
        }
        let resolved = resolved.expect("binary should resolve from PATH");
        assert_eq!(resolved.path, bin);
        assert_eq!(resolved.prepend_paths, vec![dir.path().to_path_buf()]);
    }
}
