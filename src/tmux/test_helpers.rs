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
                if std::path::Path::new(&argv0).file_name() == Some(std::ffi::OsStr::new(expected))
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

static DRAFT_WATCHERS: std::sync::Mutex<Vec<(String, std::sync::mpsc::Sender<()>)>> =
    std::sync::Mutex::new(Vec::new());

/// Signals the first time each input-line wait reads a draft in `pane`.
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
    for hook in take_hooks(&DRAFT_SEEN_HOOKS, pane) {
        hook();
    }
}

static DRAFT_SEEN_HOOKS: HookRegistry = std::sync::Mutex::new(Vec::new());

/// Runs `hook` once, on the waiting thread, the first time an input-line wait reads a draft in
/// `pane`, before it sleeps.
pub(crate) fn on_draft_seen(pane: &str, hook: impl FnOnce() + Send + 'static) {
    DRAFT_SEEN_HOOKS
        .lock()
        .unwrap()
        .push((pane.to_string(), Box::new(hook)));
}

/// Claude's input box drawn by hand: `cat` on a cooked tty echoes typed keys after the `❯`,
/// between two rules, above the footer that holds Claude's ready marker.
pub(crate) const INPUT_BOX_SCRIPT: &str =
    "printf '%s\\n\\n%s\\n%s\\n' ──────── ──────── '? for shortcuts'; \
     printf '\\033[3A❯ '; exec cat";

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
type HookRegistry = std::sync::Mutex<Vec<(String, InputCheckHook)>>;
static INPUT_CHECK_HOOKS: HookRegistry = std::sync::Mutex::new(Vec::new());

/// Removes and returns the hooks registered for `pane`.
fn take_hooks(registry: &HookRegistry, pane: &str) -> Vec<InputCheckHook> {
    let mut all = registry.lock().unwrap();
    let (mine, rest) = std::mem::take(&mut *all)
        .into_iter()
        .partition::<Vec<_>, _>(|(watched, _)| watched == pane);
    *all = rest;
    mine.into_iter().map(|(_, hook)| hook).collect()
}

/// Runs `hook` once, between a send's input-line check of `pane` and its keys.
pub(crate) fn on_input_checked(pane: &str, hook: impl FnOnce() + Send + 'static) {
    INPUT_CHECK_HOOKS
        .lock()
        .unwrap()
        .push((pane.to_string(), Box::new(hook)));
}

pub(crate) fn after_input_check(pane: &str) {
    for hook in take_hooks(&INPUT_CHECK_HOOKS, pane) {
        hook();
    }
}

static INPUT_CAPTURED_HOOKS: HookRegistry = std::sync::Mutex::new(Vec::new());

/// Runs `hook` once, after an input-line read has captured `pane` and before it classifies it.
pub(crate) fn on_input_captured(pane: &str, hook: impl FnOnce() + Send + 'static) {
    INPUT_CAPTURED_HOOKS
        .lock()
        .unwrap()
        .push((pane.to_string(), Box::new(hook)));
}

pub(crate) fn after_input_captured(pane: &str) {
    for hook in take_hooks(&INPUT_CAPTURED_HOOKS, pane) {
        hook();
    }
}

static INPUT_WAIT_DEADLINES: std::sync::Mutex<Vec<(String, std::time::Instant)>> =
    std::sync::Mutex::new(Vec::new());

pub(crate) fn record_input_wait_deadline(pane: &str, deadline: std::time::Instant) {
    let mut all = INPUT_WAIT_DEADLINES.lock().unwrap();
    all.retain(|(watched, _)| watched != pane);
    all.push((pane.to_string(), deadline));
}

/// The deadline of the latest input-line wait on `pane`.
pub(crate) fn input_wait_deadline(pane: &str) -> Option<std::time::Instant> {
    INPUT_WAIT_DEADLINES
        .lock()
        .unwrap()
        .iter()
        .find(|(watched, _)| watched == pane)
        .map(|(_, deadline)| *deadline)
}

static SCREEN_CAPTURED_HOOKS: HookRegistry = std::sync::Mutex::new(Vec::new());

/// Runs `hook` once, after an automatic-input check has captured `pane` and before it reads
/// the capture.
pub(crate) fn on_screen_captured(pane: &str, hook: impl FnOnce() + Send + 'static) {
    SCREEN_CAPTURED_HOOKS
        .lock()
        .unwrap()
        .push((pane.to_string(), Box::new(hook)));
}

pub(crate) fn after_screen_captured(pane: &str) {
    for hook in take_hooks(&SCREEN_CAPTURED_HOOKS, pane) {
        hook();
    }
}

static FAILED_SCREEN_CAPTURES: std::sync::Mutex<Vec<(String, usize)>> =
    std::sync::Mutex::new(Vec::new());

/// Makes the next `count` readiness captures of `pane` fail.
pub(crate) fn fail_screen_captures(pane: &str, count: usize) {
    FAILED_SCREEN_CAPTURES
        .lock()
        .unwrap()
        .push((pane.to_string(), count));
}

/// Whether this readiness capture of `pane` is one [`fail_screen_captures`] asked to fail.
pub(crate) fn screen_capture_fails(pane: &str) -> bool {
    let mut failing = FAILED_SCREEN_CAPTURES.lock().unwrap();
    let Some(entry) = failing.iter_mut().find(|(p, n)| p == pane && *n > 0) else {
        return false;
    };
    entry.1 -= 1;
    true
}

static TEXT_TYPED_HOOKS: HookRegistry = std::sync::Mutex::new(Vec::new());

/// Runs `hook` once, after a checked-pane send has typed its text into `pane` and before its
/// membership check for Enter.
pub(crate) fn on_text_typed(pane: &str, hook: impl FnOnce() + Send + 'static) {
    TEXT_TYPED_HOOKS
        .lock()
        .unwrap()
        .push((pane.to_string(), Box::new(hook)));
}

pub(crate) fn after_text_typed(pane: &str) {
    for hook in take_hooks(&TEXT_TYPED_HOOKS, pane) {
        hook();
    }
}
