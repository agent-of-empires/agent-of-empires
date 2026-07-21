//! Shared helpers for integration tests.
//!
//! Declared once from `tests/integration/main.rs`; consumers import via
//! `use crate::common::...`.

use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Stable per-process tmux socket for integration tests. aoe now resolves an
/// explicit `-S <socket>` (#2608) and caches it once per process, so tests
/// must (a) point aoe at a hermetic socket via `AOE_TMUX_SOCKET` and (b) make
/// their own raw `tmux` calls target the same socket. The path is stable (not
/// per-test-home) precisely because the lib caches it once; a per-home path
/// would be dropped out from under a later test. Referencing it also sets
/// `AOE_TMUX_SOCKET`, so any raw-tmux call site locks the lib onto the same
/// socket before its first lib tmux call. `#[serial]` tests keep the env write
/// single-threaded.
///
/// The name carries this process's pid so it is stable within one integration
/// binary yet never collides with a concurrent integration process (a second
/// `cargo test` run or a leftover server from a prior run). Without the pid,
/// two processes would share one tmux server and interfere, most visibly as
/// root where `/tmp` is shared across every same-uid run.
pub fn tmux_socket() -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("aoe-integration-tmux-{}.sock", std::process::id()));
    std::env::set_var("AOE_TMUX_SOCKET", &path);
    path
}

/// Path to the Node ACP test shim used by acp_* integration tests.
///
/// Gated on `feature = "serve"` because its only consumers are the
/// structured view modules, which themselves only compile under that feature.
/// Without the gate, `cargo clippy --all-targets` builds the integration
/// suite WITHOUT serve and these helpers register as dead code.
#[cfg(feature = "serve")]
pub fn shim_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("acp-worker")
        .join("test-shim")
        .join("shim.mjs")
}

/// Returns `Ok(())` if the structured view shim can be spawned (node on PATH, shim
/// file present, shim deps installed). Otherwise returns a short reason
/// that callers print before skipping. CI installs deps via `npm ci` in
/// `acp-worker/test-shim/` before running the integration leg; local
/// runs need the same one-shot setup, which the message points at.
#[cfg(feature = "serve")]
pub fn shim_ready() -> Result<(), String> {
    let node_ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !node_ok {
        return Err("node not on PATH".into());
    }
    let shim = shim_path();
    if !shim.exists() {
        return Err(format!("shim missing at {}", shim.display()));
    }
    let node_modules = shim.parent().unwrap().join("node_modules");
    if !node_modules.exists() {
        return Err(
            "shim deps not installed; run `cd acp-worker/test-shim && npm ci` first".into(),
        );
    }
    Ok(())
}

/// True when the effective uid is 0. Root bypasses the Unix DAC permission
/// bits, so a test that injects a write failure by making a dir read-only
/// cannot make the write fail and must skip rather than assert `is_err()`.
#[cfg(unix)]
pub fn running_as_root() -> bool {
    nix::unistd::geteuid().is_root()
}

/// Set `HOME` (and `XDG_CONFIG_HOME` on Linux/macOS) to a fresh temp dir so
/// tests read and write to isolated state. Returns the guard; drop it to clean
/// up.
///
/// # Safety caveat
/// `set_var` is not thread-safe. Callers must be `#[serial]`.
pub fn setup_temp_home() -> TempDir {
    let temp = TempDir::new().unwrap();
    set_temp_home(temp.path());
    temp
}

/// Variant for tests that already own a `TempDir` (e.g. ones that also seed
/// files under the same path before returning the guard).
pub fn set_temp_home(path: &Path) {
    // Establish the hermetic tmux socket before any lib tmux call so aoe's
    // once-cached socket resolution locks onto it (#2608).
    let _ = tmux_socket();
    std::env::set_var("HOME", path);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    std::env::set_var("XDG_CONFIG_HOME", path.join(".config"));
}
