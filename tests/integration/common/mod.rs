//! Shared helpers for integration tests.
//!
//! Declared once from `tests/integration/main.rs`; consumers import via
//! `use crate::common::...`.

use std::path::Path;
use tempfile::TempDir;

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
    std::env::set_var("HOME", path);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    std::env::set_var("XDG_CONFIG_HOME", path.join(".config"));
}
