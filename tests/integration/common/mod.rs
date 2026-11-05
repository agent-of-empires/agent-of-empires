//! Shared helpers for integration tests, declared from `main.rs`.

#[cfg(debug_assertions)]
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
#[cfg(debug_assertions)]
use std::time::{Duration, Instant};
use tempfile::TempDir;

mod node;
pub use node::*;

/// Sets the process-stable cached tmux socket; callers must be `#[serial]`.
pub fn tmux_socket() -> PathBuf {
    let path =
        std::env::temp_dir().join(format!("aoe-integration-tmux-{}.sock", std::process::id()));
    std::env::set_var("AOE_TMUX_SOCKET", &path);
    path
}
/// Point `HOME` (and `XDG_CONFIG_HOME`) at a fresh temp dir; drop the guard to
/// restore. `set_var` is not thread-safe, so callers must be `#[serial]`.
pub fn setup_temp_home() -> TestHome {
    let temp = TempDir::new().unwrap();
    let env = set_temp_home(temp.path());
    TestHome { env, temp }
}

/// Restore environment before the caller drops its temporary directory.
pub fn set_temp_home(path: &Path) -> EnvGuard {
    let mut env = EnvGuard::new(&["HOME", "XDG_CONFIG_HOME", "AOE_TMUX_SOCKET"]);
    let _ = tmux_socket();
    env.set("HOME", path);
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    env.set("XDG_CONFIG_HOME", path.join(".config"));
    env
}

pub struct CwdGuard(PathBuf);

impl CwdGuard {
    pub fn set(path: &Path) -> Self {
        let guard = Self(std::env::current_dir().expect("original cwd"));
        std::env::set_current_dir(path).expect("test cwd");
        guard
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).expect("restore cwd");
    }
}
#[must_use]
pub struct TestHome {
    pub env: EnvGuard,
    temp: TempDir,
}

impl TestHome {
    pub fn path(&self) -> &Path {
        self.temp.path()
    }
}

impl EnvGuard {
    pub fn and_set(mut self, key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        self.set(key, value);
        self
    }
}

#[cfg(debug_assertions)]
/// Bind ephemeral, drop, return the port. The TOCTOU window before the caller
/// binds is acceptable under `#[serial]`.
pub fn pick_free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().expect("local_addr").port()
}

#[cfg(debug_assertions)]
/// Poll-connect `127.0.0.1:port` until it succeeds or `deadline` elapses.
pub fn wait_for_port(port: u16, deadline: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", port).parse().unwrap(),
            Duration::from_millis(200),
        )
        .is_ok()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}
