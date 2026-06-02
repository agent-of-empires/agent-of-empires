//! Live-send mode: a "feels-attached" alternative to the compose dialog.
//!
//! When a user presses `Tab` on a runnable session, the home view installs
//! a `LiveSendState` and routes every subsequent key event through this
//! module's translator. Each translation produces a `tmux send-keys` call
//! against the target pane: plain characters go literally, every other
//! key (arrows, Esc, Tab, modifier combos) goes by tmux key name with
//! `C-` / `M-` prefixes. The user exits with one of the configured
//! exit chords (default: `Ctrl+q`).
//!
//! Exit chord configuration: the user picks a comma-separated list
//! of chord specs (`C-q`, `M-x`, `F12`, …) via settings. Default is
//! `C-q` alone: mobile-friendly, passes through Termius and other
//! restrictive SSH clients, and leaves every other chord available
//! to pass through to the agent. Whichever chord in the configured
//! list fires first ends live mode. The cost of binding a chord is
//! that it can't be sent through to the agent; users who need `C-q`
//! itself to reach the agent configure a different exit.
//!
//! Trade-offs vs. a compose dialog:
//! - No echo, no inline editing, no review step. The preview pane is the
//!   only feedback channel; users who need multi-line composition or want
//!   to proofread voice/dictation should use the compose dialog on `M`.
//! - Each coalesced keystroke run becomes one `tmux send-keys`
//!   subprocess. A long-lived `tmux -C` control-mode connection was
//!   tried (#1485) to avoid that fork cost on mobile, but the
//!   connection turned out to be unreliable on macOS tmux 3.x: it
//!   EOF'd within milliseconds of spawn, leaving us paying the spawn
//!   cost while never benefiting from the connection. Forking per
//!   batch is the simpler, more portable model; the per-batch fork
//!   cost is bounded by user typing speed (held keys / pastes
//!   coalesce into one fork) and is invisible on a laptop.
//!
//! Reserved (non-forwarded) chords:
//! - The configured exit chord list — exits live mode (see above).
//! - `Shift+PageUp` / `Shift+PageDown` — scroll the preview pane back
//!   through agent history without exiting. Matches the terminal-
//!   emulator convention. Bare `PageUp` / `PageDown` still passes
//!   through, so agents that page their own UI keep working.
//! - Mouse wheel over the preview pane — also scrolls the preview,
//!   handled by `handle_scroll_up` / `handle_scroll_down`.

use std::sync::mpsc::{channel, Sender};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Default exit chord set when the user hasn't configured one.
/// `Ctrl+q` is the sole default: works on mobile / restrictive SSH
/// clients (Termius), reachable on every keyboard layout we ship to,
/// and well-known as a "quit" chord. Users who need `C-q` to reach
/// the agent (vim quoted-insert, etc.) can configure a different
/// exit chord, or any comma-separated list, via settings.
///
/// `Ctrl+]` (briefly shipped in 1.9.0) and `Ctrl+\` (tried during
/// development) each silently failed on at least one common
/// terminal/keyboard combination on macOS. Rather than trap users
/// with a chord that looks like it should work and doesn't, the
/// default is one chord; users who want a two-hand exit configure
/// one. 1.9.0 users who saved settings while that release's default
/// was in effect have `"C-q,C-]"` baked into config.toml; re-saving
/// settings (or hand-editing the line out) restores the new default.
pub(super) const DEFAULT_EXIT_CHORD: &str = "C-q";

/// Default live-send leader (prefix) chord. `Ctrl+b` matches the tmux
/// and herdr leader, so multiplexer users already have the muscle
/// memory, and it's the one chord we steal from the agent (double-tap
/// `C-b C-b` still delivers a literal `C-b` downstream). Kept in sync
/// with `default_live_send_leader()` in `session::config`. An empty
/// configured value disables the leader entirely.
pub(super) const DEFAULT_LEADER: &str = "C-b";

/// Parse a tmux-style chord spec into a `(KeyCode, KeyModifiers)`
/// pair. Accepts `C-` / `Ctrl-`, `M-` / `Alt-`, `S-` / `Shift-`
/// prefixes (any order, separated by `-` or `+`) followed by a key
/// name. Key names: single ASCII chars (`q`, `]`, `1`), or one of the
/// tmux-named keys (`Escape`, `Tab`, `BTab`, `Up`, `Down`, `Left`,
/// `Right`, `Enter`, `BSpace` / `Backspace`, `DC` / `Delete`, `IC` /
/// `Insert`, `Home`, `End`, `PPage` / `PageUp`, `NPage` / `PageDown`,
/// `Space`, `F1`..`F12`).
///
/// Returns `None` on parse failure so the caller can fall back to the
/// default chord and warn the user. Chord case is normalized: char
/// keys lowercase under any modifier so `C-q` and `C-Q` parse to the
/// same canonical form.
pub(super) fn parse_chord(spec: &str) -> Option<(KeyCode, KeyModifiers)> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }
    // Split on `-` or `+`; both are common in user-facing key specs.
    let parts: Vec<&str> = trimmed.split(['-', '+']).collect();
    if parts.is_empty() {
        return None;
    }
    let mut modifiers = KeyModifiers::NONE;
    for piece in &parts[..parts.len().saturating_sub(1)] {
        match piece.to_ascii_lowercase().as_str() {
            "c" | "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
            "m" | "alt" | "meta" => modifiers |= KeyModifiers::ALT,
            "s" | "shift" => modifiers |= KeyModifiers::SHIFT,
            _ => return None,
        }
    }
    let key = *parts.last()?;
    let code = parse_key_name(key, modifiers.contains(KeyModifiers::CONTROL))?;
    Some((code, modifiers))
}

fn parse_key_name(name: &str, has_ctrl: bool) -> Option<KeyCode> {
    if name.is_empty() {
        return None;
    }
    // Function keys: "F1".."F12".
    if let Some(rest) = name.strip_prefix(['F', 'f']) {
        if let Ok(n) = rest.parse::<u8>() {
            if (1..=24).contains(&n) {
                return Some(KeyCode::F(n));
            }
        }
    }
    let lower = name.to_ascii_lowercase();
    let code = match lower.as_str() {
        "escape" | "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "btab" | "backtab" => KeyCode::BackTab,
        "enter" | "return" => KeyCode::Enter,
        "bspace" | "backspace" => KeyCode::Backspace,
        "dc" | "delete" | "del" => KeyCode::Delete,
        "ic" | "insert" | "ins" => KeyCode::Insert,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "ppage" | "pageup" | "pgup" => KeyCode::PageUp,
        "npage" | "pagedown" | "pgdn" => KeyCode::PageDown,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "space" => KeyCode::Char(' '),
        _ => {
            // Single-char key: drop case sensitivity when a modifier
            // is held (tmux conventionally treats Ctrl+a and Ctrl+A
            // as the same chord). Without modifiers, preserve case so
            // a config of "Q" really means uppercase Q.
            let mut chars = name.chars();
            let first = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            if has_ctrl {
                return Some(KeyCode::Char(first.to_ascii_lowercase()));
            }
            return Some(KeyCode::Char(first));
        }
    };
    Some(code)
}

/// True when `event` is the configured exit chord. Char codes
/// normalize under Ctrl (matches the canonical form `parse_chord`
/// produces). Strict modifier match otherwise: a chord configured as
/// `C-q` does not fire on `Ctrl+Shift+q` so the user can still
/// deliver `C-q` to the agent via Shift.
pub(super) fn chord_matches(spec: (KeyCode, KeyModifiers), event: KeyEvent) -> bool {
    let mut event_code = event.code;
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = event_code {
            event_code = KeyCode::Char(c.to_ascii_lowercase());
        }
    }
    spec.0 == event_code && spec.1 == event.modifiers
}

/// Parse a comma-separated list of chord specs (e.g. `"C-q,F12"`)
/// into the list of `(code, modifiers)` pairs the exit check
/// compares against. Invalid pieces are dropped with a warning so a
/// typo in one entry doesn't disable the whole list; an entirely
/// unparseable string falls back to the default chord set so the
/// user is never trapped in live mode without a working exit.
pub(super) fn parse_chord_list(spec: &str) -> Vec<(KeyCode, KeyModifiers)> {
    let mut out = Vec::new();
    for piece in spec.split(',') {
        let trimmed = piece.trim();
        if trimmed.is_empty() {
            continue;
        }
        match parse_chord(trimmed) {
            Some(chord) => out.push(chord),
            None => tracing::warn!(
                "live-send: ignoring unparseable exit chord '{}' in '{}'",
                trimmed,
                spec
            ),
        }
    }
    if out.is_empty() && spec != DEFAULT_EXIT_CHORD {
        tracing::warn!(
            "live-send: exit chord '{}' parsed to nothing; falling back to default '{}'",
            spec,
            DEFAULT_EXIT_CHORD
        );
        return parse_chord_list(DEFAULT_EXIT_CHORD);
    }
    out
}

/// True when `event` matches any chord in the configured list.
pub(super) fn chord_list_matches(chords: &[(KeyCode, KeyModifiers)], event: KeyEvent) -> bool {
    chords.iter().any(|c| chord_matches(*c, event))
}

/// Render the configured chord list as a banner-friendly string,
/// e.g. `"Ctrl+Q"` or `"Ctrl+Q / F12"`. Used for the status-bar live
/// banner so the user sees every chord they can press to exit.
pub(super) fn display_chord_list(chords: &[(KeyCode, KeyModifiers)]) -> String {
    chords
        .iter()
        .map(|c| display_chord(*c))
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Render a parsed chord back as a human-readable string for the
/// banner: e.g. `(KeyCode::Char('q'), CONTROL)` → "Ctrl+Q". Uses
/// uppercase for letters so the banner reads like the rest of the
/// chord hints in the TUI (Ctrl+T, Ctrl+K, etc.).
pub(super) fn display_chord(spec: (KeyCode, KeyModifiers)) -> String {
    let (code, mods) = spec;
    let mut out = String::new();
    if mods.contains(KeyModifiers::CONTROL) {
        out.push_str("Ctrl+");
    }
    if mods.contains(KeyModifiers::ALT) {
        out.push_str("Alt+");
    }
    if mods.contains(KeyModifiers::SHIFT) {
        out.push_str("Shift+");
    }
    match code {
        KeyCode::Char(c) => out.push(c.to_ascii_uppercase()),
        KeyCode::Esc => out.push_str("Esc"),
        KeyCode::Tab => out.push_str("Tab"),
        KeyCode::BackTab => out.push_str("Shift+Tab"),
        KeyCode::Enter => out.push_str("Enter"),
        KeyCode::Backspace => out.push_str("Backspace"),
        KeyCode::Delete => out.push_str("Delete"),
        KeyCode::Insert => out.push_str("Insert"),
        KeyCode::Home => out.push_str("Home"),
        KeyCode::End => out.push_str("End"),
        KeyCode::PageUp => out.push_str("PageUp"),
        KeyCode::PageDown => out.push_str("PageDown"),
        KeyCode::Up => out.push_str("Up"),
        KeyCode::Down => out.push_str("Down"),
        KeyCode::Left => out.push_str("Left"),
        KeyCode::Right => out.push_str("Right"),
        KeyCode::F(n) => out.push_str(&format!("F{n}")),
        _ => out.push('?'),
    }
    out
}

/// Lives on `HomeView::live_send` while the mode is active. Carries
/// just enough state for the banner to render, for the exit handler
/// to confirm the right pane was targeted, and for the per-keystroke
/// liveness check to detect that the session has been deleted or
/// renamed out from under us (the stored `tmux_name` is the entry-time
/// value; if the instance's current `generate_name(id, title)` diverges
/// we auto-exit rather than silently sending into the void).
// Visibility note: `pub(in crate::tui)` rather than `pub(super)` so the
// scope matches HomeView's field (whose `pub(super)` resolves to
// `pub(in crate::tui)` from mod.rs). Anything tighter triggers
// `private_interfaces`; anything looser leaks the type to the rest of
// the crate.
#[derive(Debug, Clone)]
pub(in crate::tui) struct LiveSendState {
    pub session_id: String,
    pub title: String,
    pub tmux_name: String,
    /// Which paired pane the live-send is targeting. Captured at entry
    /// time so the drift check, exit-sizing reset, and view-mode flips
    /// during live-send don't change where keystrokes are dispatched.
    pub target: LiveSendTarget,
    /// Chord list parsed from the user's configured exit-chord
    /// setting at entry time. Captured per-entry so config edits
    /// don't change behavior mid-session.
    pub exit_chords: Vec<(KeyCode, KeyModifiers)>,
    /// Leader (prefix) chord parsed from the user's configured leader
    /// setting at entry time. `None` when the user cleared the setting
    /// (leader disabled, every key passes through). When `Some`, the
    /// first press arms the live-send command menu; the next key picks
    /// a command (or a second leader press passes a literal leader to
    /// the agent). Snapshotted per-entry for the same reason as
    /// `exit_chords`.
    pub leader: Option<(KeyCode, KeyModifiers)>,
}

/// Which paired tmux pane a live-send dispatch targets. The agent
/// pane is the historical default; terminal panes (host and
/// container) reuse the same live-send dispatch machinery but route
/// to the paired terminal's tmux session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(in crate::tui) enum LiveSendTarget {
    /// The agent's tmux pane (default, pre-existing behavior).
    #[default]
    Agent,
    /// The paired host-shell terminal pane.
    Terminal,
    /// The paired container-shell terminal pane (sandboxed sessions
    /// in container terminal mode).
    ContainerTerminal,
}

/// Format a display label for a `(title, target)` pair so the compose
/// dialog header and the live-mode status banner stay in lockstep.
/// Agent keeps the bare title (historical look); terminal variants
/// get a short parenthetical so the user sees which pane the
/// keystrokes will land on without having to read the preview chrome.
pub(in crate::tui) fn format_target_label(title: &str, target: LiveSendTarget) -> String {
    match target {
        LiveSendTarget::Agent => title.to_string(),
        LiveSendTarget::Terminal => format!("{title} (terminal)"),
        LiveSendTarget::ContainerTerminal => format!("{title} (container)"),
    }
}

/// One coalesced unit of work the worker hands to tmux. `Literal` runs
/// fold together; named keys, hex-byte runs, and resizes break the run
/// because their order vs. surrounding text matters (an Up arrow between
/// "ab" and "cd" must arrive between, not after; a resize that lands
/// before keystrokes makes the agent render those keystrokes at the new
/// geometry). Consecutive `HexBytes` payloads do merge with each other
/// so a multi-blank-line paste collapses into one `send-keys -H` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TmuxAction {
    Literal(String),
    Named(String),
    HexBytes(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// Fold a batch of `WorkerMsg`s into the smallest sequence of
/// `TmuxAction`s that preserves the original ordering. Consecutive
/// `Send(Literal)` values merge into one `send-keys -l` payload, and
/// consecutive `Send(HexBytes)` values merge into one `send-keys -H`
/// argument list; a `Send(Named)`, `Send(HexBytes)`, or `Resize`
/// flushes the current literal run. Pure function so tests can verify
/// ordering without spawning a worker thread.
pub(super) fn coalesce(batch: Vec<WorkerMsg>) -> Vec<TmuxAction> {
    let mut out: Vec<TmuxAction> = Vec::new();
    let mut run = String::new();
    let flush = |out: &mut Vec<TmuxAction>, run: &mut String| {
        if !run.is_empty() {
            out.push(TmuxAction::Literal(std::mem::take(run)));
        }
    };
    for msg in batch {
        match msg {
            WorkerMsg::Send(TmuxKey::Literal(s)) => run.push_str(&s),
            WorkerMsg::Send(TmuxKey::Named(name)) => {
                flush(&mut out, &mut run);
                out.push(TmuxAction::Named(name));
            }
            WorkerMsg::Send(TmuxKey::HexBytes(bytes)) => {
                flush(&mut out, &mut run);
                match out.last_mut() {
                    Some(TmuxAction::HexBytes(prev)) => prev.extend_from_slice(&bytes),
                    _ => out.push(TmuxAction::HexBytes(bytes)),
                }
            }
            WorkerMsg::Resize { cols, rows } => {
                flush(&mut out, &mut run);
                out.push(TmuxAction::Resize { cols, rows });
            }
        }
    }
    flush(&mut out, &mut run);
    out
}

/// One unit of work the worker can be asked to perform. Resizes don't
/// coalesce with keys because they're sticky pane-level changes; a
/// burst of keystrokes that brackets a resize must arrive on either
/// side of the geometry change, not be reordered after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum WorkerMsg {
    Send(TmuxKey),
    Resize { cols: u16, rows: u16 },
}

/// Background dispatcher: drains a channel of `WorkerMsg`s and runs
/// each via a one-shot `tmux send-keys` / `resize-window` subprocess
/// after coalescing with `coalesce`. Spawned by `prepare_live_send` and
/// dropped when the user exits live mode; dropping closes the channel,
/// which makes the worker thread's `recv` return `Err` and exit on the
/// next iteration. We deliberately do not `join` because the worker is
/// idempotent and harmless if it survives a brief moment past the UI
/// thread that owned it (e.g., the user toggles live mode rapidly).
///
/// Previously (#1485) this dispatched through a long-lived
/// `tmux -C attach-session` connection to avoid one fork per
/// keystroke. The connection turned out to be unstable on at least
/// some macOS tmux 3.x builds (it would EOF within milliseconds of
/// spawn), and the resulting fork-fallback path was hit ~100% of the
/// time on those setups while still paying the spawn cost upfront.
/// Ripping out control-mode entirely keeps the dispatch path simple
/// (one fork per coalesced batch) and consistent across setups; the
/// per-keystroke fork cost is bounded by user typing speed and is
/// invisible on a laptop. Mobile/mosh users pay a few extra ms per
/// keypress, which we accept as the cost of reliability.
pub(in crate::tui) struct LiveSendWorker {
    tx: Sender<WorkerMsg>,
}

impl LiveSendWorker {
    pub(super) fn spawn(tmux_name: String, capture_wake: Option<LiveCaptureWake>) -> Self {
        let (tx, rx) = channel::<WorkerMsg>();
        std::thread::spawn(move || {
            // Block until the first message, then drain anything else
            // that piled up during the previous flush. The drain plus
            // `coalesce` collapses paste-bursts and held-key autorepeat
            // into one fork per literal run, so typing a long sentence
            // costs one `tmux send-keys -l` invocation, not one per
            // character.
            while let Ok(first) = rx.recv() {
                let mut batch = vec![first];
                while let Ok(msg) = rx.try_recv() {
                    batch.push(msg);
                }
                dispatch_batch(&tmux_name, batch);
                if let Some(wake) = &capture_wake {
                    wake.wake();
                }
            }
        });
        Self { tx }
    }

    /// Enqueue a translated key for dispatch. Returns immediately; the
    /// `tmux send-keys` fork happens on the worker thread, so the UI
    /// never blocks on tmux latency.
    pub(super) fn send(&self, key: TmuxKey) {
        // Channel send only fails if the worker thread panicked. Drop
        // silently rather than spam logs: the user's next exit attempt
        // (Ctrl+q) will clear the dead worker and we'll spawn a fresh
        // one on the next live-send entry.
        let _ = self.tx.send(WorkerMsg::Send(key));
    }

    /// Enqueue a tmux pane resize. The geometry change is serialized
    /// with surrounding keystrokes so that keys typed before the
    /// resize arrive in the old size and keys after arrive in the new
    /// size (matters when an agent uses cursor-position escapes).
    pub(super) fn resize(&self, cols: u16, rows: u16) {
        let _ = self.tx.send(WorkerMsg::Resize { cols, rows });
    }
}

/// How often the off-thread capture worker forks `tmux capture-pane`.
/// It free-runs at roughly this cadence (a fork is ~3-13ms on macOS, so
/// the real cycle is interval + fork time), keeping the preview fresh on
/// a background thread so the render loop never forks capture-pane
/// itself. Sits just under the 33ms render ticker so a frame almost
/// always finds content that postdates the last keystroke, while keeping
/// the steady-state fork rate close to the old render-driven ~30/s (a
/// tighter value buys little perceived freshness for a lot more idle
/// forks, since the render only paints every ~33ms anyway).
const LIVE_CAPTURE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

#[derive(Clone)]
pub(in crate::tui) struct LiveCaptureWake {
    tx: Sender<()>,
}

impl LiveCaptureWake {
    fn wake(&self) {
        let _ = self.tx.send(());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) enum EmptyCapturePolicy {
    PreserveLastGood,
    ForwardEmpty,
}

/// Off-thread preview capture for live-send. Spawned alongside
/// [`LiveSendWorker`] for the targeted tmux pane, it forks
/// `tmux capture-pane` on its own thread and publishes fresh pane
/// content into a single-slot mailbox the render loop drains. The render
/// loop applies the latest content without forking, which moves the
/// per-frame capture cost off the hot path. Dropping the worker flips
/// `stop` so the thread exits after its current cycle; like
/// `LiveSendWorker` we don't join.
pub(in crate::tui) struct LiveCaptureWorker {
    /// Lines the render loop wants captured (height + scrollback + buffer).
    /// `0` means "not set yet"; the worker skips capturing until the first
    /// render publishes a real value. `capture_lines_for` never yields 0.
    capture_lines: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Single-slot mailbox holding the newest capture not yet consumed by
    /// the render loop. A new capture overwrites an unconsumed one (the
    /// render only ever wants the latest), so this can't grow unbounded if
    /// the render thread stalls.
    latest: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    wake_tx: Sender<()>,
}

impl Drop for LiveCaptureWorker {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = self.wake_tx.send(());
    }
}

impl LiveCaptureWorker {
    pub(in crate::tui) fn spawn(tmux_name: String, empty_policy: EmptyCapturePolicy) -> Self {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let capture_lines = Arc::new(AtomicUsize::new(0));
        let latest: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (wake_tx, wake_rx) = channel::<()>();
        let lines_cell = capture_lines.clone();
        let slot = latest.clone();
        let stop_flag = stop.clone();
        std::thread::spawn(move || {
            let session = crate::tmux::Session::from_name(&tmux_name);
            let mut last_captured: Option<String> = None;
            while !stop_flag.load(Ordering::Relaxed) {
                let lines = lines_cell.load(Ordering::Relaxed);
                if lines > 0 {
                    let capture = match session.capture_pane(lines) {
                        Ok(content) => Some(content),
                        Err(_) if empty_policy == EmptyCapturePolicy::ForwardEmpty => {
                            Some(String::new())
                        }
                        Err(_) => None,
                    };
                    if let Some(content) = capture {
                        let allow_empty = empty_policy == EmptyCapturePolicy::ForwardEmpty;
                        let changed = last_captured.as_deref() != Some(content.as_str());
                        if (allow_empty || !content.is_empty()) && changed {
                            if let Ok(mut guard) = slot.lock() {
                                *guard = Some(content.clone());
                            }
                            last_captured = Some(content);
                        }
                    }
                }
                match wake_rx.recv_timeout(LIVE_CAPTURE_INTERVAL) {
                    Ok(_) => while wake_rx.try_recv().is_ok() {},
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });
        Self {
            capture_lines,
            latest,
            stop,
            wake_tx,
        }
    }

    pub(in crate::tui) fn waker(&self) -> LiveCaptureWake {
        LiveCaptureWake {
            tx: self.wake_tx.clone(),
        }
    }

    /// Publish the line count the worker should capture. Cheap (one atomic
    /// store); called each render so resizes and history scroll reach the
    /// worker promptly.
    pub(in crate::tui) fn set_capture_lines(&self, lines: usize) {
        self.capture_lines
            .store(lines, std::sync::atomic::Ordering::Relaxed);
    }

    /// Take the newest capture the worker has produced since the last
    /// call, if any. Returns `None` when nothing new has arrived (the
    /// render loop then keeps the current preview).
    pub(in crate::tui) fn take_latest(&self) -> Option<String> {
        self.latest.lock().ok().and_then(|mut guard| guard.take())
    }
}

/// Walk one drained batch and execute it as one-shot `tmux` subprocesses.
/// `coalesce` merges literal-key runs into a single `send-keys -l` call;
/// named keys and resizes dispatch individually. Tests verify the
/// coalescing ordering via `coalesce` directly without needing a real
/// session.
fn dispatch_batch(tmux_name: &str, batch: Vec<WorkerMsg>) {
    for action in coalesce(batch) {
        if let Err(err) = dispatch_via_fork(tmux_name, &action) {
            tracing::warn!(
                target: "tui.live_send",
                error = %err,
                action = ?action,
                "live-send fork dispatch failed; keystroke dropped",
            );
        }
    }
}

/// Execute one `TmuxAction` as a one-shot `tmux` subprocess. Module-
/// level fn (rather than a method on the worker) so it stays callable
/// from the spawned thread without holding a worker reference.
fn dispatch_via_fork(tmux_name: &str, action: &TmuxAction) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    let target = format!("{}:^.0", tmux_name);
    let mut cmd = Command::new("tmux");
    cmd.stderr(Stdio::null());
    match action {
        TmuxAction::Literal(s) => {
            // `-l --` mirrors `send_literal_no_enter`: literal-mode
            // send, followed by the end-of-options marker so a payload
            // starting with `-` isn't reparsed as a flag.
            cmd.args(["send-keys", "-t", &target, "-l", "--", s.as_str()]);
        }
        TmuxAction::Named(name) => {
            cmd.args(["send-keys", "-t", &target, name.as_str()]);
        }
        TmuxAction::HexBytes(bytes) => {
            // `-H` sends each subsequent arg as the hex byte value of an
            // ASCII character. We use this for control bytes (CR, TAB,
            // ESC) and the bracketed-paste markers, none of which can
            // ride a `-l` payload safely. One arg per byte means a large
            // multi-line paste would overflow the OS `execve` argv limit
            // (macOS ARG_MAX is 256 KiB) and fail with E2BIG, silently
            // dropping the whole paste, so split it across bounded forks.
            return dispatch_hex_bytes(&target, bytes);
        }
        TmuxAction::Resize { cols, rows } => {
            cmd.args([
                "resize-window",
                "-t",
                tmux_name,
                "-x",
                &cols.to_string(),
                "-y",
                &rows.to_string(),
            ]);
        }
    }
    let status = cmd
        .status()
        .map_err(|e| anyhow::anyhow!("spawn live-send tmux subprocess: {}", e))?;
    if !status.success() {
        anyhow::bail!("live-send tmux subprocess exited non-zero for {:?}", action);
    }
    Ok(())
}

/// Upper bound on the number of bytes encoded into a single
/// `tmux send-keys -H` fork. Each byte becomes one ~2-char hex argument
/// plus its argv pointer (~11 bytes of kernel arg space), and macOS caps
/// `execve` argv+envp at `ARG_MAX` = 256 KiB, so a per-byte encoding of
/// a large paste overflows around 20 KB and fails wholesale with E2BIG.
/// 4 KiB per fork keeps every argv under ~45 KiB, comfortably below the
/// limit on every platform while keeping the fork count low.
const MAX_HEX_BYTES_PER_SEND: usize = 4096;

/// Dispatch a raw byte payload as one or more `tmux send-keys -H` forks,
/// each bounded by [`MAX_HEX_BYTES_PER_SEND`]. tmux injects the bytes
/// into the pane in order, so splitting a bracketed paste across forks is
/// transparent to the agent: it still reassembles one contiguous paste
/// from the `\e[200~` ... `\e[201~` byte stream.
fn dispatch_hex_bytes(target: &str, bytes: &[u8]) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    for batch in hex_send_batches(bytes) {
        let mut cmd = Command::new("tmux");
        cmd.stderr(Stdio::null());
        cmd.args(["send-keys", "-t", target, "-H"]);
        cmd.args(&batch);
        let status = cmd
            .status()
            .map_err(|e| anyhow::anyhow!("spawn live-send tmux subprocess: {}", e))?;
        if !status.success() {
            anyhow::bail!(
                "live-send tmux subprocess exited non-zero for {} bytes",
                bytes.len()
            );
        }
    }
    Ok(())
}

/// Split a byte payload into the hex-argument batches passed to
/// successive `tmux send-keys -H` forks. Pure so the chunking bound and
/// byte-order preservation are unit-testable without a tmux session.
fn hex_send_batches(bytes: &[u8]) -> Vec<Vec<String>> {
    bytes
        .chunks(MAX_HEX_BYTES_PER_SEND)
        .map(|chunk| chunk.iter().map(|b| format!("{:02x}", b)).collect())
        .collect()
}

/// What the translator says to do with one incoming key event.
///
/// Note: the exit-chord check lives in `handle_live_send_key` (it
/// consults the user's configured chord list, which translate has no
/// access to). translate is purely the key-to-tmux mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LiveDispatch {
    /// Forward the keystroke to tmux in the requested form.
    Send(TmuxKey),
    /// Key has no meaningful tmux mapping (Null, CapsLock, media keys, …).
    /// Caller should drop it silently rather than echo it elsewhere.
    Ignore,
}

/// How the translator wants the keystroke delivered. `Literal` payloads
/// go through `tmux send-keys -l --`, named keys through `tmux send-keys`,
/// and `HexBytes` through `tmux send-keys -H <byte> <byte> ...` for raw
/// bytes that can't ride a literal payload (control bytes like ESC, CR,
/// TAB, and the bracketed-paste markers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TmuxKey {
    Literal(String),
    Named(String),
    HexBytes(Vec<u8>),
}

/// Map one crossterm `KeyEvent` onto a `LiveDispatch`.
///
/// Exit-chord detection is NOT done here. `handle_live_send_key`
/// checks the user's configured chord list before calling translate,
/// so this function is pure key→tmux mapping.
///
/// Conventions:
/// - Plain printable chars (`KeyCode::Char` with no Ctrl/Alt) go literal
///   so the user's case and punctuation are preserved verbatim. The shift
///   modifier is implicit in the char itself, so we don't add `S-`.
/// - Ctrl/Alt + a char folds the char to lowercase and emits a tmux name
///   like `C-a`, `M-x`, `C-M-x`. Lowercase because tmux's chord names
///   are case-insensitive for letters and `C-a` is the conventional form.
///   Shift is omitted here too (case already encodes it for letters).
/// - Named keys (arrows, F-keys, etc.) include `S-` when Shift is held
///   so editors inside the pane see `S-Up` for shift-arrow text
///   selection. `BackTab` is the lone exception: the keycode already
///   means Shift+Tab, so we emit `BTab` rather than `S-BTab`.
pub fn translate(key: KeyEvent) -> LiveDispatch {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // Char path: tmux chord names are case-insensitive for letters and
    // the case in `Char(c)` already carries Shift, so we drop `S-` here
    // to avoid double-encoding.
    if let KeyCode::Char(c) = key.code {
        if ctrl || alt {
            let p = mod_prefix(ctrl, alt, false);
            return LiveDispatch::Send(TmuxKey::Named(format!("{p}{}", c.to_ascii_lowercase())));
        }
        return LiveDispatch::Send(TmuxKey::Literal(c.to_string()));
    }

    // Named-key path: Shift IS meaningful (S-Up vs Up for editor text
    // selection). BackTab is shift+Tab semantically by its own keycode,
    // so it gets the no-shift prefix.
    let name = match key.code {
        KeyCode::Up => "Up",
        KeyCode::Down => "Down",
        KeyCode::Left => "Left",
        KeyCode::Right => "Right",
        KeyCode::Enter => "Enter",
        KeyCode::Esc => "Escape",
        KeyCode::Tab => "Tab",
        KeyCode::BackTab => {
            let p = mod_prefix(ctrl, alt, false);
            return LiveDispatch::Send(TmuxKey::Named(format!("{p}BTab")));
        }
        KeyCode::Backspace => "BSpace",
        KeyCode::Delete => "DC",
        KeyCode::Insert => "IC",
        KeyCode::Home => "Home",
        KeyCode::End => "End",
        KeyCode::PageUp => "PPage",
        KeyCode::PageDown => "NPage",
        KeyCode::F(n) => {
            let p = mod_prefix(ctrl, alt, shift);
            return LiveDispatch::Send(TmuxKey::Named(format!("{p}F{n}")));
        }
        _ => return LiveDispatch::Ignore,
    };
    let p = mod_prefix(ctrl, alt, shift);
    LiveDispatch::Send(TmuxKey::Named(format!("{p}{name}")))
}

/// Build a tmux chord prefix (e.g. `"C-S-"`, `"M-"`, `""`).
fn mod_prefix(ctrl: bool, alt: bool, shift: bool) -> String {
    let mut p = String::new();
    if ctrl {
        p.push_str("C-");
    }
    if alt {
        p.push_str("M-");
    }
    if shift {
        p.push_str("S-");
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn k_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn assert_literal(d: LiveDispatch, expected: &str) {
        match d {
            LiveDispatch::Send(TmuxKey::Literal(s)) => assert_eq!(s, expected),
            other => panic!("expected Literal({expected}), got {other:?}"),
        }
    }
    fn assert_named(d: LiveDispatch, expected: &str) {
        match d {
            LiveDispatch::Send(TmuxKey::Named(s)) => assert_eq!(s, expected),
            other => panic!("expected Named({expected}), got {other:?}"),
        }
    }

    // Exit-chord detection moved out of translate() into
    // handle_live_send_key. Translate now never emits Exit; the
    // chord-list tests below cover the configurable exit path.

    #[test]
    fn translate_never_emits_exit_for_ctrl_q() {
        // translate is pure key→tmux. Ctrl+q now passes through; the
        // exit decision belongs to the chord-list matcher.
        assert_named(
            translate(k_mod(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            "C-q",
        );
    }

    #[test]
    fn parse_chord_basics() {
        assert_eq!(
            parse_chord("C-q"),
            Some((KeyCode::Char('q'), KeyModifiers::CONTROL))
        );
        // Uppercase letter folds to lowercase under Ctrl (tmux
        // convention: C-a and C-A are the same chord).
        assert_eq!(
            parse_chord("C-Q"),
            Some((KeyCode::Char('q'), KeyModifiers::CONTROL))
        );
        // Punctuation as the key.
        assert_eq!(
            parse_chord("C-]"),
            Some((KeyCode::Char(']'), KeyModifiers::CONTROL))
        );
        // Long modifier names.
        assert_eq!(
            parse_chord("Ctrl+Alt+x"),
            Some((
                KeyCode::Char('x'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            ))
        );
        // Named keys.
        assert_eq!(
            parse_chord("F12"),
            Some((KeyCode::F(12), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_chord("S-Up"),
            Some((KeyCode::Up, KeyModifiers::SHIFT))
        );
        assert_eq!(
            parse_chord("Escape"),
            Some((KeyCode::Esc, KeyModifiers::NONE))
        );
        assert_eq!(
            parse_chord("PageUp"),
            Some((KeyCode::PageUp, KeyModifiers::NONE))
        );
    }

    #[test]
    fn parse_chord_rejects_garbage() {
        assert_eq!(parse_chord(""), None);
        assert_eq!(parse_chord("X-q"), None); // unknown modifier
        assert_eq!(parse_chord("C-qq"), None); // multi-char key without F-prefix
        assert_eq!(parse_chord("C-"), None); // missing key
    }

    #[test]
    fn chord_matches_handles_ctrl_case_folding() {
        let spec = parse_chord("C-q").unwrap();
        // Crossterm may deliver Ctrl+Q as either Char('q') or
        // Char('Q')+SHIFT depending on terminal; the match should
        // recognize the lowercase form but NOT the shift form
        // (shift means the user wants to send Ctrl+Shift+q to the
        // agent, not exit).
        assert!(chord_matches(
            spec,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)
        ));
        assert!(!chord_matches(
            spec,
            KeyEvent::new(
                KeyCode::Char('Q'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            )
        ));
        assert!(!chord_matches(
            spec,
            KeyEvent::new(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )
        ));
    }

    #[test]
    fn parse_chord_list_filters_invalid_pieces() {
        let chords = parse_chord_list("C-q, garbage, C-]");
        assert_eq!(
            chords,
            vec![
                (KeyCode::Char('q'), KeyModifiers::CONTROL),
                (KeyCode::Char(']'), KeyModifiers::CONTROL),
            ]
        );
    }

    #[test]
    fn parse_chord_list_falls_back_to_default_on_all_invalid() {
        // Misconfigured chord lists shouldn't trap the user in live
        // mode with no exit; we drop back to the default set.
        let chords = parse_chord_list("not-a-chord, also-bad");
        let defaults = parse_chord_list(DEFAULT_EXIT_CHORD);
        assert_eq!(chords, defaults);
        assert!(!chords.is_empty());
    }

    #[test]
    fn default_chord_set_is_only_ctrl_q() {
        let chords = parse_chord_list(DEFAULT_EXIT_CHORD);
        assert_eq!(chords, vec![(KeyCode::Char('q'), KeyModifiers::CONTROL)]);
        // `Ctrl+]` (1.9.0 default) and `Ctrl+\` (in-development try)
        // were both pulled because each failed on at least one common
        // macOS terminal/keyboard combination. Users who want a two-
        // hand exit configure one explicitly.
        assert!(!chords.contains(&(KeyCode::Char(']'), KeyModifiers::CONTROL)));
        assert!(!chords.contains(&(KeyCode::Char('\\'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn default_leader_is_ctrl_b() {
        // Ctrl+B matches the tmux/herdr leader. Kept in sync with
        // session::config::default_live_send_leader().
        assert_eq!(
            parse_chord(DEFAULT_LEADER),
            Some((KeyCode::Char('b'), KeyModifiers::CONTROL))
        );
    }

    #[test]
    fn leader_matches_only_its_exact_chord() {
        let leader = parse_chord(DEFAULT_LEADER).unwrap();
        // The armed leader: Ctrl+B fires it again (passthrough path).
        assert!(chord_matches(
            leader,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)
        ));
        // Bare `b` is a menu command, not a second leader press, so it
        // must NOT match the leader chord.
        assert!(!chord_matches(
            leader,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)
        ));
    }

    #[test]
    fn chord_list_matches_any() {
        let chords = parse_chord_list("C-q, C-]");
        assert!(chord_list_matches(
            &chords,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)
        ));
        assert!(chord_list_matches(
            &chords,
            KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL)
        ));
        assert!(!chord_list_matches(
            &chords,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL)
        ));
    }

    #[test]
    fn display_chord_list_joins_with_slash() {
        let chords = parse_chord_list("C-q, C-]");
        assert_eq!(display_chord_list(&chords), "Ctrl+Q / Ctrl+]");
    }

    #[test]
    fn display_chord_single() {
        let chord = parse_chord("C-]").unwrap();
        assert_eq!(display_chord(chord), "Ctrl+]");
        let chord = parse_chord("F12").unwrap();
        assert_eq!(display_chord(chord), "F12");
        let chord = parse_chord("Ctrl+Alt+Shift+x").unwrap();
        assert_eq!(display_chord(chord), "Ctrl+Alt+Shift+X");
    }

    #[test]
    fn plain_letters_go_literal_preserving_case() {
        assert_literal(translate(k(KeyCode::Char('a'))), "a");
        assert_literal(translate(k(KeyCode::Char('Z'))), "Z");
        assert_literal(translate(k(KeyCode::Char('!'))), "!");
        assert_literal(translate(k(KeyCode::Char(' '))), " ");
    }

    #[test]
    fn ctrl_letter_folds_lowercase_to_named() {
        assert_named(
            translate(k_mod(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            "C-c",
        );
        assert_named(
            translate(k_mod(KeyCode::Char('A'), KeyModifiers::CONTROL)),
            "C-a",
        );
    }

    #[test]
    fn alt_letter_folds_to_named() {
        assert_named(
            translate(k_mod(KeyCode::Char('x'), KeyModifiers::ALT)),
            "M-x",
        );
    }

    #[test]
    fn ctrl_alt_combo() {
        assert_named(
            translate(k_mod(
                KeyCode::Char('q'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            )),
            "C-M-q",
        );
    }

    #[test]
    fn arrow_keys() {
        assert_named(translate(k(KeyCode::Up)), "Up");
        assert_named(translate(k(KeyCode::Down)), "Down");
        assert_named(translate(k(KeyCode::Left)), "Left");
        assert_named(translate(k(KeyCode::Right)), "Right");
    }

    #[test]
    fn ctrl_arrow_chord() {
        assert_named(translate(k_mod(KeyCode::Up, KeyModifiers::CONTROL)), "C-Up");
    }

    #[test]
    fn shift_arrow_chord_uses_s_prefix() {
        // Editors inside the pane rely on `S-Up` / `S-Down` etc. for
        // text selection. Without the S- prefix Shift+arrow looks the
        // same as plain arrow and the editor never sees the modifier.
        assert_named(translate(k_mod(KeyCode::Up, KeyModifiers::SHIFT)), "S-Up");
        assert_named(
            translate(k_mod(KeyCode::Home, KeyModifiers::SHIFT)),
            "S-Home",
        );
        assert_named(translate(k_mod(KeyCode::End, KeyModifiers::SHIFT)), "S-End");
    }

    #[test]
    fn ctrl_shift_arrow_combines_prefixes() {
        // Shift+Ctrl+Right is "extend selection by word" in many editors.
        assert_named(
            translate(k_mod(
                KeyCode::Right,
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            "C-S-Right",
        );
    }

    #[test]
    fn shift_letter_stays_literal_uppercase() {
        // The Char path drops Shift from the prefix because the case
        // already carries it. Pressing Shift+A sends literal "A", not
        // "S-a" or "S-A".
        assert_literal(
            translate(k_mod(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            "A",
        );
    }

    #[test]
    fn back_tab_stays_btab_even_with_shift_modifier() {
        // BackTab IS Shift+Tab by keycode. Some terminals also set the
        // SHIFT modifier on top; we must NOT emit "S-BTab" (tmux would
        // reject it) just because both signals arrived.
        assert_named(
            translate(k_mod(KeyCode::BackTab, KeyModifiers::SHIFT)),
            "BTab",
        );
    }

    #[test]
    fn navigation_named_keys() {
        assert_named(translate(k(KeyCode::Esc)), "Escape");
        assert_named(translate(k(KeyCode::Enter)), "Enter");
        assert_named(translate(k(KeyCode::Tab)), "Tab");
        assert_named(translate(k(KeyCode::BackTab)), "BTab");
        assert_named(translate(k(KeyCode::Backspace)), "BSpace");
        assert_named(translate(k(KeyCode::Delete)), "DC");
        assert_named(translate(k(KeyCode::Insert)), "IC");
        assert_named(translate(k(KeyCode::Home)), "Home");
        assert_named(translate(k(KeyCode::End)), "End");
        assert_named(translate(k(KeyCode::PageUp)), "PPage");
        assert_named(translate(k(KeyCode::PageDown)), "NPage");
    }

    #[test]
    fn function_keys() {
        assert_named(translate(k(KeyCode::F(1))), "F1");
        assert_named(translate(k(KeyCode::F(12))), "F12");
        assert_named(
            translate(k_mod(KeyCode::F(5), KeyModifiers::CONTROL)),
            "C-F5",
        );
    }

    fn snd_lit(s: &str) -> WorkerMsg {
        WorkerMsg::Send(TmuxKey::Literal(s.into()))
    }
    fn snd_named(s: &str) -> WorkerMsg {
        WorkerMsg::Send(TmuxKey::Named(s.into()))
    }
    fn snd_hex(bytes: &[u8]) -> WorkerMsg {
        WorkerMsg::Send(TmuxKey::HexBytes(bytes.to_vec()))
    }

    #[test]
    fn coalesce_empty_batch_is_empty() {
        assert_eq!(coalesce(vec![]), vec![]);
    }

    #[test]
    fn coalesce_single_literal_passes_through() {
        assert_eq!(
            coalesce(vec![snd_lit("a")]),
            vec![TmuxAction::Literal("a".into())]
        );
    }

    #[test]
    fn coalesce_single_named_passes_through() {
        assert_eq!(
            coalesce(vec![snd_named("Escape")]),
            vec![TmuxAction::Named("Escape".into())]
        );
    }

    #[test]
    fn coalesce_run_of_literals_merges_into_one_call() {
        // The whole point of coalescing: typing "hello" should be a
        // single tmux send-keys call, not five.
        let out = coalesce(vec![
            snd_lit("h"),
            snd_lit("e"),
            snd_lit("l"),
            snd_lit("l"),
            snd_lit("o"),
        ]);
        assert_eq!(out, vec![TmuxAction::Literal("hello".into())]);
    }

    #[test]
    fn coalesce_named_breaks_the_run() {
        // An Up arrow in the middle of typing must arrive in order,
        // not after the surrounding text. Coalescing splits the run at
        // the named key.
        let out = coalesce(vec![
            snd_lit("a"),
            snd_lit("b"),
            snd_named("Up"),
            snd_lit("c"),
            snd_lit("d"),
        ]);
        assert_eq!(
            out,
            vec![
                TmuxAction::Literal("ab".into()),
                TmuxAction::Named("Up".into()),
                TmuxAction::Literal("cd".into()),
            ]
        );
    }

    #[test]
    fn coalesce_back_to_back_named_keys() {
        // Two named keys in a row (e.g., Up Up) stay as two separate
        // dispatches; tmux send-keys won't accept them as one literal.
        let out = coalesce(vec![snd_named("Up"), snd_named("Up")]);
        assert_eq!(
            out,
            vec![
                TmuxAction::Named("Up".into()),
                TmuxAction::Named("Up".into()),
            ]
        );
    }

    #[test]
    fn coalesce_trailing_literal_is_flushed() {
        // Regression guard for the obvious off-by-one: the final
        // unflushed literal run must escape the loop.
        let out = coalesce(vec![snd_named("Tab"), snd_lit("x"), snd_lit("y")]);
        assert_eq!(
            out,
            vec![
                TmuxAction::Named("Tab".into()),
                TmuxAction::Literal("xy".into()),
            ]
        );
    }

    #[test]
    fn coalesce_hex_bytes_breaks_literal_run() {
        // A HexBytes payload (bracketed-paste marker, raw CR, etc.)
        // must dispatch in order, not after surrounding literals.
        let out = coalesce(vec![snd_lit("a"), snd_hex(&[0x0d]), snd_lit("b")]);
        assert_eq!(
            out,
            vec![
                TmuxAction::Literal("a".into()),
                TmuxAction::HexBytes(vec![0x0d]),
                TmuxAction::Literal("b".into()),
            ]
        );
    }

    #[test]
    fn coalesce_back_to_back_hex_bytes_merge() {
        // Consecutive HexBytes payloads (e.g. a paste with a blank line
        // produces two raw-CR sends in a row) collapse into one
        // `send-keys -H` invocation. Named keys can't merge because
        // each is a separate key argument; raw bytes have no such
        // constraint.
        let out = coalesce(vec![snd_hex(&[0x0d]), snd_hex(&[0x0d])]);
        assert_eq!(out, vec![TmuxAction::HexBytes(vec![0x0d, 0x0d])]);
    }

    #[test]
    fn coalesce_preserves_order_when_hex_bytes_and_literals_interleave() {
        // A future caller could send `HexBytes` and `Literal` payloads
        // back to back (e.g. a typed-then-pasted burst the worker
        // drained in one tick). Coalesce must keep wire ordering
        // intact: each `Literal` flushes the run, and only adjacent
        // `HexBytes` pairs merge.
        let start = vec![0x1b, b'[', b'2', b'0', b'0', b'~'];
        let end = vec![0x1b, b'[', b'2', b'0', b'1', b'~'];
        let out = coalesce(vec![
            snd_hex(&start),
            snd_lit("a"),
            snd_hex(&[0x0d]),
            snd_lit("b"),
            snd_hex(&end),
        ]);
        assert_eq!(
            out,
            vec![
                TmuxAction::HexBytes(start),
                TmuxAction::Literal("a".into()),
                TmuxAction::HexBytes(vec![0x0d]),
                TmuxAction::Literal("b".into()),
                TmuxAction::HexBytes(end),
            ]
        );
    }

    #[test]
    fn coalesce_resize_breaks_literal_run() {
        // A pane resize sandwiched between keystrokes must dispatch in
        // order so the agent renders the trailing keys at the new
        // geometry (relevant for any agent using cursor-position
        // escapes or column-aware wrapping).
        let out = coalesce(vec![
            snd_lit("a"),
            snd_lit("b"),
            WorkerMsg::Resize {
                cols: 100,
                rows: 40,
            },
            snd_lit("c"),
        ]);
        assert_eq!(
            out,
            vec![
                TmuxAction::Literal("ab".into()),
                TmuxAction::Resize {
                    cols: 100,
                    rows: 40
                },
                TmuxAction::Literal("c".into()),
            ]
        );
    }

    #[test]
    fn unhandled_keys_are_ignored() {
        assert_eq!(translate(k(KeyCode::Null)), LiveDispatch::Ignore);
        assert_eq!(translate(k(KeyCode::CapsLock)), LiveDispatch::Ignore);
    }

    #[test]
    fn plain_q_is_literal_not_exit() {
        // Without Ctrl, `q` is just a letter the user wants to send.
        // translate doesn't decide exit any more, but this still
        // verifies the passthrough.
        assert_literal(translate(k(KeyCode::Char('q'))), "q");
        assert_literal(translate(k(KeyCode::Char('Q'))), "Q");
    }

    #[test]
    fn large_hex_paste_splits_into_bounded_send_keys_batches() {
        // Regression for the silently-dropped large paste: a ~100 KB
        // bracketed paste, encoded one hex arg per byte, overflows the
        // `execve` argv limit (macOS ARG_MAX = 256 KiB) and the whole
        // `tmux send-keys` fails with E2BIG. The fix splits the payload
        // into bounded batches; verify it splits, each batch stays under
        // the cap, and the bytes reconstruct in order.
        let payload: Vec<u8> = (0..100_000).map(|i| (i % 256) as u8).collect();
        let batches = hex_send_batches(&payload);
        assert!(
            batches.len() > 1,
            "a large paste must split across multiple forks, got {}",
            batches.len()
        );
        for batch in &batches {
            assert!(
                batch.len() <= MAX_HEX_BYTES_PER_SEND,
                "a single fork's argv must stay under the ARG_MAX bound",
            );
        }
        let roundtrip: Vec<u8> = batches
            .iter()
            .flatten()
            .map(|h| u8::from_str_radix(h, 16).unwrap())
            .collect();
        assert_eq!(
            roundtrip, payload,
            "chunking must preserve every byte and its order",
        );
    }

    #[test]
    fn small_hex_paste_stays_one_batch() {
        // A short bracketed paste fits in a single fork: no behavior
        // change for the common case.
        let payload = b"\x1b[200~hi\x1b[201~".to_vec();
        let batches = hex_send_batches(&payload);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].first().map(String::as_str), Some("1b"));
    }

    #[test]
    fn live_capture_worker_idle_until_geometry_set() {
        // With no line count published the worker must not capture at all,
        // so nothing crosses the channel. (`capture_lines == 0` guard.)
        let worker = LiveCaptureWorker::spawn(
            "aoe_test_capture_no_geometry".into(),
            EmptyCapturePolicy::PreserveLastGood,
        );
        std::thread::sleep(std::time::Duration::from_millis(60));
        assert!(
            worker.take_latest().is_none(),
            "worker should stay idle until set_capture_lines is called",
        );
    }

    #[test]
    fn live_capture_worker_skips_empty_captures() {
        // A worker pointed at a session that doesn't exist captures empty
        // strings. Forwarding those would blank the preview, defeating the
        // #1501 kill switch, so the worker must drop them. Deterministic
        // without a real tmux session: a missing pane always reads empty.
        let worker = LiveCaptureWorker::spawn(
            "aoe_test_capture_missing_session".into(),
            EmptyCapturePolicy::PreserveLastGood,
        );
        worker.set_capture_lines(40);
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert!(
            worker.take_latest().is_none(),
            "empty captures must never be forwarded",
        );
    }

    #[test]
    fn live_capture_worker_can_forward_empty_captures() {
        // Terminal previews treat empty output as meaningful: a missing or
        // blank pane should clear stale terminal text instead of preserving it.
        let worker = LiveCaptureWorker::spawn(
            "aoe_test_capture_forward_empty".into(),
            EmptyCapturePolicy::ForwardEmpty,
        );
        worker.set_capture_lines(40);
        std::thread::sleep(std::time::Duration::from_millis(80));
        assert_eq!(
            worker.take_latest(),
            Some(String::new()),
            "forward-empty policy must surface empty captures",
        );
    }
}
