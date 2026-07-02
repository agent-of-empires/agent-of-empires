//! Test-only helpers for tmux integration tests.
//!
//! `TmuxTestSession` reserves a unique session name and tears the tmux
//! session down on `Drop`, so a panicking `assert!`/`expect!` cannot leak a
//! tmux session into the user's environment. Tests still call
//! `tmux new-session` themselves (they need their own `-x`/`-y`/command and
//! occasionally compound argv), so the guard does not create the session.

use std::sync::atomic::{AtomicU64, Ordering};

/// RAII guard that runs `tmux kill-session -t <name>` on drop. The guard
/// owns the session name; tests call `guard.name()` wherever they need
/// `&str`.
pub(crate) struct TmuxTestSession {
    name: String,
}

impl TmuxTestSession {
    /// Reserve a unique name of the form `<prefix>_<pid>_<n>`. `n` is a
    /// process-local atomic counter so a single test can hold multiple
    /// guards without collision.
    pub(crate) fn new(prefix: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            name: format!("{}_{}_{}", prefix, std::process::id(), n),
        }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for TmuxTestSession {
    fn drop(&mut self) {
        // Best-effort, idempotent. Drop must not panic, so the Result is
        // discarded: a missing tmux server or already-dead session is fine.
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &self.name])
            .output();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmux_available() -> bool {
        crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    #[serial_test::serial]
    fn drop_kills_session() {
        if !tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
        let captured_name;
        {
            let guard = TmuxTestSession::new("aoe_test_guard_self");
            captured_name = guard.name().to_string();
            let output = crate::tmux::tmux_command()
                .args([
                    "new-session",
                    "-d",
                    "-s",
                    guard.name(),
                    "-x",
                    "80",
                    "-y",
                    "24",
                    "sleep 30",
                ])
                .output()
                .expect("tmux new-session");
            assert!(output.status.success());
            let exists = crate::tmux::tmux_command()
                .args(["has-session", "-t", guard.name()])
                .output()
                .expect("tmux has-session")
                .status
                .success();
            assert!(exists, "session should exist while guard is alive");
        }
        let exists = crate::tmux::tmux_command()
            .args(["has-session", "-t", &captured_name])
            .output()
            .expect("tmux has-session")
            .status
            .success();
        assert!(!exists, "session should be killed after guard drop");
    }

    // No `#[serial_test::serial]`: this test only touches the in-process
    // atomic counter, never tmux. Adding the attribute would needlessly
    // serialize it against unrelated tmux-spawning tests.
    #[test]
    fn unique_names_within_process() {
        let a = TmuxTestSession::new("aoe_test_unique");
        let b = TmuxTestSession::new("aoe_test_unique");
        assert_ne!(a.name(), b.name());
    }
}
