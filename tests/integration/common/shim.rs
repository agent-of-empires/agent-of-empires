use std::path::{Path, PathBuf};

pub fn shim_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("acp-worker/test-shim/shim.mjs")
}

pub fn shim_ready() -> Result<(), String> {
    shim_node()?;
    let shim = shim_path();
    if !shim.exists() {
        return Err(format!("shim missing at {}", shim.display()));
    }
    if !shim.parent().unwrap().join("node_modules").exists() {
        return Err("shim deps not installed; run cd acp-worker/test-shim && npm ci first".into());
    }
    Ok(())
}

// Resolve version-manager launchers before tests isolate HOME.
pub fn shim_node() -> Result<&'static Path, String> {
    static NODE: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    NODE.get_or_init(|| {
        if std::env::var_os("AOE_ISOLATED_RUNNER_TEST").is_some() {
            if let Some(path) = std::env::var_os("AOE_ISOLATED_NODE_BIN") {
                return std::fs::canonicalize(path)
                    .map_err(|error| format!("cannot resolve isolated Node runtime: {error}"));
            }
        }
        let output = std::process::Command::new("node")
            .args(["--print", "process.execPath"])
            .output()
            .map_err(|error| format!("cannot resolve Node runtime: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "cannot resolve Node runtime: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let path = String::from_utf8(output.stdout)
            .map_err(|error| format!("invalid Node runtime path: {error}"))?;
        std::fs::canonicalize(path.trim())
            .map_err(|error| format!("cannot resolve Node executable: {error}"))
    })
    .as_ref()
    .map(PathBuf::as_path)
    .map_err(Clone::clone)
}
