//! Isolated environment primitives shared by test hosts.

use std::path::Path;

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
