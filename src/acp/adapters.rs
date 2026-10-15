use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::{info, warn};

use crate::acp::node::{NodeSource, ResolvedNode};

/// One pinned adapter: the binary npm installs, plus its embedded manifest
/// pair.
pub struct BundledAdapter {
    pub binary: &'static str,
    package_json: &'static [u8],
    package_lock: &'static [u8],
    /// In-tree sources written beside the manifest before `npm ci`, as
    /// `(relative path, bytes)`.
    sources: &'static [(&'static str, &'static [u8])],
    entry: Option<&'static str>,
}

pub const BUNDLED_ADAPTERS: &[BundledAdapter] = &[
    BundledAdapter {
        binary: "claude-agent-acp",
        package_json: include_bytes!("../../acp-worker/adapters/claude-agent-acp/package.json"),
        package_lock: include_bytes!(
            "../../acp-worker/adapters/claude-agent-acp/package-lock.json"
        ),
        sources: &[],
        entry: None,
    },
    BundledAdapter {
        binary: "codex-acp",
        package_json: include_bytes!("../../acp-worker/adapters/codex-acp/package.json"),
        package_lock: include_bytes!("../../acp-worker/adapters/codex-acp/package-lock.json"),
        sources: &[],
        entry: None,
    },
    BundledAdapter {
        binary: "pi-acp",
        package_json: include_bytes!("../../acp-worker/adapters/pi-acp/package.json"),
        package_lock: include_bytes!("../../acp-worker/adapters/pi-acp/package-lock.json"),
        sources: &[],
        entry: None,
    },
    BundledAdapter {
        binary: crate::acp::install_hints::AOE_AGENT_BINARY,
        package_json: include_bytes!("../../acp-worker/aoe-agent/package.json"),
        package_lock: include_bytes!("../../acp-worker/aoe-agent/package-lock.json"),
        sources: &[
            (
                "src/index.ts",
                include_bytes!("../../acp-worker/aoe-agent/src/index.ts"),
            ),
            (
                "src/toolKind.ts",
                include_bytes!("../../acp-worker/aoe-agent/src/toolKind.ts"),
            ),
            (
                "src/transcript.ts",
                include_bytes!("../../acp-worker/aoe-agent/src/transcript.ts"),
            ),
        ],
        entry: Some("src/index.ts"),
    },
];

/// The adapter `doctor --fix` installs when no `--adapter` is given.
pub const DEFAULT_ADAPTER: &str = "claude-agent-acp";

const DIGEST_FILE: &str = ".aoe-lock-digest";

/// Staging and backup dirs older than this are assumed to be crash
/// leftovers and swept.
const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("`{0}` is not a bundled ACP adapter")]
    UnknownAdapter(String),
    #[error("no usable npm found for the resolved Node at {0}")]
    NpmUnavailable(PathBuf),
    #[error("npm ci exited with {0}")]
    NpmFailed(String),
    #[error("adapter binary `{0}` missing after install")]
    BinaryMissing(String),
    #[error("`{adapter}` needs Node {major}.{minor} or newer to run its sources; found {found}")]
    NodeTooOld {
        adapter: String,
        major: u32,
        minor: u32,
        found: String,
    },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub fn lookup(binary: &str) -> Option<&'static BundledAdapter> {
    BUNDLED_ADAPTERS.iter().find(|a| a.binary == binary)
}

pub fn is_bundled(binary: &str) -> bool {
    lookup(binary).is_some()
}

/// `$AOE_DATA_DIR/acp-worker/adapters`, the parent of every adapter prefix.
pub fn bundled_adapters_dir(app_dir: &Path) -> PathBuf {
    app_dir.join("acp-worker").join("adapters")
}

/// This adapter's own npm prefix.
fn adapter_dir(app_dir: &Path, binary: &str) -> PathBuf {
    bundled_adapters_dir(app_dir).join(binary)
}

fn bin_dir(app_dir: &Path, binary: &str) -> PathBuf {
    adapter_dir(app_dir, binary)
        .join("node_modules")
        .join(".bin")
}

/// Absolute path to a bundled adapter binary if it exists on disk, else
/// `None`.
pub fn bundled_adapter_bin(app_dir: &Path, binary: &str) -> Option<PathBuf> {
    let base = bin_dir(app_dir, binary).join(binary);
    let candidate = if cfg!(windows) {
        base.with_extension("cmd")
    } else {
        base
    };
    candidate.is_file().then_some(candidate)
}

/// Digest of everything the install is built from, so a source edit
/// reinstalls the same way a lock bump does.
fn install_digest(adapter: &BundledAdapter) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(adapter.package_lock.len());
    bytes.extend_from_slice(adapter.package_lock);
    for (path, contents) in adapter.sources {
        bytes.extend_from_slice(path.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(contents);
        bytes.push(0);
    }
    sha256_hex(&bytes)
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

/// The Node an in-tree adapter would launch with, when it cannot run the
/// adapter's sources: the version string, for the refusal message.
pub fn runtime_too_old_for(app_dir: &Path, binary: &str) -> Option<String> {
    ships_sources(binary).then_some(())?;
    let node = crate::acp::node::resolve_for("", app_dir, true).ok()?;
    (!crate::acp::node::supports_strip_types(&node.version)).then_some(node.version)
}

/// Whether `binary` is an in-tree adapter that runs from TypeScript sources.
pub fn ships_sources(binary: &str) -> bool {
    lookup(binary).is_some_and(|a| !a.sources.is_empty())
}

pub fn installed_copy_is_stale(app_dir: &Path, binary: &str) -> bool {
    lookup(binary).is_some_and(|adapter| {
        !adapter.sources.is_empty()
            && bundled_adapter_bin(app_dir, binary).is_some()
            && !installation_is_current(app_dir, binary)
    })
}

/// True when a complete, current install of `binary` is present: the digest
/// sidecar matches its embedded lockfile AND the binary exists.
pub fn installation_is_current(app_dir: &Path, binary: &str) -> bool {
    let Some(adapter) = lookup(binary) else {
        return false;
    };
    let expected = install_digest(adapter);
    let digest_ok = std::fs::read_to_string(adapter_dir(app_dir, binary).join(DIGEST_FILE))
        .map(|s| s.trim() == expected)
        .unwrap_or(false);
    digest_ok && bundled_adapter_bin(app_dir, binary).is_some()
}

/// Install (or upgrade) one pinned adapter into the data dir using `node`'s
/// npm.
pub fn install(app_dir: &Path, node: &ResolvedNode, binary: &str) -> Result<(), AdapterError> {
    static INSTALLING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serialized = INSTALLING.lock().unwrap_or_else(|p| p.into_inner());
    let adapter = lookup(binary).ok_or_else(|| AdapterError::UnknownAdapter(binary.to_string()))?;
    if installation_is_current(app_dir, binary) {
        info!(target: "acp.adapters", adapter = binary, "already current; nothing to install");
        return Ok(());
    }
    if !adapter.sources.is_empty() && !crate::acp::node::supports_strip_types(&node.version) {
        return Err(AdapterError::NodeTooOld {
            adapter: binary.to_string(),
            major: crate::acp::node::MIN_NODE_MAJOR,
            minor: crate::acp::node::MIN_NODE_MINOR,
            found: node.version.clone(),
        });
    }

    let parent = bundled_adapters_dir(app_dir);
    std::fs::create_dir_all(&parent)?;
    sweep_stale(&parent);

    // Build in a sibling temp dir, then publish by rename so readers never
    // see a half-built node_modules.
    let tmp = parent.join(format!("{binary}.tmp.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;

    std::fs::write(tmp.join("package.json"), adapter.package_json)?;
    std::fs::write(tmp.join("package-lock.json"), adapter.package_lock)?;
    for (path, contents) in adapter.sources {
        let target = tmp.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(target, contents)?;
    }

    let (program, args) =
        npm_ci_argv(node).ok_or_else(|| AdapterError::NpmUnavailable(node.path.clone()))?;
    info!(
        target: "acp.adapters",
        adapter = binary,
        program = %program.display(),
        "installing bundled ACP adapter via npm ci"
    );
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&args).current_dir(&tmp);
    // The npm CLI itself is `#!/usr/bin/env node`; make sure the resolved
    // node is reachable when it is not already on PATH.
    prepend_dir_to_path(&mut cmd, node.path.parent());
    let status = cmd.status().inspect_err(|_| {
        let _ = std::fs::remove_dir_all(&tmp);
    })?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(AdapterError::NpmFailed(status.to_string()));
    }

    if let Some(entry) = adapter.entry {
        write_entry_wrapper(&tmp, binary, entry)?;
    }
    let produced = tmp.join("node_modules").join(".bin").join(binary);
    let produced = if cfg!(windows) {
        produced.with_extension("cmd")
    } else {
        produced
    };
    if !produced.is_file() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(AdapterError::BinaryMissing(binary.to_string()));
    }

    // Completion marker, written last: an interrupted install never leaves
    // a matching digest behind.
    std::fs::write(
        tmp.join(DIGEST_FILE),
        format!("{}\n", install_digest(adapter)),
    )?;

    publish(&tmp, &adapter_dir(app_dir, binary))?;
    info!(target: "acp.adapters", adapter = binary, "installed");
    Ok(())
}

/// The launcher for an in-tree agent: `node --experimental-strip-types`
/// on the entry script, with `node` taken from `PATH`, which the spawn
/// prepends the resolved Node's directory to (`bundled_resolution`).
fn write_entry_wrapper(install_dir: &Path, binary: &str, entry: &str) -> std::io::Result<()> {
    let bin = install_dir.join("node_modules").join(".bin");
    std::fs::create_dir_all(&bin)?;
    if cfg!(windows) {
        let script = format!(
            "@echo off\r\nnode --experimental-strip-types \"%~dp0..\\..\\{}\" %*\r\n",
            entry.replace('/', "\\")
        );
        std::fs::write(bin.join(binary).with_extension("cmd"), script)?;
        return Ok(());
    }
    let path = bin.join(binary);
    let script = format!(
        "#!/bin/sh\nexec node --experimental-strip-types \"$(dirname \"$0\")/../../{entry}\" \"$@\"\n"
    );
    std::fs::write(&path, script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Build the argv for `npm ci`, run from the target dir.
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

/// Move a completed staging dir into place.
fn publish(tmp: &Path, final_dir: &Path) -> std::io::Result<()> {
    if !final_dir.exists() {
        return std::fs::rename(tmp, final_dir);
    }
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

/// Remove crash leftovers.
fn sweep_stale(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.contains(".tmp.") && !name.contains(".old.") {
            continue;
        }
        let old_enough = entry
            .metadata()
            .and_then(|m| m.modified())
            .and_then(|t| t.elapsed().map_err(std::io::Error::other))
            .map(|age| age > STALE_AFTER)
            .unwrap_or(false);
        if !old_enough {
            continue;
        }
        if let Err(e) = std::fs::remove_dir_all(entry.path()) {
            warn!(target: "acp.adapters", path = %entry.path().display(), error = %e, "failed to sweep stale adapter dir");
        }
    }
}

/// Prepend `dir` (if any) to the child's PATH.
fn prepend_dir_to_path(cmd: &mut std::process::Command, dir: Option<&Path>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn touch_bin(app_dir: &Path, binary: &str) {
        let bin = bin_dir(app_dir, binary);
        std::fs::create_dir_all(&bin).unwrap();
        let name = if cfg!(windows) {
            format!("{binary}.cmd")
        } else {
            binary.to_string()
        };
        std::fs::write(bin.join(name), b"#!/usr/bin/env node\n").unwrap();
    }

    fn write_digest(app_dir: &Path, binary: &str) {
        let adapter = lookup(binary).unwrap();
        std::fs::write(
            adapter_dir(app_dir, binary).join(DIGEST_FILE),
            format!("{}\n", install_digest(adapter)),
        )
        .unwrap();
    }

    #[test]
    fn installation_is_current_requires_matching_digest_and_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path();
        assert!(!installation_is_current(app, "claude-agent-acp"));

        touch_bin(app, "claude-agent-acp");
        // Binary present but no digest sidecar: incomplete, not current.
        assert!(!installation_is_current(app, "claude-agent-acp"));

        write_digest(app, "claude-agent-acp");
        assert!(installation_is_current(app, "claude-agent-acp"));
        // Installing one adapter must not make a sibling look installed.
        assert!(bundled_adapter_bin(app, "codex-acp").is_none());

        // A stale digest (an aoe upgrade bumped the pin) forces reinstall.
        std::fs::write(
            adapter_dir(app, "claude-agent-acp").join(DIGEST_FILE),
            "deadbeef\n",
        )
        .unwrap();
        assert!(!installation_is_current(app, "claude-agent-acp"));
        // An unknown adapter is never "current".
        assert!(!installation_is_current(app, "not-an-adapter"));
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

        let node = ResolvedNode {
            path: node_bin.clone(),
            version: "v22.21.0".to_string(),
            source: NodeSource::Bundled,
        };
        let (program, args) = npm_ci_argv(&node).unwrap();
        assert_eq!(program, node_bin);
        assert_eq!(args[0], npm_cli.to_string_lossy());
        assert_eq!(&args[1..], &["ci", "--no-audit", "--no-fund"]);
    }

    #[test]
    fn publish_and_sweep_keep_the_live_install() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Fresh: rename into a path that does not exist yet.
        let staging = root.join("a.tmp.1");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("marker"), b"new").unwrap();
        let final_dir = root.join("a");
        publish(&staging, &final_dir).unwrap();
        assert_eq!(std::fs::read(final_dir.join("marker")).unwrap(), b"new");
        assert!(!staging.exists());

        let staging2 = root.join("a.tmp.2");
        std::fs::create_dir_all(&staging2).unwrap();
        std::fs::write(staging2.join("marker"), b"newer").unwrap();
        publish(&staging2, &final_dir).unwrap();
        assert_eq!(std::fs::read(final_dir.join("marker")).unwrap(), b"newer");
        assert!(!staging2.exists());
        // No backup dir left behind.
        assert!(!std::fs::read_dir(root)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".old.")));

        let missing = root.join("a.tmp.absent");
        assert!(publish(&missing, &final_dir).is_err());
        assert_eq!(std::fs::read(final_dir.join("marker")).unwrap(), b"newer");

        // The stale sweep spares fresh dirs and the real install.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let fresh_tmp = root.join("claude-agent-acp.tmp.999");
        let fresh_old = root.join("claude-agent-acp.old.999");
        let installed = root.join("claude-agent-acp");
        for d in [&fresh_tmp, &fresh_old, &installed] {
            std::fs::create_dir_all(d).unwrap();
        }

        sweep_stale(root);

        assert!(fresh_tmp.exists(), "a concurrent install's staging dir");
        assert!(fresh_old.exists(), "a concurrent install's backup dir");
        assert!(installed.exists(), "the real install must never be swept");
    }

    #[test]
    fn claude_pin_satisfies_startup_floor() {
        let manifest: serde_json::Value =
            serde_json::from_slice(lookup("claude-agent-acp").unwrap().package_json).unwrap();
        let pinned = manifest["dependencies"]["@agentclientprotocol/claude-agent-acp"]
            .as_str()
            .expect("pinned claude-agent-acp version");
        let pinned = semver::Version::parse(pinned).expect("pin must be exact semver");
        let floor =
            semver::Version::parse(crate::acp::agent_compat::CLAUDE_AGENT_ACP_MIN_VERSION).unwrap();
        assert!(
            pinned >= floor,
            "pinned claude-agent-acp {pinned} is below the startup floor {floor}; \
             bump acp-worker/adapters/claude-agent-acp/package.json"
        );
    }
}
