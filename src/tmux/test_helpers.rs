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

static DRAFT_WATCHERS: std::sync::Mutex<Vec<(String, std::sync::mpsc::Sender<()>)>> =
    std::sync::Mutex::new(Vec::new());

/// Signals each time an input-line wait reads a draft in `pane`.
pub(crate) fn watch_drafts(pane: &str) -> std::sync::mpsc::Receiver<()> {
    let (tx, rx) = std::sync::mpsc::channel();
    DRAFT_WATCHERS.lock().unwrap().push((pane.to_string(), tx));
    rx
}

pub(crate) fn observe_draft(pane: &str) {
    for (watched, tx) in DRAFT_WATCHERS.lock().unwrap().iter() {
        if watched == pane {
            let _ = tx.send(());
        }
    }
}

/// Splits `session`'s first window, running `script` in the new pane, and returns its `#{pane_id}`.
pub(crate) fn split_pane(session: &str, script: &str) -> String {
    let output = crate::tmux::tmux_command()
        .args([
            "split-window",
            "-d",
            "-t",
            &format!("={session}:"),
            "-P",
            "-F",
            "#{pane_id}",
            "sh",
            "-c",
            script,
        ])
        .output()
        .expect("tmux split-window");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf8")
        .trim()
        .to_string()
}

type InputCheckHook = Box<dyn FnOnce() + Send>;
static INPUT_CHECK_HOOKS: std::sync::Mutex<Vec<(String, InputCheckHook)>> =
    std::sync::Mutex::new(Vec::new());

/// Runs `hook` once, between a send's input-line check of `pane` and its keys.
pub(crate) fn on_input_checked(pane: &str, hook: impl FnOnce() + Send + 'static) {
    INPUT_CHECK_HOOKS
        .lock()
        .unwrap()
        .push((pane.to_string(), Box::new(hook)));
}

pub(crate) fn after_input_check(pane: &str) {
    let hooks: Vec<InputCheckHook> = {
        let mut all = INPUT_CHECK_HOOKS.lock().unwrap();
        let (mine, rest) = std::mem::take(&mut *all)
            .into_iter()
            .partition::<Vec<_>, _>(|(watched, _)| watched == pane);
        *all = rest;
        mine.into_iter().map(|(_, hook)| hook).collect()
    };
    for hook in hooks {
        hook();
    }
}

static TEXT_TYPED_HOOKS: std::sync::Mutex<Vec<(String, InputCheckHook)>> =
    std::sync::Mutex::new(Vec::new());

/// Runs `hook` once, after a checked-pane send has typed its text into `pane` and before its
/// membership check for Enter.
pub(crate) fn on_text_typed(pane: &str, hook: impl FnOnce() + Send + 'static) {
    TEXT_TYPED_HOOKS
        .lock()
        .unwrap()
        .push((pane.to_string(), Box::new(hook)));
}

pub(crate) fn after_text_typed(pane: &str) {
    let hooks: Vec<InputCheckHook> = {
        let mut all = TEXT_TYPED_HOOKS.lock().unwrap();
        let (mine, rest) = std::mem::take(&mut *all)
            .into_iter()
            .partition::<Vec<_>, _>(|(watched, _)| watched == pane);
        *all = rest;
        mine.into_iter().map(|(_, hook)| hook).collect()
    };
    for hook in hooks {
        hook();
    }
}
