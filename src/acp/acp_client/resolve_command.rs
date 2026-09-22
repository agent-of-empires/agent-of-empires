//! Resolving an agent name to a command: PATH lookup, the bundled copy, and
//! the version floor a PATH copy has to clear.

use std::process::{Command, Stdio};
use tracing::warn;

/// A resolved agent binary plus the dirs to prepend to the child's PATH, so
/// the adapter's own `node` / `npx` subprocesses resolve against the same
/// install.
pub struct ResolvedAgentCommand {
    pub path: std::path::PathBuf,
    pub prepend_paths: Vec<std::path::PathBuf>,
}

/// PATH first, so a user's explicit install wins, unless it is below the
/// startup gate's version floor and a pinned bundled copy exists; then the
/// on-demand bundled adapter (#1017), then the node-version-manager scan.
/// Uncached, so an `nvm use` after daemon start takes effect immediately.
/// `None` for a path, a `${placeholder}`, or a binary found nowhere.
///
/// `app_dir` is optional so a `get_app_dir` failure degrades to PATH plus the
/// node-manager scan rather than resolving nothing (#1048).
pub fn resolve_agent_command(
    command: &str,
    app_dir: Option<&std::path::Path>,
) -> Option<ResolvedAgentCommand> {
    if command.contains('/') || command.contains('\\') || command.contains("${") {
        return None;
    }

    if let Some(path) = find_in_path_env(command) {
        let bundled = app_dir.and_then(|d| crate::acp::adapters::bundled_adapter_bin(d, command));
        // Probe only when a bundle exists to fall back to.
        match bundled {
            Some(bundled_path) if path_copy_below_floor(command, &path) => {
                warn!(
                    target: "acp.adapters",
                    adapter = command,
                    path = %path.display(),
                    "PATH copy is below the supported version floor; using the bundled pinned copy"
                );
                return Some(bundled_resolution(bundled_path, app_dir, command));
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
        if app_dir.is_some_and(|d| crate::acp::adapters::installed_copy_is_stale(d, command)) {
            warn!(
                target: "acp.adapters",
                adapter = command,
                "installed copy predates this aoe build; refusing it until it is reinstalled"
            );
            return None;
        }
        if let Some(found) =
            app_dir.and_then(|d| crate::acp::adapters::runtime_too_old_for(d, command))
        {
            warn!(
                target: "acp.adapters",
                adapter = command,
                found,
                "Node cannot run this adapter's sources (needs {}.{}); refusing it",
                crate::acp::node::MIN_NODE_MAJOR,
                crate::acp::node::MIN_NODE_MINOR
            );
            return None;
        }
        return Some(bundled_resolution(path, app_dir, command));
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

/// The npm `.bin` shim is `#!/usr/bin/env node`, so the interpreter must also
/// be reachable: add the Node aoe uses for the adapter to the child PATH.
pub(super) fn bundled_resolution(
    path: std::path::PathBuf,
    app_dir: Option<&std::path::Path>,
    command: &str,
) -> ResolvedAgentCommand {
    let mut prepend_paths = Vec::new();
    if let Some(dir) = path.parent() {
        prepend_paths.push(dir.to_path_buf());
    }
    let sources = crate::acp::adapters::ships_sources(command);
    if let Some(node) = app_dir.and_then(|d| crate::acp::node::resolve_for("", d, sources).ok()) {
        if let Some(node_bin) = node.path.parent() {
            prepend_paths.push(node_bin.to_path_buf());
        }
    }
    ResolvedAgentCommand {
        path,
        prepend_paths,
    }
}

/// Conservative: a failed probe or unparseable output reads as false, so an
/// unknown version keeps the user's own copy.
pub(super) fn path_copy_below_floor(command: &str, path: &std::path::Path) -> bool {
    let Some(gate) = crate::acp::agent_compat::version_gate_for(
        crate::acp::agent_compat::ExpectedAgent::from_command(command),
    ) else {
        return false;
    };
    let Ok(min) = semver::Version::parse(gate.min_version) else {
        return false;
    };
    let Some(raw) = probe_version_bounded(Command::new(path).arg("--version")) else {
        return false;
    };
    crate::acp::version_probe::whitespace_token_below_floor(&raw, min)
}

/// Bounded to the synchronous spawn path's budget.
fn probe_version_bounded(command: &mut Command) -> Option<String> {
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

    let mut child = command
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
                        path = %std::path::Path::new(command.get_program()).display(),
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

/// Node bin dirs the adapter is likely installed into. First hit wins.
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

    /// A path or a `${placeholder}` is already resolved, so it is left alone.
    #[test]
    fn resolve_agent_command_skips_paths_and_placeholders() {
        let app = std::path::Path::new("/nonexistent-app-dir");
        for command in [
            "/usr/local/bin/claude-agent-acp",
            "./relative/path",
            "${aoe_data_dir}/acp-worker/dist/aoe-agent",
        ] {
            assert!(
                resolve_agent_command(command, Some(app)).is_none(),
                "{command}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn resolve_agent_command_falls_back_to_bundled_when_not_on_path() {
        // PATH-scrubbed because the adapter names are real: a dev machine
        // with a global `claude-agent-acp` would (correctly) resolve that
        // copy instead of the bundled one.
        let app = tempfile::TempDir::new().unwrap();
        let name = "claude-agent-acp";
        let bin_dir = app
            .path()
            .join("acp-worker/adapters/claude-agent-acp/node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bin = bin_dir.join(name);
        std::fs::write(&bin, "#!/usr/bin/env node\n").unwrap();

        let empty = tempfile::TempDir::new().unwrap();
        let _path = crate::session::test_support::EnvGuard::set(&[("PATH", empty.path())]);
        let resolved = resolve_agent_command(name, Some(app.path()))
            .expect("should resolve from the bundled adapter dir");
        assert_eq!(resolved.path, bin);
        assert_eq!(resolved.prepend_paths.first(), Some(&bin_dir));
    }

    /// The probe reads a version off a cooperative binary and abandons one
    /// that hangs, proving through `entered` that it really did start.
    #[cfg(unix)]
    #[test]
    fn probe_version_bounded_reads_output_and_gives_up_on_a_hang() {
        let dir = tempfile::TempDir::new().unwrap();
        let prints = dir.path().join("prints");
        std::fs::write(&prints, "echo 0.61.0\n").unwrap();
        // Reproduce a concurrent fork retaining the fixture writer.
        let _writer = std::fs::OpenOptions::new()
            .write(true)
            .open(&prints)
            .unwrap();
        let out = probe_version_bounded(Command::new("/bin/sh").arg(&prints))
            .expect("should capture stdout");
        assert_eq!(out.trim(), "0.61.0");

        let hangs = dir.path().join("hangs");
        let entered = dir.path().join("entered");
        std::fs::write(&hangs, "printf entered > \"$1\"\nexec /bin/sleep 30\n").unwrap();
        let started = std::time::Instant::now();
        assert!(probe_version_bounded(Command::new("/bin/sh").arg(&hangs).arg(&entered)).is_none());
        assert_eq!(std::fs::read(&entered).unwrap(), b"entered");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "probe should abandon a hanging binary"
        );
    }

    /// #1048: without an app dir, resolution still falls through to PATH and
    /// the node-manager scan rather than collapsing to nothing.
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
        let dir = tempfile::TempDir::new().unwrap();
        let bin = dir.path().join("aoe-test-resolver-fake");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _path = crate::session::test_support::path_prepended(dir.path());
        let resolved = resolve_agent_command("aoe-test-resolver-fake", None)
            .expect("binary should resolve from PATH");
        assert_eq!(resolved.path, bin);
        assert_eq!(resolved.prepend_paths, vec![dir.path().to_path_buf()]);
    }
}
