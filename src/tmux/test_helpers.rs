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

    /// Guard an exact name derived by production naming code. The caller is
    /// still responsible for choosing a unique name and creating the session.
    pub(crate) fn from_name(name: impl Into<String>) -> Self {
        Self { name: name.into() }
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

pub(crate) fn pane_field(target: &str, format: &str) -> String {
    let output = crate::tmux::tmux_command()
        .args(["display-message", "-t", target, "-p", format])
        .output()
        .expect("tmux display-message");
    assert!(
        output.status.success(),
        "tmux probe for {target} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_string()
}

/// Capture the sole pane's identity before adding windows or splits.
pub(crate) fn only_pane_id(session_name: &str) -> String {
    let id = pane_field(session_name, "#{pane_id}");
    assert!(
        id.starts_with('%'),
        "no pane id for session {session_name}: {id:?}"
    );
    id
}

/// Observe exec on the raw pane ID, independently of the API's target resolution.
pub(crate) fn wait_for_pane_command(pane_id: &str, expected: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let current = pane_field(pane_id, "#{pane_current_command}");
        if current == expected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pane {pane_id} still reports {current:?}, expected {expected:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

pub(crate) fn wait_for_pane_dead(pane_id: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let dead = pane_field(pane_id, "#{pane_dead}");
        if dead == "1" {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pane {pane_id} did not exit: pane_dead={dead:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
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
