//! Isolated environment primitives shared by test hosts.

use std::path::{Path, PathBuf};

/// Keeps a spawned or in-process aoe off the developer's systemd user manager.
pub const SYSTEMD_OFF: &str = "[systemd]\nenabled = false\n";

/// Appends [`SYSTEMD_OFF`] to the `config.toml` in `app_dir`.
pub fn disable_systemd_scopes(app_dir: &Path) {
    use std::io::Write;
    std::fs::create_dir_all(app_dir).expect("create app dir");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(app_dir.join("config.toml"))
        .expect("open config.toml");
    writeln!(file, "\n{SYSTEMD_OFF}").expect("write config.toml");
}

#[must_use]
pub struct EnvGuard {
    vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    pub fn new(keys: &[&'static str]) -> Self {
        Self {
            vars: keys
                .iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect(),
        }
    }

    pub fn set(&mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) {
        if !self.vars.iter().any(|(saved, _)| *saved == key) {
            self.vars.push((key, std::env::var_os(key)));
        }
        std::env::set_var(key, value);
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, old) in self.vars.drain(..).rev() {
            match old {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}
/// Node shim used by the managed consumer suites.
pub fn shim_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("acp-worker")
        .join("test-shim")
        .join("shim.mjs")
}

/// Reports missing Node/shim dependencies before isolated setup.
pub fn shim_ready() -> Result<(), String> {
    shim_node()?;
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

/// Resolve the runtime behind version-manager launchers before tests isolate
/// HOME: probe and spawn must use the same executable.
pub fn shim_node() -> Result<&'static Path, String> {
    static NODE: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    NODE.get_or_init(|| {
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
