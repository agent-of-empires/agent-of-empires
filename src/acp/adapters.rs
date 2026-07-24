//! On-demand install and resolution of the npm-distributed ACP adapters
//! that aoe pins (`claude-agent-acp`, `codex-acp`, `pi-acp`).
//!
//! Mirrors the bundled-Node pattern in [`crate::acp::node`]: a pinned
//! manifest is embedded in the binary and installed into the data dir by
//! `aoe acp doctor --fix` using the resolved Node's own npm, instead of
//! `npm install -g` (no global prefix, no sudo, a version aoe controls).
//! See issue #1017.
//!
//! Layout: `$AOE_DATA_DIR/acp-worker/adapters/node_modules/.bin/<binary>`.
//! The install builds into a sibling temp dir and publishes by rename, so
//! a concurrent reader never observes a half-built `node_modules`. A
//! `.aoe-lock-digest` sidecar (SHA-256 of the embedded lockfile), written
//! last, doubles as the completion marker and the upgrade trigger.

use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::{info, warn};

use crate::acp::node::{NodeSource, ResolvedNode};

const PACKAGE_JSON: &[u8] = include_bytes!("../../acp-worker/adapters/package.json");
const PACKAGE_LOCK: &[u8] = include_bytes!("../../acp-worker/adapters/package-lock.json");

/// Binaries the pinned manifest is expected to produce. Kept in lockstep
/// with `acp-worker/adapters/package.json`; used to verify a completed
/// install and to report presence in `aoe acp doctor`.
pub const BUNDLED_ADAPTER_BINS: &[&str] = &["claude-agent-acp", "codex-acp", "pi-acp"];

const DIGEST_FILE: &str = ".aoe-lock-digest";

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("no usable npm found for the resolved Node at {0}")]
    NpmUnavailable(PathBuf),
    #[error("npm ci exited with {0}")]
    NpmFailed(String),
    #[error("adapter binary `{0}` missing after install")]
    BinaryMissing(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// `$AOE_DATA_DIR/acp-worker/adapters`.
pub fn bundled_adapters_dir(app_dir: &Path) -> PathBuf {
    app_dir.join("acp-worker").join("adapters")
}

fn bin_dir(app_dir: &Path) -> PathBuf {
    bundled_adapters_dir(app_dir)
        .join("node_modules")
        .join(".bin")
}

/// Absolute path to a bundled adapter binary if it exists on disk, else
/// `None`. npm writes a `.cmd` shim on Windows.
pub fn bundled_adapter_bin(app_dir: &Path, binary: &str) -> Option<PathBuf> {
    let base = bin_dir(app_dir).join(binary);
    let candidate = if cfg!(windows) {
        base.with_extension("cmd")
    } else {
        base
    };
    candidate.is_file().then_some(candidate)
}

fn expected_digest() -> String {
    sha256_hex(PACKAGE_LOCK)
}

/// True when a complete, current install is present: the digest sidecar
/// matches the embedded lockfile AND every expected binary exists. Used to
/// skip reinstalling and to trigger reinstall after an aoe upgrade bumps
/// the pinned versions.
pub fn installation_is_current(app_dir: &Path) -> bool {
    let dir = bundled_adapters_dir(app_dir);
    let digest_ok = std::fs::read_to_string(dir.join(DIGEST_FILE))
        .map(|s| s.trim() == expected_digest())
        .unwrap_or(false);
    digest_ok
        && BUNDLED_ADAPTER_BINS
            .iter()
            .all(|b| bundled_adapter_bin(app_dir, b).is_some())
}

/// Install (or upgrade) the pinned adapters into the data dir using
/// `node`'s npm. Idempotent: returns early when the current lockfile is
/// already installed.
pub fn install(app_dir: &Path, node: &ResolvedNode) -> Result<(), AdapterError> {
    if installation_is_current(app_dir) {
        info!(target: "acp.adapters", "bundled ACP adapters already current; nothing to install");
        return Ok(());
    }

    let worker_dir = app_dir.join("acp-worker");
    std::fs::create_dir_all(&worker_dir)?;
    sweep_stale(&worker_dir);

    // Build in a sibling temp dir, then publish by rename so readers never
    // see a half-built node_modules.
    // ponytail: no advisory lock, matching the existing non-atomic Node
    // installer; immutable release dirs would remove the tiny upgrade
    // window and are the upgrade path if concurrent installs ever bite.
    let tmp = worker_dir.join(format!("adapters.tmp.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;

    std::fs::write(tmp.join("package.json"), PACKAGE_JSON)?;
    std::fs::write(tmp.join("package-lock.json"), PACKAGE_LOCK)?;

    let (program, args) =
        npm_ci_argv(node).ok_or_else(|| AdapterError::NpmUnavailable(node.path.clone()))?;
    info!(
        target: "acp.adapters",
        program = %program.display(),
        "installing bundled ACP adapters via npm ci"
    );
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&args).current_dir(&tmp);
    // The npm CLI itself is `#!/usr/bin/env node`; make sure the resolved
    // node is reachable when it (or the bundled node) is not already on PATH.
    prepend_dirs_to_path(&mut cmd, node.path.parent());
    let status = cmd.status().inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&tmp);
    })?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(AdapterError::NpmFailed(status.to_string()));
    }

    for b in BUNDLED_ADAPTER_BINS {
        let base = tmp.join("node_modules").join(".bin").join(b);
        let cand = if cfg!(windows) {
            base.with_extension("cmd")
        } else {
            base
        };
        if !cand.is_file() {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(AdapterError::BinaryMissing((*b).to_string()));
        }
    }

    // Completion marker, written last: an interrupted install never leaves
    // a matching digest behind.
    std::fs::write(tmp.join(DIGEST_FILE), format!("{}\n", expected_digest()))?;

    publish(&tmp, &bundled_adapters_dir(app_dir))?;
    info!(target: "acp.adapters", "bundled ACP adapters installed");
    Ok(())
}

/// Build the argv for `npm ci`, run from the target dir. For a bundled
/// Node, invoke its own `npm-cli.js` with that exact node (the official
/// tarball ships it at `<root>/lib/node_modules/npm/bin/npm-cli.js`); for a
/// host Node, use `npm` on PATH, because a host Node's npm layout is not
/// something we can assume. `None` when no usable npm is found.
pub fn npm_ci_argv(node: &ResolvedNode) -> Option<(PathBuf, Vec<String>)> {
    let ci_flags = || {
        vec![
            "ci".to_string(),
            "--no-audit".to_string(),
            "--no-fund".to_string(),
        ]
    };
    if matches!(node.source, NodeSource::Bundled) {
        // `<root>/bin/node` -> `<root>`.
        if let Some(root) = node.path.parent().and_then(|p| p.parent()) {
            let npm_cli = root
                .join("lib")
                .join("node_modules")
                .join("npm")
                .join("bin")
                .join("npm-cli.js");
            if npm_cli.is_file() {
                let mut args = vec![npm_cli.to_string_lossy().into_owned()];
                args.extend(ci_flags());
                return Some((node.path.clone(), args));
            }
        }
    }
    let npm = which::which("npm").ok()?;
    Some((npm, ci_flags()))
}

fn publish(tmp: &Path, final_dir: &Path) -> std::io::Result<()> {
    if !final_dir.exists() {
        return std::fs::rename(tmp, final_dir);
    }
    // `rename` cannot replace a non-empty dir on Unix, so move the old one
    // aside first. An already-running adapter keeps its open inodes.
    let backup = final_dir.with_extension(format!("old.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::rename(final_dir, &backup)?;
    match std::fs::rename(tmp, final_dir) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&backup);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::rename(&backup, final_dir);
            let _ = std::fs::remove_dir_all(tmp);
            Err(e)
        }
    }
}

fn sweep_stale(worker_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(worker_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("adapters.tmp.") || name.starts_with("adapters.old.") {
            if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                warn!(target: "acp.adapters", path = %entry.path().display(), error = %e, "failed to sweep stale adapter dir");
            }
        }
    }
}

/// Prepend `dir` (if any) to the child's PATH.
fn prepend_dirs_to_path(cmd: &mut std::process::Command, dir: Option<&Path>) {
    let Some(dir) = dir else {
        return;
    };
    let current = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![dir.to_path_buf()];
    dirs.extend(std::env::split_paths(&current));
    if let Ok(joined) = std::env::join_paths(&dirs) {
        cmd.env("PATH", joined);
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in digest {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xF) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::node::ResolvedNode;

    fn resolved(path: PathBuf, source: NodeSource) -> ResolvedNode {
        ResolvedNode {
            path,
            version: "v22.21.0".to_string(),
            source,
        }
    }

    #[test]
    fn adapter_paths_are_under_the_data_dir() {
        let app = Path::new("/data");
        assert_eq!(
            bundled_adapters_dir(app),
            Path::new("/data/acp-worker/adapters")
        );
        assert_eq!(
            bin_dir(app),
            Path::new("/data/acp-worker/adapters/node_modules/.bin")
        );
    }

    #[test]
    fn bundled_adapter_bin_is_none_until_present_then_some() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path();
        assert!(bundled_adapter_bin(app, "claude-agent-acp").is_none());

        let bin = bin_dir(app);
        std::fs::create_dir_all(&bin).unwrap();
        let name = if cfg!(windows) {
            "claude-agent-acp.cmd"
        } else {
            "claude-agent-acp"
        };
        std::fs::write(bin.join(name), b"#!/usr/bin/env node\n").unwrap();
        assert!(bundled_adapter_bin(app, "claude-agent-acp").is_some());
        assert!(bundled_adapter_bin(app, "codex-acp").is_none());
    }

    #[test]
    fn installation_is_current_requires_matching_digest_and_all_bins() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path();
        assert!(!installation_is_current(app));

        let bin = bin_dir(app);
        std::fs::create_dir_all(&bin).unwrap();
        for b in BUNDLED_ADAPTER_BINS {
            let name = if cfg!(windows) {
                format!("{b}.cmd")
            } else {
                (*b).to_string()
            };
            std::fs::write(bin.join(name), b"shim").unwrap();
        }
        // Bins present but no digest sidecar: still not current.
        assert!(!installation_is_current(app));

        let dir = bundled_adapters_dir(app);
        std::fs::write(dir.join(DIGEST_FILE), format!("{}\n", expected_digest())).unwrap();
        assert!(installation_is_current(app));

        // Wrong digest: stale, triggers reinstall.
        std::fs::write(dir.join(DIGEST_FILE), "deadbeef\n").unwrap();
        assert!(!installation_is_current(app));
    }

    #[test]
    fn npm_ci_argv_uses_bundled_npm_cli_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("node-v22.21.0");
        let node_bin = root.join("bin").join("node");
        std::fs::create_dir_all(node_bin.parent().unwrap()).unwrap();
        std::fs::write(&node_bin, b"node").unwrap();
        let npm_cli = root.join("lib/node_modules/npm/bin/npm-cli.js");
        std::fs::create_dir_all(npm_cli.parent().unwrap()).unwrap();
        std::fs::write(&npm_cli, b"npm").unwrap();

        let node = resolved(node_bin.clone(), NodeSource::Bundled);
        let (program, args) = npm_ci_argv(&node).unwrap();
        assert_eq!(program, node_bin);
        assert_eq!(args[0], npm_cli.to_string_lossy());
        assert_eq!(&args[1..], &["ci", "--no-audit", "--no-fund"]);
    }

    #[test]
    fn digest_is_stable() {
        assert_eq!(expected_digest(), expected_digest());
        assert_eq!(expected_digest().len(), 64);
    }
}
