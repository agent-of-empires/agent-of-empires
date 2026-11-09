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

/// Wait for the real agent beneath the protected-environment pane shell.
/// A pane's command can remain `sh` while its launched child has already exec'd.
pub(crate) fn wait_for_pane_agent_command(
    pane_id: &str,
    agent_pid_file: &std::path::Path,
    expected: &str,
) -> u32 {
    let pane_pid = pane_field(pane_id, "#{pane_pid}")
        .parse::<u32>()
        .expect("pane PID");
    let pane_birth = crate::process::process_birth_identity(pane_pid)
        .unwrap()
        .expect("live pane birth before agent readiness");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(pid) = std::fs::read_to_string(agent_pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
        {
            if let Some((_, argv0)) = crate::process::parent_and_argv0(pid) {
                if std::path::Path::new(&argv0).file_name()
                    == Some(std::ffi::OsStr::new(expected))
                {
                    let agent_birth = crate::process::process_birth_identity(pid)
                        .unwrap()
                        .expect("agent command has a real process birth");
                    let mut ancestor = pid;
                    while ancestor != pane_pid && ancestor > 1 {
                        ancestor = crate::process::parent_and_argv0(ancestor)
                            .expect("read actual agent ancestry")
                            .0;
                    }
                    assert_eq!(ancestor, pane_pid, "agent must belong to this exact pane");
                    assert!(crate::process::process_birth_is_live(pid, &agent_birth).unwrap());
                    assert!(crate::process::process_birth_is_live(pane_pid, &pane_birth).unwrap());
                    assert_eq!(pane_field(pane_id, "#{pane_pid}"), pane_pid.to_string());
                    assert_eq!(pane_field(pane_id, "#{pane_dead}"), "0");
                    return pid;
                }
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pane {pane_id} never launched a real {expected:?} agent from {}",
            agent_pid_file.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
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
