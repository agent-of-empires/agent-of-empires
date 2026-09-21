//! Test-only tmux session guard and pane helpers.

use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) struct TmuxTestSession {
    name: String,
}

impl TmuxTestSession {
    pub(crate) fn new(prefix: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            name: format!("{}_{}_{}", prefix, std::process::id(), n),
        }
    }

    pub(crate) fn from_name(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for TmuxTestSession {
    fn drop(&mut self) {
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

pub(crate) fn only_pane_id(session_name: &str) -> String {
    let id = pane_field(session_name, "#{pane_id}");
    assert!(
        id.starts_with('%'),
        "no pane id for session {session_name}: {id:?}"
    );
    id
}

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

pub(crate) fn tmux_available() -> bool {
    crate::tmux::tmux_command()
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Return early from a test when no tmux binary is usable.
macro_rules! require_tmux {
    () => {
        if !$crate::tmux::test_helpers::tmux_available() {
            eprintln!("Skipping test: tmux not available");
            return;
        }
    };
}
pub(crate) use require_tmux;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn drop_kills_session() {
        require_tmux!();
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

    #[test]
    fn unique_names_within_process() {
        let a = TmuxTestSession::new("aoe_test_unique");
        let b = TmuxTestSession::new("aoe_test_unique");
        assert_ne!(a.name(), b.name());
    }
}
